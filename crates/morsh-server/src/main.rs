use morsh_crypto::{Base64Key, Session};
use morsh_network::{Connection, Transport};
use morsh_network::transport::SendState;
use morsh_proto::client::UserMessage;
use morsh_statesync::Complete;
use morsh_terminal::ScreenSnapshot;
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

/// Parse the SSH_CONNECTION environment variable to get the server's IP.
/// SSH_CONNECTION format: "client_ip client_port server_ip server_port"
/// Returns the server_ip (3rd field), or None if not available.
fn get_ssh_connection_ip() -> Option<String> {
    let ssh_conn = env::var("SSH_CONNECTION").ok()?;
    let parts: Vec<&str> = ssh_conn.split_whitespace().collect();
    if parts.len() >= 3 {
        let server_ip = parts[2].to_string();
        if let Some(stripped) = server_ip.strip_prefix("::ffff:") {
            return Some(stripped.to_string());
        }
        return Some(server_ip);
    }
    None
}

/// Fork and create a new session (Unix) or detach from console (Windows).
/// Returns true in the child process, false in the parent.
fn fork_and_detach() -> bool {
    #[cfg(unix)]
    {
        use std::process;

        match unsafe { libc::fork() } {
            -1 => {
                eprintln!("morsh-server: fork failed");
                std::process::exit(1);
            }
            0 => {
                // Child: create new session to detach from SSH terminal
                unsafe { libc::setsid(); }
                true
            }
            pid => {
                // Parent: wait for child to exit, then exit ourselves
                unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0); }
                std::process::exit(0);
            }
        }
    }
    #[cfg(windows)]
    {
        // Windows: detach from the SSH console
        extern "system" {
            fn FreeConsole() -> i32;
        }
        unsafe { FreeConsole(); }
        true
    }
    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

/// Redirect stdio to /dev/null (Unix) or NUL (Windows) after daemonizing.
#[cfg(unix)]
fn redirect_stdio() {
    use std::os::unix::io::AsRawFd;

    let devnull = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .expect("Failed to open /dev/null");

    unsafe {
        libc::dup2(devnull.as_raw_fd(), 0);
        libc::dup2(devnull.as_raw_fd(), 1);
        libc::dup2(devnull.as_raw_fd(), 2);
    }
}

#[cfg(windows)]
fn redirect_stdio() {
    use std::os::windows::io::AsRawHandle;

    let nul = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("NUL")
        .expect("Failed to open NUL");

    let handle = nul.as_raw_handle();

    unsafe {
        windows_sys::Win32::System::Console::SetStdHandle(
            windows_sys::Win32::System::Console::STD_INPUT_HANDLE,
            handle,
        );
        windows_sys::Win32::System::Console::SetStdHandle(
            windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE,
            handle,
        );
        windows_sys::Win32::System::Console::SetStdHandle(
            windows_sys::Win32::System::Console::STD_ERROR_HANDLE,
            handle,
        );
    }

    std::mem::forget(nul);
}

fn main() {
    env_logger::init();

    let args: Vec<String> = env::args().collect();

    let mut bind_port: u16 = 0;
    let mut bind_addr: Option<String> = None;
    let mut command_args: Vec<String> = Vec::new();
    let mut use_ssh_connection = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("morsh-server: -p requires a port argument");
                    std::process::exit(1);
                }
                bind_port = args[i].parse().map_err(|e| format!("Invalid port: {e}")).unwrap_or_else(|e| {
                    eprintln!("morsh-server: {e}");
                    std::process::exit(1);
                });
            }
            "-a" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("morsh-server: -a requires an address argument");
                    std::process::exit(1);
                }
                bind_addr = Some(args[i].clone());
            }
            "-s" => {
                use_ssh_connection = true;
            }
            "-e" => {
                i += 1;
                while i < args.len() && !args[i].starts_with('-') {
                    command_args.push(args[i].clone());
                    i += 1;
                }
                continue;
            }
            "-h" | "--help" => {
                eprintln!("Usage: morsh-server [options] [command]");
                eprintln!();
                eprintln!("Starts a morsh-server that listens for a morsh-client connection.");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  -p PORT     Bind to this port (default: random)");
                eprintln!("  -a ADDR     Bind to this address (default: 0.0.0.0)");
                eprintln!("  -s          Use SSH_CONNECTION to determine bind IP (default)");
                eprintln!("  -e CMD...   Execute command instead of shell");
                eprintln!("  -h, --help  Show this help message");
                eprintln!("  -v, --version  Show version information");
                eprintln!();
                eprintln!("Environment:");
                eprintln!("  MORSH_KEY    Encryption key (auto-generated if not set)");
                eprintln!("  SHELL       Shell to run (default: /bin/sh)");
                eprintln!();
                eprintln!("The server prints 'MOSH CONNECT <port> <key>' to stdout");
                eprintln!("when ready, which is used by the morsh wrapper to start the client.");
                std::process::exit(0);
            }
            "-v" | "--version" => {
                eprintln!("morsh-server (morsh) {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
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

    let key = if let Ok(key_str) = env::var("MORSH_KEY") {
        Base64Key::from_printable(&key_str)
            .unwrap_or_else(|e| {
                eprintln!("morsh-server: Invalid MORSH_KEY: {e}");
                std::process::exit(1);
            })
    } else {
        Base64Key::random()
    };

    // Determine bind address
    let ssh_ip = if use_ssh_connection || bind_addr.is_none() {
        get_ssh_connection_ip()
    } else {
        None
    };
    let desired_ip = bind_addr.as_deref().or(ssh_ip.as_deref());

    // Try binding to get a port, then fork
    let addr: SocketAddr = if let Some(ip) = desired_ip {
        format!("{}:{}", ip, bind_port).parse().unwrap_or_else(|_| {
            format!("0.0.0.0:{}", bind_port).parse().unwrap()
        })
    } else {
        format!("0.0.0.0:{}", bind_port).parse().unwrap()
    };

    // Create UDP socket to determine the port
    let sock = std::net::UdpSocket::bind(addr)
        .unwrap_or_else(|e| {
            // Fall back to 0.0.0.0
            let fallback: SocketAddr = format!("0.0.0.0:{}", bind_port).parse().unwrap();
            eprintln!("morsh-server: Failed to bind to {addr}: {e}, trying {fallback}");
            std::net::UdpSocket::bind(fallback).unwrap_or_else(|e| {
                eprintln!("morsh-server: Failed to bind: {e}");
                std::process::exit(1);
            })
        });

    let port = sock.local_addr().unwrap().port();
    drop(sock); // Release the port; tokio will rebind

    // Print MORSH CONNECT before forking so the wrapper can read it
    println!("MOSH CONNECT {} {}", port, key.printable_key());
    println!("MOSH CONNECTION ID: 1");
    std::io::stdout().flush().unwrap();

    // Fork: parent exits, child detaches and runs the server
    if !fork_and_detach() {
        // Parent already exited via fork_and_detach
        unreachable!();
    }

    // Child: redirect stdio to /dev/null (Unix) or NUL (Windows)
    redirect_stdio();

    log::info!("Server starting on port {port}");

    // Create tokio runtime and run server
    let rt = tokio::runtime::Runtime::new().unwrap();
    if let Err(e) = rt.block_on(run_server(bind_port, desired_ip, key, shell)) {
        log::error!("Server error: {e}");
        std::process::exit(1);
    }
}

async fn run_server(
    bind_port: u16,
    desired_ip: Option<&str>,
    key: Base64Key,
    shell: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let session = Session::new(*key.data());

    // Bind to the port
    let connection = if let Some(ip) = desired_ip {
        let addr: SocketAddr = format!("{}:{}", ip, bind_port).parse()
            .map_err(|e| format!("Invalid bind address: {e}"))?;
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
    cmd.env("MORSH", env!("CARGO_PKG_VERSION"));

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

    let mut client_assumed_state: ScreenSnapshot = Complete::new(80, 24)?.snapshot();

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

                        let diff = terminal_state.diff_from(&client_assumed_state);
                        if !diff.is_empty() {
                            let ack_num = transport.receiver.remote_state_num();
                            let throwaway = transport.sender.throwaway_num();
                            if let Err(e) = transport.send_diff(diff, ack_num, throwaway).await {
                                log::warn!("Send error: {e}");
                            } else {
                                client_assumed_state = terminal_state.snapshot();
                                transport.sender.advance_state();
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
                            client_assumed_state = terminal_state.snapshot();
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
                if !transport.connection().has_remote() {
                    continue;
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

    // Send shutdown marker to client
    log::info!("Sending shutdown marker");
    let ack_num = transport.receiver.remote_state_num();
    let _ = transport.send_shutdown(ack_num).await;

    log::info!("Shutting down");
    let _ = child.kill();
    result
}
