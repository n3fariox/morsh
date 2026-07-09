use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::{cursor, execute, terminal};
use futures::StreamExt;
use mosh_crypto::{Base64Key, Session};
use mosh_network::{Connection, Transport};
use mosh_prediction::{DisplayPreference, PredictionEngine};
use mosh_statesync::{Complete, UserStream};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;

/// Events from the terminal input thread.
enum TermEvent {
    Key(Vec<u8>),
    Resize(i32, i32),
    Quit,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // Parse command line: mosh-client <server-ip:port>
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: mosh-client <server-ip:port>");
        eprintln!("  Environment: MOSH_KEY (base64 key from mosh-server)");
        std::process::exit(1);
    }

    let server_addr: SocketAddr = args[1]
        .parse()
        .map_err(|e| format!("Invalid server address '{}': {}", args[1], e))?;

    // Read key from environment
    let key_str = std::env::var("MOSH_KEY")
        .map_err(|_| "MOSH_KEY environment variable not set. Run via mosh wrapper.".to_string())?;
    let key = Base64Key::from_printable(&key_str)
        .map_err(|e| format!("Invalid MOSH_KEY: {e}"))?;

    log::info!("Connecting to {server_addr}");

    // Create crypto session
    let session = Session::new(*key.data());

    // Create connection and transport (client side)
    let connection = Connection::new_client(session).await
        .map_err(|e| format!("Failed to create connection: {e}"))?;
    let mut transport = Transport::new_client(connection);
    transport.connection_mut().set_remote_addr(server_addr);

    // State trackers
    let mut user_stream = UserStream::new();
    let mut sent_stream = UserStream::new(); // What we've already sent
    let mut terminal_state = Complete::new(80, 24)?;

    // Prediction engine for speculative local echo
    let mut prediction = PredictionEngine::new();
    prediction.set_display_preference(DisplayPreference::Adaptive);

    // Get initial terminal size
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    user_stream.push_resize(cols as i32, rows as i32);

    // Enable raw mode and alternate screen
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    // Channel for terminal events
    let (term_tx, mut term_rx) = mpsc::channel::<TermEvent>(64);

    // Spawn terminal input reader
    let input_handle = tokio::task::spawn_local(async move {
        let mut events = EventStream::new();
        while let Some(Ok(event)) = events.next().await {
            match event {
                Event::Key(KeyEvent { code, modifiers, kind: crossterm::event::KeyEventKind::Press, .. }) => {
                    let mut bytes = Vec::new();
                    match code {
                        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                            let _ = term_tx.send(TermEvent::Quit).await;
                            return;
                        }
                        KeyCode::Char(ch) => {
                            if modifiers.contains(KeyModifiers::CONTROL) {
                                // Ctrl+char: send control code
                                bytes.push((ch as u8) - b'a' + 1);
                            } else if modifiers.contains(KeyModifiers::ALT) {
                                // Alt+char: send ESC + char
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
                            // F1-F12
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
                        let _ = term_tx.send(TermEvent::Key(bytes)).await;
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
        sent_stream = user_stream.clone();
        transport.sender.advance_state();
    }

    let mut send_timer = tokio::time::interval(Duration::from_millis(50));
    send_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    log::info!("Entering event loop");

    // Main event loop
    let result: Result<(), Box<dyn std::error::Error>> = loop {
        tokio::select! {
            Some(term_event) = term_rx.recv() => {
                match term_event {
                    TermEvent::Key(bytes) => {
                        for byte in bytes {
                            user_stream.push_key(byte);
                            // Feed to prediction engine for speculative echo
                            let ch = byte as char;
                            let (cur_row, cur_col) = prediction.cursor_pos().unwrap_or((0, 0));
                            let (cols, rows) = terminal::size().unwrap_or((80, 24));
                            prediction.new_user_byte(
                                ch,
                                cur_row,
                                cur_col,
                                ' ', // TODO: get actual cell at cursor
                                cols as usize,
                                rows as usize,
                            );
                        }
                    }
                    TermEvent::Resize(w, h) => {
                        user_stream.push_resize(w, h);
                        // Update terminal state dimensions
                        terminal_state = Complete::new(w as u16, h as u16)?;
                        log::info!("Resize: {w}x{h}");
                    }
                    TermEvent::Quit => {
                        break Ok(());
                    }
                }
            }
            result = transport.recv_diff() => {
                match result {
                    Ok(Some(diff)) => {
                        // Apply diff to our terminal state
                        terminal_state.apply_string(&diff.diff);

                        // Validate predictions against new server state
                        let snap = terminal_state.snapshot();
                        prediction.validate_predictions(
                            |r, c| snap.cell(c as u16, r as u16)
                                .and_then(|d| d.text.chars().next())
                                .unwrap_or(' '),
                            snap.cursor_y as usize,
                            snap.cursor_x as usize,
                        );

                        // Write diff bytes directly to terminal
                        stdout.write_all(&diff.diff)?;
                        stdout.flush()?;
                        log::debug!("Applied diff: {} bytes", diff.diff.len());
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log::warn!("Recv error: {e}");
                    }
                }
            }
            _ = send_timer.tick() => {
                // Send pending user stream diffs
                if user_stream.len() > sent_stream.len() {
                    let diff = user_stream.diff_from(&sent_stream);
                    if !diff.is_empty() {
                        let ack_num = transport.receiver.remote_state_num();
                        let throwaway = transport.sender.throwaway_num();
                        if let Err(e) = transport.send_diff(diff, ack_num, throwaway).await {
                            log::warn!("Send error: {e}");
                        } else {
                            sent_stream = user_stream.clone();
                            transport.sender.advance_state();
                            log::debug!("Sent {} bytes of user diff", user_stream.len() - sent_stream.len());
                        }
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                // Periodic: check for port hopping, etc.
                if transport.should_hop_port(std::time::Instant::now()) {
                    if let Err(e) = transport.hop_port().await {
                        log::warn!("Port hop failed: {e}");
                    }
                }
            }
        }
    };

    // Cleanup
    let _ = execute!(stdout, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    input_handle.abort();

    result
}
