use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::{cursor, execute, terminal};
use futures::StreamExt;
use morsh_crypto::{Base64Key, Session};
use morsh_network::{Connection, Transport};
use morsh_prediction::{DisplayPreference, NotificationEngine, PredictionEngine};
use morsh_statesync::{Complete, UserStream};
use morsh_terminal::{DisplayDiff, ScreenSnapshot};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const ESCAPE_KEY: u8 = 0x1E; // Ctrl-^ (Ctrl-Shift-6) — stock mosh escape

/// Longest we sleep between housekeeping passes.
const MAX_WAIT: Duration = Duration::from_millis(250);
/// Give up if the server never answers at all.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

type ClientTransport = Transport<UserStream, Complete>;

enum TermEvent {
    Key(Vec<u8>),
    Resize(i32, i32),
    Quit,
}

/// Overlay the prediction engine's speculative cells onto the frame we are
/// about to paint.
///
/// Mosh does the same thing (`OverlayManager::apply`) rather than drawing
/// predictions over the top of the finished frame: a prediction that is folded
/// into the frame participates in the next diff, so it is cleaned up
/// automatically when the server's real output supersedes it.
fn apply_overlay(frame: &mut ScreenSnapshot, prediction: &PredictionEngine) {
    if !prediction.should_display() {
        return;
    }

    let flagging = prediction.is_flagging();
    let late_acked = prediction.local_frame_late_acked();
    let (cols, rows) = (frame.cols as usize, frame.rows_count as usize);

    for row in prediction.overlay().rows() {
        if row.row_num >= rows {
            continue;
        }
        for cell in &row.cells {
            if !cell.active || cell.col >= cols {
                continue;
            }
            let pending = late_acked < cell.expiration_frame;
            let Some(target) = frame.cell_mut(cell.col as u16, row.row_num as u16) else {
                continue;
            };
            if cell.unknown {
                // An unknown prediction says "something will be here", not
                // "a blank will be here" — painting a space over it is what
                // made every keystroke erase the rest of the line.
                if flagging && pending && cell.col + 1 != cols {
                    target.style.underline = true;
                }
                continue;
            }
            target.text = cell.replacement.to_string();
            target.wide = false;
            target.spacer = false;
            if flagging && pending {
                target.style.underline = true;
            }
        }
    }

    if let Some((row, col)) = prediction.cursor_pos() {
        if row < rows && col < cols {
            frame.cursor_y = row as u16;
            frame.cursor_x = col as u16;
        }
    }
}

/// Paint the difference between what is on the user's screen and what the
/// server's latest state (plus predictions) says should be there.
fn output_new_frame(
    transport: &ClientTransport,
    prediction: &PredictionEngine,
    local_framebuffer: &mut ScreenSnapshot,
    stdout: &mut io::Stdout,
) -> io::Result<()> {
    let mut target = transport.get_latest_remote_state().state.snapshot();
    apply_overlay(&mut target, prediction);

    let diff = DisplayDiff::diff(local_framebuffer, &target);
    if !diff.is_empty() {
        stdout.write_all(diff.as_bytes())?;
        stdout.flush()?;
    }
    *local_framebuffer = target;
    Ok(())
}

fn spawn_input_reader(term_tx: mpsc::Sender<TermEvent>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut events = EventStream::new();
        let mut escape_started = false;
        while let Some(Ok(event)) = events.next().await {
            let mut handle_bytes = |bytes: Vec<u8>| {
                // Stock mosh escape sequence: Ctrl-^ (0x1E) then '.' to quit
                if escape_started {
                    if bytes.len() == 1 {
                        match bytes[0] {
                            b'.' => {
                                let _ = term_tx.try_send(TermEvent::Quit);
                                return;
                            }
                            0x1A => { /* suspend - not yet implemented */ }
                            ESCAPE_KEY => {
                                // Send literal escape key
                                let _ = term_tx.try_send(TermEvent::Key(vec![ESCAPE_KEY]));
                            }
                            other => {
                                // Send escape + the byte literally
                                let mut both = vec![ESCAPE_KEY];
                                both.push(other);
                                let _ = term_tx.try_send(TermEvent::Key(both));
                            }
                        }
                    } else {
                        let mut both = vec![ESCAPE_KEY];
                        both.extend(bytes);
                        let _ = term_tx.try_send(TermEvent::Key(both));
                    }
                    escape_started = false;
                    return;
                }
                // Check for escape key (Ctrl-^)
                if bytes.len() == 1 && bytes[0] == ESCAPE_KEY {
                    escape_started = true;
                    return;
                }
                let _ = term_tx.try_send(TermEvent::Key(bytes));
            };
            match event {
                Event::Key(KeyEvent {
                    code,
                    modifiers,
                    kind: crossterm::event::KeyEventKind::Press,
                    ..
                }) => {
                    let mut bytes = Vec::new();
                    match code {
                        KeyCode::Char(ch) => {
                            if modifiers.contains(KeyModifiers::CONTROL) && ch.is_ascii_lowercase() {
                                bytes.push((ch as u8) - b'a' + 1);
                            } else if modifiers.contains(KeyModifiers::ALT) {
                                bytes.push(0x1b);
                                bytes.push(ch as u8);
                            } else {
                                let mut buf = [0u8; 4];
                                bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                            }
                        }
                        KeyCode::Enter => bytes.push(b'\r'),
                        KeyCode::Backspace => bytes.push(0x7f),
                        KeyCode::Tab => bytes.push(b'\t'),
                        KeyCode::Esc => bytes.push(0x1b),
                        KeyCode::Up => bytes.extend_from_slice(b"\x1b[A"),
                        KeyCode::Down => bytes.extend_from_slice(b"\x1b[B"),
                        KeyCode::Right => bytes.extend_from_slice(b"\x1b[C"),
                        KeyCode::Left => bytes.extend_from_slice(b"\x1b[D"),
                        KeyCode::Home => bytes.extend_from_slice(b"\x1b[H"),
                        KeyCode::End => bytes.extend_from_slice(b"\x1b[F"),
                        KeyCode::PageUp => bytes.extend_from_slice(b"\x1b[5~"),
                        KeyCode::PageDown => bytes.extend_from_slice(b"\x1b[6~"),
                        KeyCode::Insert => bytes.extend_from_slice(b"\x1b[2~"),
                        KeyCode::Delete => bytes.extend_from_slice(b"\x1b[3~"),
                        KeyCode::F(n) => match n {
                            1 => bytes.extend_from_slice(b"\x1bOP"),
                            2 => bytes.extend_from_slice(b"\x1bOQ"),
                            3 => bytes.extend_from_slice(b"\x1bOR"),
                            4 => bytes.extend_from_slice(b"\x1bOS"),
                            5 => bytes.extend_from_slice(b"\x1b[15~"),
                            6 => bytes.extend_from_slice(b"\x1b[17~"),
                            7 => bytes.extend_from_slice(b"\x1b[18~"),
                            8 => bytes.extend_from_slice(b"\x1b[19~"),
                            9 => bytes.extend_from_slice(b"\x1b[20~"),
                            10 => bytes.extend_from_slice(b"\x1b[21~"),
                            11 => bytes.extend_from_slice(b"\x1b[23~"),
                            12 => bytes.extend_from_slice(b"\x1b[24~"),
                            _ => {}
                        },
                        _ => {}
                    }
                    if !bytes.is_empty() {
                        handle_bytes(bytes);
                    }
                }
                Event::Resize(w, h) => {
                    let _ = term_tx.send(TermEvent::Resize(w as i32, h as i32)).await;
                }
                _ => {}
            }
        }
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _guard = morsh_log::init("morsh-client");

    let args: Vec<String> = std::env::args().collect();

    // Handle help and version flags before other arg parsing
    if args.len() >= 2 {
        match args[1].as_str() {
            "-h" | "--help" => {
                eprintln!("Usage: morsh-client <server-ip:port>");
                eprintln!();
                eprintln!(
                    "Connects to a morsh-server using the morsh state synchronization protocol."
                );
                eprintln!();
                eprintln!("Options:");
                eprintln!("  -h, --help       Show this help message");
                eprintln!("  -v, --version    Show version information");
                eprintln!();
                eprintln!("Environment:");
                eprintln!("  MORSH_KEY    Base64 encryption key (from morsh-server output)");
                std::process::exit(0);
            }
            "-v" | "--version" => {
                eprintln!("morsh-client (morsh) {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ => {}
        }
    }

    if args.len() < 2 {
        eprintln!("Usage: morsh-client <server-ip:port>");
        eprintln!("  Environment: MORSH_KEY (base64 key from morsh-server)");
        std::process::exit(1);
    }

    let server_addr: SocketAddr = args[1]
        .parse()
        .map_err(|e| format!("Invalid server address '{}': {}", args[1], e))?;

    let key_str = std::env::var("MORSH_KEY")
        .map_err(|_| "MORSH_KEY environment variable not set. Run via morsh wrapper.".to_string())?;
    let key = Base64Key::from_printable(&key_str).map_err(|e| format!("Invalid MORSH_KEY: {e}"))?;

    tracing::info!("Connecting to {server_addr}");

    let session = Session::new(*key.data());
    let mut connection = Connection::new_client(session)
        .await
        .map_err(|e| format!("Failed to create connection: {e}"))?;
    connection.set_remote_addr(server_addr);

    let (cols, rows) = terminal::size()
        .ok()
        .filter(|&(c, r)| c > 0 && r > 0)
        .unwrap_or((80, 24));

    let mut user_stream = UserStream::new();
    user_stream.push_resize(cols as i32, rows as i32);

    let mut transport: ClientTransport =
        Transport::new_client(connection, user_stream, Complete::new(cols, rows)?);

    let mut prediction = PredictionEngine::new();
    prediction.set_display_preference(DisplayPreference::Adaptive);
    prediction.set_send_interval(transport.send_interval_ms());

    let mut notifications = NotificationEngine::new();
    notifications.set_escape_key_string("Ctrl-^ .".to_string());

    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    // What we believe is currently on the user's screen. The alternate screen
    // we just entered is blank, and a blank `Complete` is precisely our model
    // of a blank terminal — building the baseline any other way makes the
    // first frame repaint the whole screen with spaces.
    let mut local_framebuffer = Complete::new(cols, rows)?.snapshot();

    let (term_tx, mut term_rx) = mpsc::channel::<TermEvent>(64);
    let input_handle = spawn_input_reader(term_tx);

    tracing::info!("Entering event loop");
    let started = Instant::now();

    let result: Result<(), Box<dyn std::error::Error>> = loop {
        output_new_frame(&transport, &prediction, &mut local_framebuffer, &mut stdout)?;

        let mut wait = transport.wait_time_ms().min(prediction.wait_time_ms());
        if transport.get_remote_state_num() == 0 {
            wait = wait.min(250);
        }
        let wait = Duration::from_millis(wait).min(MAX_WAIT);

        tokio::select! {
            // Both branches are cancel-safe: `mpsc::Receiver::recv` and
            // `Transport::recv`, whose only await is a `recv_from`.
            Some(term_event) = term_rx.recv() => {
                if handle_term_event(
                    term_event,
                    &mut transport,
                    &mut prediction,
                    &local_framebuffer,
                    &mut notifications,
                ) {
                    // The user asked to quit and we have no server to tell.
                    break Ok(());
                }
            }
            result = transport.recv() => {
                match result {
                    Ok(_) => {
                        notifications.clear_network_error();
                        notifications.server_heard(Instant::now());
                        notifications.server_acked(transport.sender.sent_state_acked_timestamp());

                        prediction.set_local_frame_acked(transport.sender.sent_state_acked());
                        prediction.set_send_interval(transport.send_interval_ms());
                        let remote = &transport.get_latest_remote_state().state;
                        prediction.set_local_frame_late_acked(remote.echo_ack());

                        let screen = remote.snapshot();
                        prediction.validate_predictions(
                            |r, c| {
                                screen
                                    .cell(c as u16, r as u16)
                                    .and_then(|d| d.text.chars().next())
                                    .unwrap_or(' ')
                            },
                            screen.cursor_y as usize,
                            screen.cursor_x as usize,
                        );
                    }
                    Err(e) => {
                        tracing::warn!("Recv error: {e}");
                        notifications.set_network_error(e.to_string());
                    }
                }
            }
            _ = tokio::time::sleep(wait) => {}
        }

        // Quit once our shutdown has been acknowledged, or we give up on it.
        if transport.shutdown_in_progress()
            && (transport.shutdown_acknowledged() || transport.shutdown_ack_timed_out())
        {
            break Ok(());
        }
        // The server asked to shut down and we have acknowledged it.
        if transport.counterparty_shutdown_ack_sent() {
            tracing::info!("Server sent shutdown");
            break Ok(());
        }

        if transport.get_remote_state_num() == 0
            && started.elapsed() > CONNECT_TIMEOUT
            && !transport.shutdown_in_progress()
        {
            tracing::info!("Timed out waiting for server");
            notifications.set_notification("Timed out waiting for server...".to_string(), true);
            break Ok(());
        }

        if let Err(e) = transport.tick().await {
            tracing::warn!("Send error: {e}");
            notifications.set_network_error(e.to_string());
        }

        let now = Instant::now();
        if transport.should_hop_port(now) {
            if let Err(e) = transport.hop_port().await {
                tracing::warn!("Port hop failed: {e}");
            }
        }
    };

    // Cleanup
    let _ = execute!(stdout, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    input_handle.abort();
    println!("morsh is exiting.");

    result
}

/// Returns true if the client should exit immediately.
fn handle_term_event(
    event: TermEvent,
    transport: &mut ClientTransport,
    prediction: &mut PredictionEngine,
    local_framebuffer: &ScreenSnapshot,
    notifications: &mut NotificationEngine,
) -> bool {
    if transport.shutdown_in_progress() {
        return false;
    }

    match event {
        TermEvent::Key(bytes) => {
            // Don't predict for bulk data (a paste).
            let paste = bytes.len() > 100;
            if paste {
                prediction.reset();
            }
            prediction.set_local_frame_sent(transport.sender.sent_state_last());

            let (cols, rows) = (
                local_framebuffer.cols as usize,
                local_framebuffer.rows_count as usize,
            );

            for byte in bytes {
                if !paste {
                    let (cur_row, cur_col) = prediction.cursor_pos().unwrap_or((
                        local_framebuffer.cursor_y as usize,
                        local_framebuffer.cursor_x as usize,
                    ));
                    prediction.new_user_byte(byte as char, cur_row, cur_col, cols, rows, |r, c| {
                        local_framebuffer
                            .cell(c as u16, r as u16)
                            .and_then(|d| d.text.chars().next())
                            .unwrap_or(' ')
                    });
                }
                transport.sender.current_state_mut().push_key(byte);
            }
            false
        }
        TermEvent::Resize(w, h) => {
            transport.sender.current_state_mut().push_resize(w, h);
            prediction.reset();
            tracing::info!("Resize: {w}x{h}");
            false
        }
        TermEvent::Quit => {
            if transport.has_remote_addr() {
                notifications.set_notification("Exiting on user request...".to_string(), true);
                transport.start_shutdown();
                false
            } else {
                true
            }
        }
    }
}
