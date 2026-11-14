mod cli;
mod frame;
mod input;
mod tty;

use crossterm::terminal;
use morsh_crypto::{Base64Key, Session};
use morsh_network::{Connection, Transport};
use morsh_prediction::{DisplayPreference, NotificationEngine, PredictionEngine};
use morsh_statesync::{Complete, UserStream};
#[cfg(unix)]
use morsh_terminal::ScreenSnapshot;
use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use cli::ParseOutcome;
use frame::output_new_frame;
use input::{handle_term_event, spawn_input_reader, EscapeConfig, TermEvent};
use tty::Tty;

/// Longest we sleep between housekeeping passes.
const MAX_WAIT: Duration = Duration::from_millis(250);
/// Give up if the server never answers at all.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Say that the server hasn't answered once it has been this long.
const CONNECTING_NOTICE_AFTER: Duration = Duration::from_millis(250);
/// Stock mosh-client's warning when the server never acknowledged the end of
/// the session.
const UNCLEAN_SHUTDOWN: &str = "\n\nmorsh did not shut down cleanly. Please note that the\n\
                                morsh-server process may still be running on the server.\n";

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

    // What we believe is currently on the user's screen. The alternate screen
    // we are about to enter is blank, and a blank `Complete` is precisely our
    // model of a blank terminal — building the baseline any other way makes
    // the first frame repaint the whole screen with spaces.
    let mut local_framebuffer = Complete::new(cols, rows)?.snapshot();

    let mut tty = Tty::enter()?;
    let mut stdout = io::stdout();

    let (term_tx, mut term_rx) = mpsc::channel::<TermEvent>(64);
    spawn_input_reader(term_tx, escape);

    let mut signals = Signals::new();

    tracing::info!("Entering event loop");
    let started = Instant::now();
    // Like stock mosh: true once the session ended by agreement with the
    // server, rather than by giving up on it.
    let mut clean_shutdown = false;
    let connecting_notification = format!(
        "Nothing received from server on UDP port {}.",
        server_addr.port()
    );

    let result: Result<(), Box<dyn std::error::Error>> = loop {
        // Not `?`: that would leave without handing the terminal back or
        // saying why.
        if let Err(e) = output_new_frame(
            &transport,
            &mut prediction,
            &mut notifications,
            &mut local_framebuffer,
            &mut stdout,
        ) {
            break Err(e.into());
        }

        let mut wait = transport
            .wait_time_ms()
            .min(prediction.wait_time_ms())
            .min(notifications.wait_time_ms());
        if transport.get_remote_state_num() == 0 {
            wait = wait.min(250);
        }
        let wait = Duration::from_millis(wait).min(MAX_WAIT);

        tokio::select! {
            // Both branches are cancel-safe: `mpsc::Receiver::recv` and
            // `Transport::recv`, whose only await is a `recv_from`.
            Some(term_event) = term_rx.recv() => {
                if let TermEvent::Suspend = term_event {
                    // Like stock mosh, not once the session is ending.
                    #[cfg(unix)]
                    if !transport.shutdown_in_progress() {
                        if let Err(e) = tty.suspend() {
                            break Err(e.into());
                        }
                        forget_screen(&mut local_framebuffer);
                    }
                } else if handle_term_event(
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
                        notifications.server_heard(transport.get_latest_remote_state().timestamp);
                        notifications.server_acked(transport.sender.sent_state_acked_timestamp());

                        prediction.set_send_interval(transport.send_interval_ms());
                        // The predictions are checked against the new state
                        // when the next frame is drawn.
                        prediction.set_local_frame_late_acked(
                            transport.get_latest_remote_state().state.echo_ack(),
                        );
                    }
                    Err(e) => {
                        tracing::warn!("Recv error: {e}");
                        if !transport.shutdown_in_progress() {
                            notifications.set_network_error(e.to_string());
                        }
                    }
                }
            }
            signal = signals.recv() => match signal {
                SignalEvent::Shutdown => {
                    if !transport.has_remote_addr() {
                        break Ok(());
                    }
                    if !transport.shutdown_in_progress() {
                        tracing::info!("Signal received, shutting down");
                        notifications
                            .set_notification("Signal received, shutting down...".to_string(), true);
                        transport.start_shutdown();
                    }
                }
                // Stopped and continued from outside (or after our own
                // suspend): the terminal may have been reset meanwhile.
                #[cfg(unix)]
                SignalEvent::Continued => {
                    if let Err(e) = tty.resume() {
                        break Err(e.into());
                    }
                    forget_screen(&mut local_framebuffer);
                }
            },
            _ = tokio::time::sleep(wait) => {}
        }

        // Quit once our shutdown has been acknowledged, or we give up on it.
        if transport.shutdown_in_progress() && transport.shutdown_acknowledged() {
            clean_shutdown = true;
            break Ok(());
        }
        if transport.shutdown_in_progress() && transport.shutdown_ack_timed_out() {
            break Ok(());
        }
        // The server asked to shut down and we have acknowledged it.
        if transport.counterparty_shutdown_ack_sent() {
            tracing::info!("Server sent shutdown");
            clean_shutdown = true;
            break Ok(());
        }

        let still_connecting = transport.get_remote_state_num() == 0;
        if still_connecting
            && started.elapsed() > CONNECT_TIMEOUT
            && !transport.shutdown_in_progress()
        {
            tracing::info!("Timed out waiting for server");
            notifications.set_notification("Timed out waiting for server...".to_string(), true);
            break Ok(());
        }
        // Say so if the server hasn't answered yet, as stock mosh does.
        if still_connecting
            && !transport.shutdown_in_progress()
            && started.elapsed() > CONNECTING_NOTICE_AFTER
        {
            notifications.set_notification(connecting_notification.clone(), false);
        } else if !still_connecting && notifications.notification() == connecting_notification {
            notifications.set_notification(String::new(), false);
        }

        match transport.tick().await {
            Ok(()) => notifications.clear_network_error(),
            Err(e) => {
                tracing::warn!("Send error: {e}");
                notifications.set_network_error(e.to_string());
            }
        }

        let now = Instant::now();
        if transport.should_hop_port(now) {
            if let Err(e) = transport.hop_port().await {
                tracing::warn!("Port hop failed: {e}");
            }
        }
    };

    tty.restore();

    // As stock mosh-client's `STMClient::shutdown` and `main`.
    if transport.get_remote_state_num() == 0 {
        eprint!("{}", connect_failure_hint(server_addr, started.elapsed()));
    } else if !clean_shutdown {
        eprint!("{UNCLEAN_SHUTDOWN}");
    }
    if let Err(e) = &result {
        eprint!("Error: {e}\r\n");
    }
    println!("[morsh is exiting.]");

    if result.is_err() || !clean_shutdown {
        std::process::exit(1);
    }
    Ok(())
}

/// The alternate screen was entered afresh and is blank: model it so, and
/// the next frame repaints everything.
#[cfg(unix)]
fn forget_screen(local_framebuffer: &mut ScreenSnapshot) {
    let (cols, rows) = (local_framebuffer.cols, local_framebuffer.rows_count);
    *local_framebuffer = Complete::new(cols, rows)
        .map(|blank| blank.snapshot())
        .unwrap_or_else(|_| ScreenSnapshot::new(cols, rows));
}

/// Nothing is listened for on Windows yet.
#[cfg_attr(not(unix), allow(dead_code))]
enum SignalEvent {
    /// End the session.
    Shutdown,
    /// The process was continued after being stopped.
    #[cfg(unix)]
    Continued,
}

/// The signals stock mosh-client acts on: SIGTERM, SIGINT, SIGHUP and
/// SIGPIPE end the session, and SIGCONT takes the terminal back over.
struct Signals {
    #[cfg(unix)]
    shutdown: [Option<tokio::signal::unix::Signal>; 4],
    #[cfg(unix)]
    continued: Option<tokio::signal::unix::Signal>,
}

impl Signals {
    fn new() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let listen = |kind: SignalKind| {
                signal(kind)
                    .inspect_err(|e| tracing::warn!("Failed to listen for {kind:?}: {e}"))
                    .ok()
            };
            Self {
                shutdown: [
                    listen(SignalKind::terminate()),
                    listen(SignalKind::interrupt()),
                    listen(SignalKind::hangup()),
                    listen(SignalKind::pipe()),
                ],
                continued: listen(SignalKind::from_raw(libc::SIGCONT)),
            }
        }
        #[cfg(not(unix))]
        Self {}
    }

    /// Wait for the next signal. Cancel-safe.
    async fn recv(&mut self) -> SignalEvent {
        #[cfg(unix)]
        {
            async fn one(signal: &mut Option<tokio::signal::unix::Signal>) {
                if let Some(signal) = signal {
                    if signal.recv().await.is_some() {
                        return;
                    }
                }
                std::future::pending().await
            }
            let [a, b, c, d] = &mut self.shutdown;
            tokio::select! {
                () = one(a) => SignalEvent::Shutdown,
                () = one(b) => SignalEvent::Shutdown,
                () = one(c) => SignalEvent::Shutdown,
                () = one(d) => SignalEvent::Shutdown,
                () = one(&mut self.continued) => SignalEvent::Continued,
            }
        }
        #[cfg(not(unix))]
        std::future::pending().await
    }
}

/// Message shown when the server never answers. SSH already worked, so the
/// server is almost certainly running and UDP is being blocked on the way.
fn connect_failure_hint(server_addr: SocketAddr, waited: Duration) -> String {
    let port = server_addr.port();
    let waited = waited.as_secs();
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
