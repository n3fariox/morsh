use crate::pty::{self, MorshPty, PtyEvent};
use morsh_crypto::{Base64Key, Session};
use morsh_network::transport::{ReceivedDiff, SendState};
use morsh_network::{Connection, Transport};
use morsh_proto::client::UserMessage;
use morsh_statesync::Complete;
use morsh_terminal::ScreenSnapshot;
use portable_pty::PtySize;
use prost::Message;
use std::io::Write;
use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::time::Duration;

struct Handler {
    transport: Transport,
    terminal_state: Complete,
    client_assumed_state: ScreenSnapshot,
    pty: MorshPty,
}

pub async fn run_server(
    bind_port: u16,
    desired_ip: Option<&str>,
    key: Base64Key,
    shell: String,
    command_args: Vec<String>,
    locale_vars: Vec<(String, String)>,
) -> Result<(), Box<dyn std::error::Error>> {
    log::info!(
        "run_server entered: port={bind_port}, shell={shell}, ip={:?}, args.len={}",
        desired_ip,
        command_args.len()
    );

    let mut transport = bind_transport(bind_port, desired_ip, &key).await?;
    transport.sender.set_state(SendState::Active);
    log::info!("UDP transport ready, waiting for client on port {bind_port}");

    let (terminal_state, pty_size) = wait_for_client(&mut transport).await?;

    log::info!(
        "Client connected, spawning shell (remote_state_num={})",
        transport.receiver.remote_state_num()
    );

    let pty_setup = pty::spawn_pty(pty_size, &shell, &command_args, &locale_vars)?;
    let client_assumed_state: ScreenSnapshot = Complete::new(80, 24)?.snapshot();

    let mut handler = Handler {
        transport,
        terminal_state,
        client_assumed_state,
        pty: pty_setup,
    };

    handler.run().await
}

async fn bind_transport(
    bind_port: u16,
    desired_ip: Option<&str>,
    key: &Base64Key,
) -> Result<Transport, Box<dyn std::error::Error>> {
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
                log::warn!("Failed to bind to {}: {}, trying 0.0.0.0", ip, e);
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

    Ok(Transport::new_server(connection))
}

async fn wait_for_client(
    transport: &mut Transport,
) -> Result<(Complete, PtySize), Box<dyn std::error::Error>> {
    let mut terminal_state = Complete::new(80, 24)?;
    let mut pty_size = PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    };

    loop {
        match transport.recv_diff().await {
            Ok(Some(diff)) => {
                log::info!(
                    "Wait loop received diff: {} bytes (old={}, new={})",
                    diff.diff.len(),
                    diff.old_num,
                    diff.new_num
                );
                if !diff.diff.is_empty() {
                    if let Ok(msg) = UserMessage::decode(diff.diff.as_slice()) {
                        log::info!(
                            "Decoded UserMessage with {} instructions",
                            msg.instruction.len()
                        );
                        for inst in &msg.instruction {
                            if let Some(ref resize) = inst.resize {
                                pty_size = PtySize {
                                    rows: resize.height.unwrap_or(24) as u16,
                                    cols: resize.width.unwrap_or(80) as u16,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                };
                                terminal_state =
                                    Complete::new(pty_size.cols, pty_size.rows)?;
                                log::info!(
                                    "Client initial resize: {}x{}",
                                    pty_size.cols,
                                    pty_size.rows
                                );
                            }
                        }
                    }
                }
                log::info!("Wait loop breaking, proceeding to spawn PTY");
                break;
            }
            Ok(None) => continue,
            Err(e) => {
                log::warn!("Error waiting for client: {e}");
                continue;
            }
        }
    }

    Ok((terminal_state, pty_size))
}

impl Handler {
    async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        log::info!("Entering serve loop");

        let mut keepalive_timer = tokio::time::interval(Duration::from_millis(3000));
        keepalive_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let result = loop {
            tokio::select! {
                Some(pty_event) = self.pty.rx.recv() => {
                    if self.handle_pty_event(pty_event).await == ControlFlow::Break(()) {
                        break Ok(());
                    }
                }
                result = self.transport.recv_diff() => {
                    if self.handle_network_receive(result).await == ControlFlow::Break(()) {
                        break Ok(());
                    }
                }
                _ = keepalive_timer.tick() => {
                    if !self.transport.connection().has_remote() {
                        continue;
                    }
                    if self.handle_keepalive().await == ControlFlow::Break(()) {
                        break Ok(());
                    }
                }
            }
        };

        self.shutdown().await;
        result
    }

    async fn handle_pty_event(&mut self, event: PtyEvent) -> ControlFlow<()> {
        match event {
            PtyEvent::Output(data) => {
                log::info!("PTY output: {} bytes", data.len());
                self.terminal_state.apply_string(&data);

                let diff = self.terminal_state.diff_from(&self.client_assumed_state);
                log::info!(
                    "Diff from state: {} bytes, state_num={}",
                    diff.len(),
                    self.transport.sender.state_num()
                );
                if diff.is_empty() {
                    return ControlFlow::Continue(());
                }

                let ack_num = self.transport.receiver.remote_state_num();
                let throwaway = self.transport.sender.throwaway_num();
                log::info!("Sending diff: ack_num={}, throwaway={}", ack_num, throwaway);

                if let Err(e) = self.transport.send_diff(diff, ack_num, throwaway).await {
                    log::warn!("Send error: {e}");
                } else {
                    self.client_assumed_state = self.terminal_state.snapshot();
                    self.transport.sender.advance_state();
                    log::info!(
                        "Sent diff OK, state_num now={}",
                        self.transport.sender.state_num()
                    );
                }
                ControlFlow::Continue(())
            }
            PtyEvent::Exited(status) => {
                log::info!("Shell exited with status: {}", status.exit_code());
                self.send_remaining_diff().await;
                ControlFlow::Break(())
            }
        }
    }

    async fn handle_network_receive(
        &mut self,
        result: Result<Option<ReceivedDiff>, String>,
    ) -> ControlFlow<()> {
        match result {
            Ok(Some(diff)) => {
                if !diff.diff.is_empty() {
                    if let Ok(user_msg) = UserMessage::decode(diff.diff.as_slice()) {
                        self.apply_client_instructions(&user_msg);
                    } else {
                        log::warn!("Failed to decode UserMessage");
                    }
                }

                if self.transport.receiver.shutdown_received() {
                    log::info!("Client sent shutdown");
                    return ControlFlow::Break(());
                }

                if let Err(e) = self.pty.writer.flush() {
                    log::warn!("PTY flush error: {e}");
                }
            }
            Ok(None) => {}
            Err(e) => {
                log::warn!("Recv error: {e}");
            }
        }
        ControlFlow::Continue(())
    }

    fn apply_client_instructions(&mut self, user_msg: &UserMessage) {
        for inst in &user_msg.instruction {
            if let Some(ref keystroke) = inst.keystroke {
                self.write_keystrokes(keystroke);
            }
            if let Some(ref resize) = inst.resize {
                self.apply_resize(resize);
            }
        }
    }

    fn write_keystrokes(&mut self, keystroke: &morsh_proto::client::Keystroke) {
        let Some(ref keys) = keystroke.keys else {
            return;
        };
        for &byte in keys {
            if let Err(e) = self.pty.writer.write_all(&[byte]) {
                log::warn!("PTY write error: {e}");
            }
        }
    }

    fn apply_resize(&mut self, resize: &morsh_proto::client::ResizeMessage) {
        let w = resize.width.unwrap_or(80) as u16;
        let h = resize.height.unwrap_or(24) as u16;
        let _ = self.pty.master.resize(PtySize {
            rows: h,
            cols: w,
            pixel_width: 0,
            pixel_height: 0,
        });
        match Complete::new(w, h) {
            Ok(new_state) => self.terminal_state = new_state,
            Err(e) => log::warn!("Failed to create terminal state: {e}"),
        }
        log::info!("Client resize: {w}x{h}");
    }

    async fn handle_keepalive(&mut self) -> ControlFlow<()> {
        if let Ok(Some(status)) = self.pty.child.try_wait() {
            log::info!("Child process exited with status {}", status.exit_code());
            self.send_remaining_diff().await;
            return ControlFlow::Break(());
        }

        let now = std::time::Instant::now();
        if now
            .duration_since(self.transport.sender.last_send_time())
            .as_millis()
            > 2000
        {
            let ack_num = self.transport.receiver.remote_state_num();
            if let Err(e) = self.transport.send_ack(ack_num).await {
                log::debug!("Keepalive ACK error: {e}");
            }
        }

        ControlFlow::Continue(())
    }

    async fn send_remaining_diff(&mut self) {
        let diff = self.terminal_state.diff_from(&self.client_assumed_state);
        if !diff.is_empty() {
            let ack_num = self.transport.receiver.remote_state_num();
            let throwaway = self.transport.sender.throwaway_num();
            let _ = self.transport.send_diff(diff, ack_num, throwaway).await;
            self.transport.sender.advance_state();
        }
    }

    async fn shutdown(&mut self) {
        log::info!("Sending shutdown marker");
        let _ = self.transport.send_shutdown().await;
        log::info!("Shutting down");
        let _ = self.pty.child.kill();
    }
}
