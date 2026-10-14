mod cli;
mod frame;
mod input;

use crossterm::{cursor, execute, terminal};
use morsh_crypto::{Base64Key, Session};
use morsh_network::{Connection, Transport};
use morsh_prediction::{DisplayPreference, NotificationEngine, PredictionEngine};
use morsh_statesync::{Complete, UserStream};
use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use cli::ParseOutcome;
use frame::output_new_frame;
use input::{handle_term_event, spawn_input_reader, EscapeConfig, TermEvent};

/// Longest we sleep between housekeeping passes.
const MAX_WAIT: Duration = Duration::from_millis(250);
/// Give up if the server never answers at all.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub type ClientTransport = Transport<UserStream, Complete>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _guard = morsh_log::init("morsh-client");

    let args: Vec<String> = std::env::args().collect();
    let server_addr: SocketAddr = match cli::parse(&args) {
        ParseOutcome::Connect(addr) => addr,
        ParseOutcome::Help => {
            eprintln!("{}", cli::HELP);
            return Ok(());
        }
        ParseOutcome::Version => {
            eprintln!("morsh-client (morsh) {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        ParseOutcome::Error(msg) => {
            eprintln!("{msg}");
            std::process::exit(1);
        }
    };

    let key_str = std::env::var("MORSH_KEY").map_err(|_| {
        "MORSH_KEY environment variable not set. Run via morsh wrapper.".to_string()
    })?;
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
    // Same variable and values as stock mosh-client
    let display_preference = match std::env::var("MOSH_PREDICTION_DISPLAY").as_deref() {
        Ok("always") => DisplayPreference::Always,
        Ok("never") => DisplayPreference::Never,
        _ => DisplayPreference::Adaptive,
    };
    tracing::info!("Prediction display: {display_preference:?}");
    prediction.set_display_preference(display_preference);
    prediction.set_send_interval(transport.send_interval_ms());

    let mut notifications = NotificationEngine::new();
    // Same variable and values as stock mosh-client; empty disables it.
    let escape = EscapeConfig::from_env();
    if let Some(escape) = &escape {
        notifications.set_escape_key_string(escape.quit_keystroke());
        notifications.set_escape_key_help(escape.help());
    }

    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    #[cfg(windows)]
    let original_console_mode = enable_delayed_wrap();
    #[cfg(windows)]
    let original_input_mode = input::enable_vt_input();
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    // What we believe is currently on the user's screen. The alternate screen
    // we just entered is blank, and a blank `Complete` is precisely our model
    // of a blank terminal — building the baseline any other way makes the
    // first frame repaint the whole screen with spaces.
    let mut local_framebuffer = Complete::new(cols, rows)?.snapshot();

    let (term_tx, mut term_rx) = mpsc::channel::<TermEvent>(64);
    spawn_input_reader(term_tx, escape);

    tracing::info!("Entering event loop");
    let started = Instant::now();
    let mut connect_failed = false;

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
            connect_failed = true;
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
    // The saved input mode is the raw one, so put it back before leaving raw mode.
    #[cfg(windows)]
    input::restore_input_mode(original_input_mode);
    let _ = terminal::disable_raw_mode();
    #[cfg(windows)]
    restore_console_mode(original_console_mode);
    println!("morsh is exiting.");

    if connect_failed {
        eprint!("{}", connect_failure_hint(server_addr));
        std::process::exit(1);
    }

    result
}

/// Message shown when the server never answers. SSH already worked, so the
/// server is almost certainly running and UDP is being blocked on the way.
fn connect_failure_hint(server_addr: SocketAddr) -> String {
    let port = server_addr.port();
    let waited = CONNECT_TIMEOUT.as_secs();
    format!(
        "morsh: no response from server at {server_addr} (UDP) after {waited}s.\n\
         \n\
         SSH worked, so the server was started, but its UDP packets aren't reaching us.\n\
         This is usually a firewall on the remote host (or between you and it).\n\
         Allow inbound UDP port {port} (mosh/morsh use 60000-61000 by default), e.g.:\n\
         \n\
         \x20 ufw:          sudo ufw allow 60000:61000/udp\n\
         \x20 firewalld:    sudo firewall-cmd --permanent --add-port=60000-61000/udp && sudo firewall-cmd --reload\n\
         \x20 nftables:     sudo nft add rule inet filter input udp dport 60000-61000 accept\n"
    )
}

/// Windows console: make the cursor wait at the right margin instead of
/// wrapping as soon as the last column is written (which scrolls the screen
/// when on the bottom row). Returns the original mode to restore on exit.
#[cfg(windows)]
fn enable_delayed_wrap() -> Option<u32> {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, DISABLE_NEWLINE_AUTO_RETURN,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
    };
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return None;
        }
        let new_mode = mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN;
        if SetConsoleMode(handle, new_mode) == 0 {
            tracing::warn!("Failed to set console mode {new_mode:#x}");
            return None;
        }
        Some(mode)
    }
}

#[cfg(windows)]
fn restore_console_mode(mode: Option<u32>) {
    use windows_sys::Win32::System::Console::{GetStdHandle, SetConsoleMode, STD_OUTPUT_HANDLE};
    if let Some(mode) = mode {
        unsafe {
            SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), mode);
        }
    }
}
