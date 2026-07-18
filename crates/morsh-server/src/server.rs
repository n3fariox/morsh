use crate::pty::{self, PtyEvent};
use morsh_crypto::{Base64Key, Session};
use morsh_network::{Connection, Transport};
use morsh_network::transport::SendState;
use morsh_proto::client::UserMessage;
use morsh_statesync::Complete;
use morsh_terminal::ScreenSnapshot;
use portable_pty::PtySize;
use prost::Message;
use std::io::Write;
use std::net::SocketAddr;
use std::time::Duration;

pub async fn run_server(
    bind_port: u16,
    desired_ip: Option<&str>,
    key: Base64Key,
    shell: String,
    command_args: Vec<String>,
    locale_vars: Vec<(String, String)>,
) -> Result<(), Box<dyn std::error::Error>> {
    log::info!("run_server entered: port={bind_port}, shell={shell}, ip={:?}, args.len={}", desired_ip, command_args.len());
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
                Connection::new_server(fallback, Session::new(*key.data())).await
                    .map_err(|e| format!("Failed to bind: {e}"))?
            }
        }
    } else {
        let addr: SocketAddr = format!("0.0.0.0:{}", bind_port).parse().unwrap();
        Connection::new_server(addr, session).await
            .map_err(|e| format!("Failed to bind: {e}"))?
    };

    let mut transport = Transport::new_server(connection);
    transport.sender.set_state(SendState::Active);

    log::info!("UDP transport ready, waiting for client on port {bind_port}");

    let mut terminal_state = Complete::new(80, 24)?;
    let mut pty_size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
    loop {
        match transport.recv_diff().await {
                Ok(Some(diff)) => {
                log::info!("Wait loop received diff: {} bytes (old={}, new={})",
                    diff.diff.len(), diff.old_num, diff.new_num);
                if !diff.diff.is_empty() {
                    match UserMessage::decode(diff.diff.as_slice()) {
                        Ok(msg) => {
                            log::info!("Decoded UserMessage with {} instructions", msg.instruction.len());
                            for inst in &msg.instruction {
                                if let Some(ref resize) = inst.resize {
                                    pty_size = PtySize {
                                        rows: resize.height.unwrap_or(24) as u16,
                                        cols: resize.width.unwrap_or(80) as u16,
                                        pixel_width: 0,
                                        pixel_height: 0,
                                    };
                                    terminal_state = Complete::new(pty_size.cols, pty_size.rows)?;
                                    log::info!("Client initial resize: {}x{}", pty_size.cols, pty_size.rows);
                                }
                            }
                        }
                        Err(e) => {
                            log::warn!("Failed to decode first client message: {e}");
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

    log::info!("Client connected, spawning shell (remote_state_num={})",
        transport.receiver.remote_state_num());

    let pty_setup = pty::spawn_pty(pty_size, &shell, &command_args, &locale_vars)?;

    let mut pty_rx = pty_setup.rx;
    let mut child = pty_setup.child;
    let pair_master = pty_setup.master;
    let mut pty_writer = pty_setup.writer;

    let mut client_assumed_state: ScreenSnapshot = Complete::new(80, 24)?.snapshot();

    log::info!("Entering serve loop");

    let mut keepalive_timer = tokio::time::interval(Duration::from_millis(3000));
    keepalive_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let result: Result<(), Box<dyn std::error::Error>> = loop {
        tokio::select! {
            Some(pty_event) = pty_rx.recv() => {
                match pty_event {
                    PtyEvent::Output(data) => {
                        log::info!("PTY output: {} bytes", data.len());
                        terminal_state.apply_string(&data);

                        let diff = terminal_state.diff_from(&client_assumed_state);
                        log::info!("Diff from state: {} bytes, state_num={}", diff.len(), transport.sender.state_num());
                        if !diff.is_empty() {
                            let ack_num = transport.receiver.remote_state_num();
                            let throwaway = transport.sender.throwaway_num();
                            log::info!("Sending diff: ack_num={}, throwaway={}", ack_num, throwaway);
                            if let Err(e) = transport.send_diff(diff, ack_num, throwaway).await {
                                log::warn!("Send error: {e}");
                            } else {
                                client_assumed_state = terminal_state.snapshot();
                                transport.sender.advance_state();
                                log::info!("Sent diff OK, state_num now={}", transport.sender.state_num());
                            }
                        }
                    }
                    PtyEvent::Exited(status) => {
                        log::info!("Shell exited with status: {}", status.exit_code());
                        let diff = terminal_state.diff_from(&client_assumed_state);
                        if !diff.is_empty() {
                            let ack_num = transport.receiver.remote_state_num();
                            let throwaway = transport.sender.throwaway_num();
                            let _ = transport.send_diff(diff, ack_num, throwaway).await;
                            transport.sender.advance_state();
                        }
                        let ack_num = transport.receiver.remote_state_num();
                        let _ = transport.send_shutdown(ack_num).await;
                        break Ok(());
                    }
                }
            }

            result = transport.recv_diff() => {
                match result {
                    Ok(Some(diff)) => {
                        if !diff.diff.is_empty() {
                            match UserMessage::decode(diff.diff.as_slice()) {
                                Ok(user_msg) => {
                                    for inst in &user_msg.instruction {
                                        if let Some(ref keystroke) = inst.keystroke {
                                            if let Some(ref keys) = keystroke.keys {
                                                for &byte in keys {
                                                    if let Err(e) = pty_writer.write_all(&[byte]) {
                                                        log::warn!("PTY write error: {e}");
                                                    }
                                                }
                                            }
                                        }
                                        if let Some(ref resize) = inst.resize {
                                            let w = resize.width.unwrap_or(80) as u16;
                                            let h = resize.height.unwrap_or(24) as u16;
                                            let _ = pair_master.resize(PtySize {
                                                rows: h,
                                                cols: w,
                                                pixel_width: 0,
                                                pixel_height: 0,
                                            });
                                            terminal_state = Complete::new(w, h)?;
                                            log::info!("Client resize: {w}x{h}");
                                        }
                                    }
                                }
                                Err(e) => {
                                    log::warn!("Failed to decode UserMessage: {e}");
                                }
                            }
                        }

                        if transport.receiver.shutdown_received() {
                            log::info!("Client sent shutdown");
                            break Ok(());
                        }

                        if let Err(e) = pty_writer.flush() {
                            log::warn!("PTY flush error: {e}");
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log::warn!("Recv error: {e}");
                    }
                }
            }

            _ = keepalive_timer.tick() => {
                if !transport.connection().has_remote() {
                    continue;
                }

                if let Ok(Some(status)) = child.try_wait() {
                    log::info!("Child process exited with status {}", status.exit_code());
                    let diff = terminal_state.diff_from(&client_assumed_state);
                    if !diff.is_empty() {
                        let ack_num = transport.receiver.remote_state_num();
                        let throwaway = transport.sender.throwaway_num();
                        let _ = transport.send_diff(diff, ack_num, throwaway).await;
                        transport.sender.advance_state();
                    }
                    let ack_num = transport.receiver.remote_state_num();
                    let _ = transport.send_shutdown(ack_num).await;
                    break Ok(());
                }

                let now = std::time::Instant::now();
                if now.duration_since(transport.sender.last_send_time()).as_millis() > 2000 {
                    let ack_num = transport.receiver.remote_state_num();
                    if let Err(e) = transport.send_ack(ack_num).await {
                        log::debug!("Keepalive ACK error: {e}");
                    }
                }
            }
        }
    };

    log::info!("Sending shutdown marker");
    let ack_num = transport.receiver.remote_state_num();
    let _ = transport.send_shutdown(ack_num).await;

    log::info!("Shutting down");
    let _ = child.kill();
    result
}
