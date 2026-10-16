use crate::pty::{self, MorshPty, PtyEvent};
use morsh_crypto::{Base64Key, Session};
use morsh_network::{Connection, RecvOutcome, State, Transport};
use morsh_statesync::{Complete, UserEvent, UserStream};
use portable_pty::PtySize;
use std::io::Write;
use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

/// How long to wait for a first client packet before giving up.
const TIMEOUT_IF_NO_CLIENT: Duration = Duration::from_secs(60);
/// Longest we sleep between housekeeping passes.
const MAX_WAIT: Duration = Duration::from_millis(5000);

type ServerTransport = Transport<Complete, UserStream>;

struct Handler {
    transport: ServerTransport,
    pty: MorshPty,
    /// Newest remote state we have already fed to the terminal.
    last_remote_num: u64,
}

pub async fn run_server(
    bind_port: u16,
    desired_ip: Option<&str>,
    key: Base64Key,
    shell: String,
    command_args: Vec<String>,
    locale_vars: Vec<(String, String)>,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!(
        "run_server entered: port={bind_port}, shell={shell}, ip={:?}, args.len={}",
        desired_ip,
        command_args.len()
    );

    let mut transport = bind_transport(bind_port, desired_ip, &key).await?;
    tracing::info!("UDP transport ready, waiting for client on port {bind_port}");

    let pty_size = wait_for_client(&mut transport).await?;

    tracing::info!(
        "Client connected, spawning shell (remote_state_num={})",
        transport.get_remote_state_num()
    );

    // The terminal the protocol synchronizes is the sender's current state;
    // there is no second copy to keep in step with it.
    transport
        .get_current_state_mut()
        .resize(pty_size.cols, pty_size.rows)
        .map_err(|e| format!("Failed to size terminal: {e}"))?;

    let pty_setup = pty::spawn_pty(pty_size, &shell, &command_args, &locale_vars)?;

    let last_remote_num = transport.get_remote_state_num();
    let mut handler = Handler {
        transport,
        pty: pty_setup,
        last_remote_num,
    };

    handler.run().await
}

async fn bind_transport(
    bind_port: u16,
    desired_ip: Option<&str>,
    key: &Base64Key,
) -> Result<ServerTransport, Box<dyn std::error::Error>> {
    let session = Session::new(*key.data());

    let connection = if let Some(ip) = desired_ip {
        let addr = SocketAddr::new(
            ip.parse::<std::net::IpAddr>()
                .map_err(|e| format!("Invalid bind address: {e}"))?,
            bind_port,
        );
        match Connection::new_server(addr, session).await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!("Failed to bind to {}: {}, trying 0.0.0.0", ip, e);
                let fallback: SocketAddr = format!("0.0.0.0:{}", bind_port).parse().unwrap();
                Connection::new_server(fallback, Session::new(*key.data()))
                    .await
                    .map_err(|e| format!("Failed to bind: {e}"))?
            }
        }
    } else {
        let addr: SocketAddr = format!("0.0.0.0:{}", bind_port).parse().unwrap();
        Connection::new_server(addr, session)
            .await
            .map_err(|e| format!("Failed to bind: {e}"))?
    };

    Ok(Transport::new_server(
        connection,
        Complete::new(80, 24)?,
        UserStream::new(),
    ))
}

/// Block until the client's first state arrives, and take the window size it
/// asks for. Mosh releases the login shell at the same point.
async fn wait_for_client(transport: &mut ServerTransport) -> Result<PtySize, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + TIMEOUT_IF_NO_CLIENT;
    let mut pty_size = PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    };

    while transport.get_remote_state_num() == 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "No connection within {} seconds",
                TIMEOUT_IF_NO_CLIENT.as_secs()
            )
            .into());
        }

        tokio::select! {
            result = transport.recv() => match result {
                Ok(RecvOutcome::NewState { num }) => {
                    tracing::info!("Client's first state: {num}");
                }
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!("Error waiting for client: {e}");
                    continue;
                }
            },
            _ = tokio::time::sleep(remaining) => continue,
        }
    }

    let mut stream = UserStream::new();
    if let Err(e) = stream.apply_string(&transport.get_remote_diff()) {
        tracing::warn!("Client's first state was malformed: {e}");
    }
    for event in stream.events() {
        if let UserEvent::Resize { width, height } = event {
            pty_size = PtySize {
                rows: (*height).clamp(1, u16::MAX as i32) as u16,
                cols: (*width).clamp(1, u16::MAX as i32) as u16,
                pixel_width: 0,
                pixel_height: 0,
            };
            tracing::info!("Client initial resize: {}x{}", pty_size.cols, pty_size.rows);
        }
    }

    Ok(pty_size)
}

impl Handler {
    async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        tracing::info!("Entering serve loop");

        let result = loop {
            let wait = self.wait_time();

            tokio::select! {
                // `mpsc::Receiver::recv` and `Transport::recv` are both
                // cancel-safe, so losing this race never drops an event.
                Some(pty_event) = self.pty.rx.recv() => {
                    self.handle_pty_event(pty_event);
                }
                result = self.transport.recv() => {
                    if let Err(e) = result {
                        tracing::warn!("Recv error: {e}");
                    }
                }
                _ = tokio::time::sleep(wait) => {}
            }

            if self.handle_client_input().is_break() {
                break Ok(());
            }

            if self.housekeeping().is_break() {
                break Ok(());
            }

            if let Err(e) = self.transport.tick().await {
                tracing::warn!("Send error: {e}");
            }
        };

        self.shutdown().await;
        result
    }

    /// How long we may sleep before the transport or the echo-ack timer needs
    /// attention.
    fn wait_time(&mut self) -> Duration {
        let now = Instant::now();
        let mut wait = self.transport.wait_time_ms();
        if let Some(echo) = self.transport.get_current_state().wait_time_ms(now) {
            wait = wait.min(echo);
        }
        Duration::from_millis(wait).min(MAX_WAIT)
    }

    fn handle_pty_event(&mut self, event: PtyEvent) {
        match event {
            PtyEvent::Output(data) => {
                tracing::debug!("PTY output: {} bytes", data.len());
                if self.transport.shutdown_in_progress() {
                    return;
                }
                if let Err(e) = self.transport.get_current_state_mut().act(&data) {
                    tracing::warn!("Terminal error: {e}");
                }
            }
            PtyEvent::Exited(status) => {
                tracing::info!("Shell exited with status: {}", status.exit_code());
                // The final screen goes out with the shutdown packet.
                self.transport.start_shutdown();
            }
        }
    }

    /// Feed any newly received user input to the terminal and the child.
    fn handle_client_input(&mut self) -> ControlFlow<()> {
        let remote_num = self.transport.get_remote_state_num();
        if remote_num == self.last_remote_num {
            return ControlFlow::Continue(());
        }
        self.last_remote_num = remote_num;

        let mut stream = UserStream::new();
        if let Err(e) = stream.apply_string(&self.transport.get_remote_diff()) {
            tracing::warn!("Malformed user input from client: {e}");
            return ControlFlow::Continue(());
        }

        // Keystrokes go to the pty in one write per run, not one per byte.
        // Windows ConPTY parses each write on its own and takes an ESC that
        // ends a write as the Escape key, so a byte-at-a-time Shift+Left
        // (`ESC [ 1 ; 2 D`) reached the shell as Escape plus "[1;2D".
        let mut keys = Vec::new();
        let events: Vec<UserEvent> = stream.events().to_vec();
        for (i, event) in events.iter().enumerate() {
            match event {
                UserEvent::UserByte(byte) => keys.push(*byte),
                UserEvent::Resize { width, height } => {
                    // Apply only the last of a run of resizes.
                    if matches!(events.get(i + 1), Some(UserEvent::Resize { .. })) {
                        continue;
                    }
                    self.write_keys(&mut keys);
                    self.apply_resize(*width, *height);
                }
            }
        }
        self.write_keys(&mut keys);

        if !events.is_empty() && !self.transport.shutdown_in_progress() {
            // Remember when this input arrived, so the echo ack can tell the
            // client when it was actually echoed rather than merely received.
            self.transport
                .get_current_state_mut()
                .register_input_frame(remote_num, Instant::now());
        }

        ControlFlow::Continue(())
    }

    /// Write and drain pending keystrokes as a single pty write.
    fn write_keys(&mut self, keys: &mut Vec<u8>) {
        if keys.is_empty() {
            return;
        }
        if let Err(e) = self.pty.writer.write_all(keys) {
            tracing::warn!("PTY write error: {e}");
        }
        if let Err(e) = self.pty.writer.flush() {
            tracing::warn!("PTY flush error: {e}");
        }
        keys.clear();
    }

    fn apply_resize(&mut self, width: i32, height: i32) {
        let w = width.clamp(1, u16::MAX as i32) as u16;
        let h = height.clamp(1, u16::MAX as i32) as u16;

        let _ = self.pty.master.resize(PtySize {
            rows: h,
            cols: w,
            pixel_width: 0,
            pixel_height: 0,
        });

        if self.transport.shutdown_in_progress() {
            return;
        }
        // Resize the terminal in place. Replacing it with a fresh blank one
        // threw away everything on the screen.
        if let Err(e) = self.transport.get_current_state_mut().resize(w, h) {
            tracing::warn!("Failed to resize terminal: {e}");
        }
        tracing::info!("Client resize: {w}x{h}");
    }

    /// Everything mosh's serve loop does between reading and ticking.
    fn housekeeping(&mut self) -> ControlFlow<()> {
        if let Ok(Some(status)) = self.pty.child.try_wait() {
            tracing::info!("Child process exited with status {}", status.exit_code());
            self.transport.start_shutdown();
        }

        if self.transport.shutdown_in_progress() {
            if self.transport.shutdown_acknowledged() {
                tracing::info!("Shutdown acknowledged by client");
                return ControlFlow::Break(());
            }
            if self.transport.shutdown_ack_timed_out() {
                tracing::info!("Gave up waiting for the client to acknowledge shutdown");
                return ControlFlow::Break(());
            }
        } else {
            // Publish a new echo ack when input has been sitting in the
            // terminal long enough to count as echoed.
            self.transport
                .get_current_state_mut()
                .set_echo_ack(Instant::now());
        }

        // The client asked to shut down and we have acknowledged it.
        if self.transport.counterparty_shutdown_ack_sent() {
            tracing::info!("Client requested shutdown; acknowledged");
            return ControlFlow::Break(());
        }

        ControlFlow::Continue(())
    }

    async fn shutdown(&mut self) {
        tracing::info!("Shutting down");
        let _ = self.pty.child.kill();
    }
}
