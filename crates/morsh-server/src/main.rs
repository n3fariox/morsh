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
        log::info!("Forking to daemonize...");

        match unsafe { libc::fork() } {
            -1 => {
                eprintln!("morsh-server: fork failed");
                std::process::exit(1);
            }
            0 => {
                // Child: create new session to detach from SSH terminal
                log::info!("Child forked OK, calling setsid()");
                unsafe { libc::setsid(); }
                log::info!("setsid() complete, child PID={}", std::process::id());
                true
            }
            pid => {
                // Parent: exit immediately — child continues as daemon
                log::info!("Parent (PID={}) exiting, child (PID={}) continues as daemon", std::process::id(), pid);
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

    log::info!("Redirecting stdio to /dev/null");
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
    let mut no_daemonize = false;
    let mut log_file_path: Option<String> = None;
    let mut locale_vars: Vec<(String, String)> = Vec::new();
    let mut i = 1;

    // Skip optional "new" subcommand (stock mosh compatibility)
    if i < args.len() && args[i] == "new" {
        i += 1;
    }

    let mut after_separator = false;
    while i < args.len() {
        if after_separator {
            command_args.push(args[i].clone());
            i += 1;
            continue;
        }

        match args[i].as_str() {
            "--" => {
                after_separator = true;
            }
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
            "-i" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("morsh-server: -i requires an address argument");
                    std::process::exit(1);
                }
                bind_addr = Some(args[i].clone());
            }
            "-s" => {
                use_ssh_connection = true;
            }
            "-D" | "--no-daemonize" => {
                no_daemonize = true;
            }
            "-l" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("morsh-server: -l requires NAME=VALUE");
                    std::process::exit(1);
                }
                let val = &args[i];
                if let Some((name, value)) = val.split_once('=') {
                    locale_vars.push((name.to_string(), value.to_string()));
                } else {
                    eprintln!("morsh-server: -l argument must be NAME=VALUE, got '{val}'");
                    std::process::exit(1);
                }
            }
            "--log-file" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("morsh-server: --log-file requires a file path");
                    std::process::exit(1);
                }
                log_file_path = Some(args[i].clone());
            }
            "-c" => {
                i += 1; // skip color count (ignored)
            }
            "-h" | "--help" => {
                eprintln!("Usage: morsh-server new [options] [-- command...]");
                eprintln!();
                eprintln!("Starts a morsh-server that listens for a morsh-client connection.");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  -p PORT[:PORT2]  Bind to this port/range (default: random)");
                eprintln!("  -i LOCALADDR   Bind to this address (default: 0.0.0.0)");
                eprintln!("  -s             Use SSH_CONNECTION for bind IP");
                eprintln!("  -l NAME=VALUE  Set locale-related environment variable");
                eprintln!("  -D, --no-daemonize  Run in foreground (morsh extension)");
                eprintln!("  --log-file FILE     Write logs to FILE (morsh extension)");
                eprintln!("  -c COLORS       Terminal color count (ignored)");
                eprintln!("  -h, --help      Show this help message");
                eprintln!("  -v, --version   Show version information");
                eprintln!();
                eprintln!("Environment:");
                eprintln!("  MORSH_KEY    Encryption key (auto-generated if not set)");
                eprintln!("  SHELL        Shell to run (default: /bin/sh)");
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
        match ip.parse::<std::net::IpAddr>() {
            Ok(ip) => SocketAddr::new(ip, bind_port),
            Err(_) => SocketAddr::new("0.0.0.0".parse().unwrap(), bind_port),
        }
    } else {
        SocketAddr::new("0.0.0.0".parse().unwrap(), bind_port)
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

    log::info!("MOSH CONNECT printed, preparing to daemonize (port {port})");

    if no_daemonize {
        log::info!("No-daemonize mode, skipping fork and stdio redirect");
    } else {
        // Fork: parent exits, child detaches and runs the server
        if !fork_and_detach() {
            // Parent already exited via fork_and_detach
            unreachable!();
        }

        log::info!("Child process continuing after fork, detaching from SSH");

        // Open log file BEFORE redirecting stdio, so errors are visible on stderr
        let log_file = log_file_path.as_ref().and_then(|path| {
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                Ok(file) => {
                    eprintln!("morsh-server: logging to {path}");
                    Some(file)
                }
                Err(e) => {
                    eprintln!("morsh-server: failed to open log file {path}: {e}");
                    None
                }
            }
        });

        // Child: redirect stdio to /dev/null (Unix) or NUL (Windows)
        redirect_stdio();

        // After redirect, dup log file onto stderr so env_logger output lands there
        if let Some(file) = log_file {
            #[cfg(unix)] {
                use std::os::unix::io::AsRawFd;
                unsafe { libc::dup2(file.as_raw_fd(), 2); }
            }
            #[cfg(windows)] {
                use std::os::windows::io::AsRawHandle;
                unsafe {
                    windows_sys::Win32::System::Console::SetStdHandle(
                        windows_sys::Win32::System::Console::STD_ERROR_HANDLE,
                        file.as_raw_handle(),
                    );
                }
            }
        }

        log::info!("Stdio redirected to /dev/null");
    }

    log::info!("Server starting on port {port} (PID: {})", std::process::id());

    // Create tokio runtime and run server
    let rt = tokio::runtime::Runtime::new().unwrap();
    if let Err(e) = rt.block_on(run_server(port, desired_ip, key, shell, command_args, locale_vars)) {
        log::error!("Server error: {e}");
        std::process::exit(1);
    }
}

async fn run_server(
    bind_port: u16,
    desired_ip: Option<&str>,
    key: Base64Key,
    shell: String,
    command_args: Vec<String>,
    locale_vars: Vec<(String, String)>,
) -> Result<(), Box<dyn std::error::Error>> {
    log::info!("run_server entered: port={bind_port}, shell={shell}, ip={:?}, args.len={}", desired_ip, command_args.len());
    let session = Session::new(*key.data());

    // Bind to the port
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

    // Create transport and activate it immediately
    let mut transport = Transport::new_server(connection);
    transport.sender.set_state(SendState::Active);

    log::info!("UDP transport ready, waiting for client on port {bind_port}");

    // Wait for the client to send its first packet before spawning the shell.
    // This ensures the server knows the client's address so PTY output isn't lost.
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

    let mut cmd = if command_args.is_empty() {
        // Interactive shell mode — spawn shell directly
        CommandBuilder::new(&shell)
    } else {
        // Command mode (-e CMD...) — run through sh -c like stock mosh
        let full_cmd = command_args.join(" ");
        let mut c = CommandBuilder::new("/bin/sh");
        c.arg("-c");
        c.arg(full_cmd);
        c
    };
    cmd.env("TERM", "xterm-256color");
    cmd.env("LANG", "en_US.UTF-8");
    cmd.env("MORSH", env!("CARGO_PKG_VERSION"));
    for (name, value) in &locale_vars {
        cmd.env(name, value);
    }

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system.openpty(pty_size)
        .map_err(|e| format!("Failed to open PTY: {e}"))?;

    let mut child = pair.slave.spawn_command(cmd)
        .map_err(|e| format!("Failed to spawn shell: {e}"))?;
    // Drop slave handle so we get EOF on the PTY master reader when the child exits.
    // Without this, the parent's open slave FD keeps the PTY alive and we never
    // detect that the child has exited.
    std::mem::drop(pair.slave);

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
