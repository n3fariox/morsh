use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::{cursor, execute, terminal};
use futures::StreamExt;
use morsh_crypto::{Base64Key, Session};
use morsh_network::{Connection, Transport};
use morsh_prediction::{DisplayPreference, NotificationEngine, PredictionEngine};
use morsh_proto::host::HostMessage;
use morsh_statesync::{Complete, UserStream};
use prost::Message;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;

const ESCAPE_KEY: u8 = 0x1E; // Ctrl-^ (Ctrl-Shift-6) — stock mosh escape

enum TermEvent {
    Key(Vec<u8>),
    Resize(i32, i32),
    Quit,
}

/// Render the prediction overlay with underlined characters for pending predictions.
///
/// Shows users which characters have been predicted locally but not yet
/// confirmed by the server. Uses a single pass with buffered output to
/// minimize terminal write overhead.
fn render_prediction_overlay(
    prediction: &morsh_prediction::PredictionEngine,
    stdout: &mut io::Stdout,
) -> Result<(), io::Error> {
    if !prediction.should_display() {
        return Ok(());
    }

    let overlay = prediction.overlay();
    let late_acked = prediction.local_frame_late_acked();
    let (cols, rows) = terminal::size().unwrap_or((80, 24));

    use std::fmt::Write;
    let mut buf = String::with_capacity(4096);
    buf.push_str("\x1b[s");

    for row in overlay.rows() {
        if row.row_num >= rows as usize {
            continue;
        }
        for cell in &row.cells {
            if !cell.active || cell.col >= cols as usize {
                continue;
            }
            let pending = late_acked < cell.expiration_frame;
            write!(buf, "\x1b[{};{}H\x1b[0m", row.row_num + 1, cell.col + 1).unwrap();
            if pending {
                buf.push_str("\x1b[4m");
            }
            buf.push(if cell.unknown { ' ' } else { cell.replacement });
            if pending {
                buf.push_str("\x1b[0m");
            }
        }
    }

    buf.push_str("\x1b[u");
    write!(stdout, "{buf}")?;
    stdout.flush()
}

/// Extract raw VT bytes and optional EchoAck from a server diff.
/// Stock mosh-server wraps VT data in a `HostBuffers::HostMessage` protobuf.
/// Our own morsh-server sends raw VT bytes directly.
fn extract_host_message(data: &[u8]) -> (Vec<u8>, Option<u64>) {
    if let Ok(msg) = HostMessage::decode(data) {
        let mut vt_bytes = Vec::new();
        let mut echo_ack = None;
        for inst in &msg.instruction {
            if let Some(ref hb) = inst.hostbytes {
                if let Some(ref s) = hb.hoststring {
                    vt_bytes.extend_from_slice(s);
                }
            }
            if let Some(ref ea) = inst.echoack {
                if ea.echo_ack_num.is_some() {
                    echo_ack = echo_ack.max(ea.echo_ack_num);
                }
            }
        }
        if !vt_bytes.is_empty() || echo_ack.is_some() {
            return (vt_bytes, echo_ack);
        }
    }
    (data.to_vec(), None)
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
                eprintln!("Connects to a morsh-server using the morsh state synchronization protocol.");
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
    let key = Base64Key::from_printable(&key_str)
        .map_err(|e| format!("Invalid MORSH_KEY: {e}"))?;

    tracing::info!("Connecting to {server_addr}");

    let session = Session::new(*key.data());

    let connection = Connection::new_client(session).await
        .map_err(|e| format!("Failed to create connection: {e}"))?;
    let mut transport = Transport::new_client(connection);
    transport.connection_mut().set_remote_addr(server_addr);

    // Activate the transport (client starts in Active state)
    transport.sender.set_state(morsh_network::transport::SendState::Active);

    let mut user_stream = UserStream::new();
    let mut sent_stream = UserStream::new();
    let (cols, rows) = terminal::size()
        .ok()
        .filter(|&(c, r)| c > 0 && r > 0)
        .unwrap_or((80, 24));
    let mut terminal_state = Complete::new(cols, rows)?;

    let mut prediction = PredictionEngine::new();
    prediction.set_display_preference(DisplayPreference::Adaptive);
    prediction.set_send_interval(transport.sender.send_interval_ms());

    let mut notifications = NotificationEngine::new();
    notifications.set_escape_key_string("Ctrl-^ .".to_string());

    user_stream.push_resize(cols as i32, rows as i32);

    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let (term_tx, mut term_rx) = mpsc::channel::<TermEvent>(64);

    // Use spawn instead of spawn_local (no LocalSet available)
    let input_handle = tokio::spawn(async move {
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
                Event::Key(KeyEvent { code, modifiers, kind: crossterm::event::KeyEventKind::Press, .. }) => {
                    let mut bytes = Vec::new();
                    match code {
                        KeyCode::Char(ch) => {
                            if modifiers.contains(KeyModifiers::CONTROL) && ch.is_ascii_lowercase() {
                                bytes.push((ch as u8) - b'a' + 1);
                            } else if modifiers.contains(KeyModifiers::ALT) {
                                bytes.push(0x1b);
                                bytes.push(ch as u8);
                            } else {
                                bytes.push(ch as u8);
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
                        KeyCode::F(n) => {
                            match n {
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
                            }
                        }
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
    });

    // Send initial state
    let init_diff = user_stream.diff_from(&sent_stream);
    if !init_diff.is_empty() {
        let ack_num = transport.receiver.remote_state_num();
        let throwaway = transport.sender.throwaway_num();
        transport.send_diff(init_diff.clone(), ack_num, throwaway).await?;
        tracing::debug!("Sent initial diff ({} bytes) state=0>1", init_diff.len());
        sent_stream = user_stream.clone();
        transport.sender.advance_state();
    }

    // Adaptive send timer based on RTT
    let send_interval = Duration::from_millis(transport.sender.send_interval_ms());
    let mut send_timer = tokio::time::interval(send_interval);
    send_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Handshake: wait for server response with timeout and retry
    let mut handshake_retries = 0;
    const MAX_HANDSHAKE_RETRIES: u32 = 10;
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
    let mut handshake_deadline = std::time::Instant::now() + HANDSHAKE_TIMEOUT;

    tracing::info!("Entering event loop");

    let result: Result<(), Box<dyn std::error::Error>> = loop {
        tokio::select! {
            Some(term_event) = term_rx.recv() => {
                match term_event {
                    TermEvent::Key(bytes) => {
                        let snap = terminal_state.snapshot();
                        let (cols, rows) = terminal::size().unwrap_or((80, 24));
                        for byte in bytes {
                            user_stream.push_key(byte);
                            // Get actual cell at cursor for prediction
                            let (cur_row, cur_col) = prediction.cursor_pos()
                                .unwrap_or((snap.cursor_y as usize, snap.cursor_x as usize));
                            let cell_char = snap.cell(cur_col as u16, cur_row as u16)
                                .and_then(|d| d.text.chars().next())
                                .unwrap_or(' ');
                            prediction.new_user_byte(
                                byte as char,
                                cur_row,
                                cur_col,
                                cell_char,
                                cols as usize,
                                rows as usize,
                            );
                        }
                        // Update prediction frame tracking
                        let frame = transport.sender.state_num();
                        prediction.set_local_frame_sent(frame);

                        if let Err(e) = render_prediction_overlay(&prediction, &mut stdout) {
                            tracing::debug!("Overlay render error: {e}");
                        }

                        // Send keystroke immediately if enough time has passed since last send
                        let now = std::time::Instant::now();
                        if transport.sender.should_send(now) && user_stream.len() > sent_stream.len() {
                            let diff = user_stream.diff_from(&sent_stream);
                            if !diff.is_empty() {
                                let ack_num = transport.receiver.remote_state_num();
                                let throwaway = transport.sender.throwaway_num();
                                let bytes_to_send = diff.len();
                                let state_before = transport.sender.state_num();
                                tracing::debug!("SEND immediate: state={state_before} ack={ack_num} diff={diff:?}");
                                if let Err(e) = transport.send_diff(diff, ack_num, throwaway).await {
                                    tracing::warn!("Send error: {e}");
                                } else {
                                    sent_stream = user_stream.clone();
                                    transport.sender.advance_state();
                                    tracing::debug!("Sent {bytes_to_send} bytes (immediate) state now={}", transport.sender.state_num());
                                }
                            }
                        }
                    }
                    TermEvent::Resize(w, h) => {
                        user_stream.push_resize(w, h);
                        terminal_state = Complete::new(w as u16, h as u16)?;
                        prediction.reset();
                        tracing::info!("Resize: {w}x{h}");
                    }
                    TermEvent::Quit => {
                        break Ok(());
                    }
                }
            }
            result = transport.recv_diff() => {
                match result {
                    Ok(Some(diff)) => {
                        // Handshake complete — stop retransmitting full state
                        handshake_retries = 0;
                        handshake_deadline = std::time::Instant::now()
                            + std::time::Duration::from_secs(86400 * 365);
                        notifications.clear_network_error();
                        notifications.server_heard(std::time::Instant::now());

                        // Stock mosh-server sends HostMessage protobuf wrapper; our own server sends raw VT bytes.
                        let (vt_bytes, echo_ack) = extract_host_message(&diff.diff);
                        terminal_state.apply_string(&vt_bytes);

                        // Update prediction frame tracking from server acks
                        let acked = transport.sender.acked_state_num();
                        prediction.set_local_frame_acked(acked);
                        prediction.set_local_frame_late_acked(echo_ack.unwrap_or(acked));

                        // Validate predictions against new server state
                        let snap = terminal_state.snapshot();
                        prediction.validate_predictions(
                            |r, c| snap.cell(c as u16, r as u16)
                                .and_then(|d| d.text.chars().next())
                                .unwrap_or(' '),
                            snap.cursor_y as usize,
                            snap.cursor_x as usize,
                        );

                        tracing::debug!("RECV vt_bytes=[{:?}] raw_diff=[{:?}] new_num={} ack_num={}",
                            String::from_utf8_lossy(&vt_bytes),
                            String::from_utf8_lossy(&diff.diff),
                            diff.new_num,
                            diff.ack_num,
                        );
                        stdout.write_all(&vt_bytes)?;
                        stdout.flush()?;

                        // Check for server shutdown
                        if transport.receiver.shutdown_received() {
                            tracing::info!("Server sent shutdown");
                            // ACK the server's shutdown packet with ack_num = u64::MAX
                            // so the server knows we received it and can exit cleanly
                            if let Err(e) = transport.send_shutdown_ack().await {
                                tracing::warn!("Failed to send shutdown ACK: {e}");
                            }
                            break Ok(());
                        }

                        // Render prediction overlay (underlined characters for pending predictions)
                        if let Err(e) = render_prediction_overlay(&prediction, &mut stdout) {
                            tracing::debug!("Overlay render error: {e}");
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!("Recv error: {e}");
                        notifications.set_network_error(e.to_string());
                    }
                }

                // Update send interval from RTT
                let rtt_ms = transport.connection().rtt().srtt_ms();
                transport.update_send_interval(rtt_ms);
                prediction.set_send_interval(rtt_ms);
            }
            _ = send_timer.tick() => {
                let now = std::time::Instant::now();

                // Handshake timeout: retransmit initial diff if no server response yet
                if handshake_retries < MAX_HANDSHAKE_RETRIES && now >= handshake_deadline {
                    handshake_retries += 1;
                    handshake_deadline = now + HANDSHAKE_TIMEOUT;
                    tracing::warn!("Handshake timeout ({}/{}), retransmitting initial diff", handshake_retries, MAX_HANDSHAKE_RETRIES);
                    // Use send_handshake_message so the state numbers are always
                    // old=0, new=1, even if the sender's state_num has advanced.
                    let init_diff = user_stream.diff_from(&UserStream::new());
                    if !init_diff.is_empty() {
                        let ack_num = transport.receiver.remote_state_num();
                        let throwaway = transport.sender.throwaway_num();
                        if let Err(e) = transport.send_handshake_message(init_diff, ack_num, throwaway).await {
                            tracing::warn!("Handshake retransmit failed: {e}");
                        }
                    }
                } else if handshake_retries > 0 {
                    // Handshake succeeded (we got a response)
                    handshake_retries = 0;
                }

                // Check for connection loss (no data from server for 15s)
                if transport.connection().time_since_last_heard(now) > std::time::Duration::from_secs(15) {
                    tracing::info!("Connection lost (no server response for 15s)");
                    break Ok(());
                }

                // Send pending user stream diffs
                if user_stream.len() > sent_stream.len() {
                    let diff = user_stream.diff_from(&sent_stream);
                    if !diff.is_empty() {
                        let ack_num = transport.receiver.remote_state_num();
                        let throwaway = transport.sender.throwaway_num();
                        let bytes_to_send = diff.len();
                        let state_before = transport.sender.state_num();
                        tracing::debug!("SEND timer: state={state_before} ack={ack_num} diff={diff:?}");
                        if let Err(e) = transport.send_diff(diff, ack_num, throwaway).await {
                            tracing::warn!("Send error: {e}");
                        } else {
                            sent_stream = user_stream.clone();
                            transport.sender.advance_state();
                            tracing::debug!("Sent {bytes_to_send} bytes (timer) state now={}", transport.sender.state_num());
                        }
                    }
                }

                // Send delayed ACK if needed
                let now = std::time::Instant::now();
                if transport.should_send_ack(now) {
                    let ack_num = transport.receiver.remote_state_num();
                    if let Err(e) = transport.send_ack(ack_num).await {
                        tracing::warn!("ACK send error: {e}");
                    }
                }

                // Check for port hopping
                if transport.should_hop_port(now) {
                    if let Err(e) = transport.hop_port().await {
                        tracing::warn!("Port hop failed: {e}");
                    }
                }
            }
        }
    };

    // Send shutdown marker to server
    tracing::info!("Sending shutdown marker");
    println!("morsh is exiting.");
    if let Err(e) = transport.send_shutdown().await {
        tracing::warn!("Failed to send shutdown: {e}");
    }

    // Cleanup
    let _ = execute!(stdout, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    input_handle.abort();

    result
}
