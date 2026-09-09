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
use input::{handle_term_event, spawn_input_reader, TermEvent};

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
