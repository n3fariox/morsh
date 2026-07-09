use mosh_crypto::{Base64Key, Session};
use mosh_network::{Connection, Transport};
use mosh_network::transport::SendState;
use mosh_proto::client::UserMessage;
use mosh_statesync::Complete;
use portable_pty::{CommandBuilder, ExitStatus, PtySize};
use prost::Message;
use std::env;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;

enum PtyEvent {
    Output(Vec<u8>),
    Exited(ExitStatus),
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let args: Vec<String> = env::args().collect();

    let mut bind_port: u16 = 0;
    let mut bind_addr = "0.0.0.0".to_string();
    let mut command_args: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                i += 1;
                bind_port = args[i].parse().map_err(|e| format!("Invalid port: {e}"))?;
            }
            "-a" => {
                i += 1;
                bind_addr = args[i].clone();
            }
            _ => {
                command_args.push(args[i].clone());
            }
        }
        i += 1;
    }

    let shell = if command_args.is_empty() {
        env::var("SHELL").unwrap_or_else(|_| {
            if cfg!(windows) {
                "cmd.exe".to_string()
            } else {
                "/bin/sh".to_string()
            }
        })
    } else {
        command_args[0].clone()
    };

    let key = if let Ok(key_str) = env::var("MOSH_KEY") {
        Base64Key::from_printable(&key_str)
            .map_err(|e| format!("Invalid MOSH_KEY: {e}"))?
    } else {
        Base64Key::random()
    };

    let session = Session::new(*key.data());

    let bind_addr: SocketAddr = format!("{bind_addr}:{bind_port}").parse()
        .map_err(|e| format!("Invalid bind address: {e}"))?;
    let connection = Connection::new_server(bind_addr, session).await
        .map_err(|e| format!("Failed to bind: {e}"))?;

    let local_addr = connection.remote_addr().unwrap_or(bind_addr);

    println!(
        "MOSH CONNECT {} {} {}",
        local_addr.ip(),
        local_addr.port(),
        key.printable_key()
    );
    println!("MOSH CONNECTION ID: 1");
    std::io::stdout().flush()?;

    log::info!("Server started, waiting for client at {local_addr}");

    // Create transport and activate it immediately
    let mut transport = Transport::new_server(connection);
    transport.sender.set_state(SendState::Active);

    let mut terminal_state = Complete::new(80, 24)?;

    let pty_size = PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    };

    let mut cmd = CommandBuilder::new(&shell);
    cmd.env("TERM", "xterm-256color");
    cmd.env("LANG", "en_US.UTF-8");

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system.openpty(pty_size)
        .map_err(|e| format!("Failed to open PTY: {e}"))?;

    let mut child = pair.slave.spawn_command(cmd)
        .map_err(|e| format!("Failed to spawn shell: {e}"))?;

    log::info!("Spawned shell: {shell}");

    let (pty_tx, mut pty_rx) = mpsc::channel::<PtyEvent>(64);

    let mut pty_reader = pair.master.try_clone_reader()
        .map_err(|e| format!("Failed to clone PTY reader: {e}"))?;

    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) => {
                    let _ = pty_tx.blocking_send(PtyEvent::Exited(
                        ExitStatus::with_exit_code(0)
                    ));
                    break;
                }
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    if pty_tx.blocking_send(PtyEvent::Output(data)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut pty_writer = pair.master.take_writer()
        .map_err(|e| format!("Failed to get PTY writer: {e}"))?;

    let mut client_assumed_state = Complete::new(80, 24)?;

    log::info!("Entering serve loop");

    // Keepalive timer: send empty ACKs every 3s when idle (like stock mosh)
    let mut keepalive_timer = tokio::time::interval(Duration::from_millis(3000));
    keepalive_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let result: Result<(), Box<dyn std::error::Error>> = loop {
        tokio::select! {
            // PTY output → compute diff → send to client
            Some(pty_event) = pty_rx.recv() => {
                match pty_event {
                    PtyEvent::Output(data) => {
                        terminal_state.apply_string(&data);

                        // Use transport's should_send for adaptive timing
                        let now = std::time::Instant::now();
                        let diff = terminal_state.diff_from(&client_assumed_state);
                        if !diff.is_empty() && transport.should_send(now) {
                            let ack_num = transport.receiver.remote_state_num();
                            let throwaway = transport.sender.throwaway_num();
                            if let Err(e) = transport.send_diff(diff, ack_num, throwaway).await {
                                log::warn!("Send error: {e}");
                            } else {
                                client_assumed_state = terminal_state.clone();
                                transport.sender.advance_state();
                            }
                        }
                    }
                    PtyEvent::Exited(status) => {
                        log::info!("Shell exited with status: {}", status.exit_code());
                        // Send final state + shutdown
                        let diff = terminal_state.diff_from(&client_assumed_state);
                        if !diff.is_empty() {
                            let ack_num = transport.receiver.remote_state_num();
                            let throwaway = transport.sender.throwaway_num();
                            let _ = transport.send_diff(diff, ack_num, throwaway).await;
                            client_assumed_state = terminal_state.clone();
                            transport.sender.advance_state();
                        }
                        break Ok(());
                    }
                }
            }

            // Network receive → apply to PTY
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
                                            let _ = pair.master.resize(portable_pty::PtySize {
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

                        // Shutdown was detected by recv_diff() internally
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

            // Keepalive: send empty ACK to keep connection alive
            _ = keepalive_timer.tick() => {
                // Don't send keepalives until client has connected
                if !transport.connection().has_remote() {
                    continue;
                }
                let now = std::time::Instant::now();
                // Only send keepalive if we haven't sent anything recently
                if now.duration_since(transport.sender.last_send_time()).as_millis() > 2000 {
                    let ack_num = transport.receiver.remote_state_num();
                    if let Err(e) = transport.send_ack(ack_num).await {
                        log::debug!("Keepalive ACK error: {e}");
                    }
                }
            }
        }
    };

    // Send shutdown marker to client
    log::info!("Sending shutdown marker");
    let ack_num = transport.receiver.remote_state_num();
    let _ = transport.send_shutdown(ack_num).await;

    log::info!("Shutting down");
    let _ = child.kill();
    result
}
