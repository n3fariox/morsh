mod cli;
mod daemon;
mod pty;
mod server;

use morsh_crypto::Base64Key;
use std::env;
use std::io::Write;
use std::net::SocketAddr;

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

fn main() {
    let opts = cli::parse();

    let is_daemon_child = env::var("MORSH_DAEMON_CHILD").is_ok();
    let mut bind_port = opts.port;
    if is_daemon_child {
        if let Ok(port_str) = env::var("MORSH_DAEMON_PORT") {
            if let Ok(port_val) = port_str.parse::<u16>() {
                bind_port = port_val;
            }
        }
    }

    let shell = if opts.command_args.is_empty() {
        env::var("SHELL").unwrap_or_else(|_| {
            if cfg!(windows) {
                "cmd.exe".to_string()
            } else {
                "/bin/sh".to_string()
            }
        })
    } else {
        opts.command_args[0].clone()
    };

    let key = if let Ok(key_str) = env::var("MORSH_KEY") {
        Base64Key::from_printable(&key_str).unwrap_or_else(|e| {
            eprintln!("morsh-server: Invalid MORSH_KEY: {e}");
            std::process::exit(1);
        })
    } else {
        Base64Key::random()
    };

    let ssh_ip = if opts.use_ssh_connection || opts.bind_addr.is_none() {
        get_ssh_connection_ip()
    } else {
        None
    };
    let desired_ip = opts.bind_addr.as_deref().or(ssh_ip.as_deref());

    let addr: SocketAddr = if let Some(ip) = desired_ip {
        match ip.parse::<std::net::IpAddr>() {
            Ok(ip) => SocketAddr::new(ip, bind_port),
            Err(_) => SocketAddr::new("0.0.0.0".parse().unwrap(), bind_port),
        }
    } else {
        SocketAddr::new("0.0.0.0".parse().unwrap(), bind_port)
    };

    let sock = std::net::UdpSocket::bind(addr).unwrap_or_else(|e| {
        let fallback: SocketAddr = format!("0.0.0.0:{}", bind_port).parse().unwrap();
        eprintln!("morsh-server: Failed to bind to {addr}: {e}, trying {fallback}");
        std::net::UdpSocket::bind(fallback).unwrap_or_else(|e| {
            eprintln!("morsh-server: Failed to bind: {e}");
            std::process::exit(1);
        })
    });

    let port = sock.local_addr().unwrap().port();
    drop(sock);

    if !is_daemon_child {
        println!("MOSH CONNECT {} {}", port, key.printable_key());
        println!("MOSH CONNECTION ID: 1");
        std::io::stdout().flush().unwrap();
        log::info!("MOSH CONNECT printed, preparing to daemonize (port {port})");
    }

    if opts.no_daemonize || is_daemon_child {
        if is_daemon_child {
            daemon::redirect_stdio();

            let log_path = opts.log_file.clone().unwrap_or_else(|| {
                let dir = std::env::temp_dir();
                dir.join(format!("morsh-server-{}.log", std::process::id()))
                    .to_string_lossy()
                    .to_string()
            });

            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
            {
                Ok(file) => {
                    env_logger::Builder::from_env(
                        env_logger::Env::default().default_filter_or("info"),
                    )
                    .target(env_logger::Target::Pipe(Box::new(file)))
                    .init();
                }
                Err(_) => {
                    env_logger::Builder::from_env(
                        env_logger::Env::default().default_filter_or("info"),
                    )
                    .init();
                }
            }
        } else {
            env_logger::Builder::from_env(
                env_logger::Env::default().default_filter_or("info"),
            )
            .init();
        }
        if is_daemon_child {
            log::info!("Daemon child mode, stdio redirected to NUL, logging to file");
        } else {
            log::info!("No-daemonize mode, skipping fork and stdio redirect");
        }
    } else {
        env::set_var("MORSH_DAEMON_CHILD", "1");
        env::set_var("MORSH_DAEMON_PORT", port.to_string());
        env::set_var("MORSH_KEY", key.printable_key());

        log::info!("Preparing to daemonize (port {port})");

        if daemon::fork_and_detach(port) {
            log::info!("Child process continuing after fork, detaching from SSH");

            let log_path = opts.log_file.clone().unwrap_or_else(|| {
                let dir = std::env::temp_dir();
                dir.join(format!("morsh-server-{}.log", std::process::id()))
                    .to_string_lossy()
                    .to_string()
            });

            let log_file = match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
            {
                Ok(file) => {
                    eprintln!("morsh-server: logging to {log_path}");
                    Some(file)
                }
                Err(e) => {
                    eprintln!("morsh-server: failed to open log file {log_path}: {e}");
                    None
                }
            };

            daemon::redirect_stdio();

            if let Some(file) = log_file {
                env_logger::Builder::from_env(
                    env_logger::Env::default().default_filter_or("info"),
                )
                .target(env_logger::Target::Pipe(Box::new(file)))
                .init();
            } else {
                env_logger::Builder::from_env(
                    env_logger::Env::default().default_filter_or("info"),
                )
                .init();
            }

            log::info!("Stdio redirected to /dev/null");
        }
    }

    log::info!("Server starting on port {port} (PID: {})", std::process::id());

    let rt = tokio::runtime::Runtime::new().unwrap();
    if let Err(e) = rt.block_on(server::run_server(
        port,
        desired_ip,
        key,
        shell,
        opts.command_args,
        opts.locale_vars,
    )) {
        log::error!("Server error: {e}");
        std::process::exit(1);
    }
}


