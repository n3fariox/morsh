use mosh_crypto::{Base64Key, Session};
use mosh_network::{Connection, Transport};
use mosh_statesync::Complete;
use portable_pty::{CommandBuilder, ExitStatus, PtySize};
use std::env;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;

/// Events from the PTY reader thread.
enum PtyEvent {
    Output(Vec<u8>),
    Exited(ExitStatus),
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // Parse command line
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: mosh-server [options] [command]");
        eprintln!("  Options:");
        eprintln!("    -p PORT     Bind to specific port");
        eprintln!("    -a ADDRESS  Bind address (default: 0.0.0.0)");
        eprintln!("  Environment:");
        eprintln!("    MOSH_KEY    Base64 key (auto-generated if not set)");
        std::process::exit(1);
    }

    // Parse options
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

    // Determine shell command
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

    // Generate or read key
    let key = if let Ok(key_str) = env::var("MOSH_KEY") {
        Base64Key::from_printable(&key_str)
            .map_err(|e| format!("Invalid MOSH_KEY: {e}"))?
    } else {
        Base64Key::random()
    };

    // Create crypto session
    let session = Session::new(*key.data());

    // Create server connection
    let bind_addr: SocketAddr = format!("{bind_addr}:{bind_port}").parse()
        .map_err(|e| format!("Invalid bind address: {e}"))?;
    let connection = Connection::new_server(bind_addr, session).await
        .map_err(|e| format!("Failed to bind: {e}"))?;

    let local_addr = connection.remote_addr().unwrap_or_else(|| {
        // If no remote yet, use the bound address
        bind_addr
    });

    // Print connection info for the wrapper to parse
    println!(
        "MOSH CONNECT {} {} {}",
        local_addr.ip(),
        local_addr.port(),
        key.printable_key()
    );
    println!("MOSH CONNECTION ID: 1");

    // Flush stdout so the wrapper can read the connection info
    std::io::stdout().flush()?;

    log::info!("Server started, waiting for client at {local_addr}");

    // Create transport (server side)
    let mut transport = Transport::new_server(connection);

    // Create terminal state
    let mut terminal_state = Complete::new(80, 24)?;

    // Spawn PTY
    let pty_size = PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    };

    let mut cmd = CommandBuilder::new(&shell);
    // Set environment variables for the child process
    cmd.env("TERM", "xterm-256color");
    cmd.env("LANG", "en_US.UTF-8");

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system.openpty(pty_size)
        .map_err(|e| format!("Failed to open PTY: {e}"))?;

    // Spawn the child process
    let mut child = pair.slave.spawn_command(cmd)
        .map_err(|e| format!("Failed to spawn shell: {e}"))?;

    log::info!("Spawned shell: {shell}");

    // Create channels for PTY output
    let (pty_tx, mut pty_rx) = mpsc::channel::<PtyEvent>(64);

    // Get the reader from the PTY master
    let mut pty_reader = pair.master.try_clone_reader()
        .map_err(|e| format!("Failed to clone PTY reader: {e}"))?;

    // Spawn PTY reader thread
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

    // Get the PTY writer
    let mut pty_writer = pair.master.take_writer()
        .map_err(|e| format!("Failed to get PTY writer: {e}"))?;

    // State tracking
    let mut client_assumed_state = Complete::new(80, 24)?;
    let mut last_send_time = std::time::Instant::now();
    let send_interval = Duration::from_millis(50);

    log::info!("Entering serve loop");

    // Main serve loop
    let result: Result<(), Box<dyn std::error::Error>> = loop {
        tokio::select! {
            // PTY output → compute diff → send to client
            Some(pty_event) = pty_rx.recv() => {
                match pty_event {
                    PtyEvent::Output(data) => {
                        // Apply PTY output to terminal state
                        terminal_state.apply_string(&data);

                        // Compute diff from what client last saw
                        let diff = terminal_state.diff_from(&client_assumed_state);

                        if !diff.is_empty() && last_send_time.elapsed() >= send_interval {
                            // Send diff to client
                            let ack_num = 0; // TODO: track user stream acks
                            let throwaway = transport.sender.throwaway_num();
                            if let Err(e) = transport.send_diff(diff, ack_num, throwaway).await {
                                log::warn!("Send error: {e}");
                            } else {
                                client_assumed_state = terminal_state.clone();
                                transport.sender.advance_state();
                                last_send_time = std::time::Instant::now();
                            }
                        }
                    }
                    PtyEvent::Exited(status) => {
                        log::info!("Shell exited with status: {}", status.exit_code());
                        break Ok(());
                    }
                }
            }

            // Network receive → apply to PTY
            result = transport.recv_diff() => {
                match result {
                    Ok(Some(diff)) => {
                        // Apply keystrokes/resize to PTY
                        for byte in &diff.diff {
                            if let Err(e) = pty_writer.write_all(&[*byte]) {
                                log::warn!("PTY write error: {e}");
                            }
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

            // Periodic: send pending diffs
            _ = tokio::time::sleep(Duration::from_millis(10)) => {
                // Check if there's a pending diff to send
                let diff = terminal_state.diff_from(&client_assumed_state);
                if !diff.is_empty() && last_send_time.elapsed() >= send_interval {
                    let ack_num = 0;
                    let throwaway = transport.sender.throwaway_num();
                    if let Err(e) = transport.send_diff(diff, ack_num, throwaway).await {
                        log::warn!("Send error: {e}");
                    } else {
                        client_assumed_state = terminal_state.clone();
                        transport.sender.advance_state();
                        last_send_time = std::time::Instant::now();
                    }
                }
            }
        }
    };

    // Cleanup
    log::info!("Shutting down");
    let _ = child.kill();
    result
}
