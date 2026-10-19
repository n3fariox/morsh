mod cli;
mod daemon;
mod pty;
mod server;

use morsh_crypto::Base64Key;
use morsh_log::LogGuard;
use morsh_network::{PORT_RANGE_HIGH, PORT_RANGE_LOW};
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

/// Find a free UDP port on `ip` within the requested range.
///
/// `low == 0` means no range was requested, so use mosh's default
/// 60001-60999 rather than an OS-assigned ephemeral port that firewall rules
/// for mosh won't cover.  Mirrors mosh, which walks the range and takes the
/// first port it can bind.
fn pick_port(ip: &str, low: u16, high: u16) -> Result<u16, String> {
    let addr_for = |port: u16| -> SocketAddr {
        match ip.parse::<std::net::IpAddr>() {
            Ok(parsed) => SocketAddr::new(parsed, port),
            Err(_) => SocketAddr::new("0.0.0.0".parse().unwrap(), port),
        }
    };

    let (low, high) = if low == 0 {
        (PORT_RANGE_LOW, PORT_RANGE_HIGH)
    } else {
        (low, high)
    };

    let mut last_err = String::new();
    for port in low..=high {
        match std::net::UdpSocket::bind(addr_for(port)) {
            Ok(sock) => return sock.local_addr().map(|a| a.port()).map_err(|e| e.to_string()),
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(format!("Failed to bind to any port in {low}:{high}: {last_err}"))
}

fn init_logging(log_file: &Option<String>) -> LogGuard {
    match log_file {
        Some(path) => morsh_log::init_at("morsh-server", &std::path::PathBuf::from(path)),
        None => morsh_log::init("morsh-server"),
    }
}

fn main() {
    let opts = cli::parse();

    let is_daemon_child = env::var("MORSH_DAEMON_CHILD").is_ok();

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
    let desired_ip = opts.bind_addr.clone().or(ssh_ip);

    // The daemon child inherits the port its parent already advertised.
    let port = if is_daemon_child {
        env::var("MORSH_DAEMON_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0)
    } else {
        match pick_port(
            desired_ip.as_deref().unwrap_or("0.0.0.0"),
            opts.port_low,
            opts.port_high,
        ) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("morsh-server: {e}");
                std::process::exit(1);
            }
        }
    };

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

    if !is_daemon_child {
        println!("MOSH CONNECT {} {}", port, key.printable_key());
        println!("MOSH CONNECTION ID: 1");
        std::io::stdout().flush().unwrap();
        // Like stock mosh-server's banner; the wrapper shows stderr when the
        // connect fails, so this says which build the remote actually ran.
        eprintln!("morsh-server (morsh) {}", env!("CARGO_PKG_VERSION"));
    }

    // The appender guard must live for the whole of `main`; binding it inside
    // an `if` would drop it at the end of the block and silently discard every
    // log record written after startup.
    let mut _log_guard: Option<LogGuard> = None;

    if opts.no_daemonize || is_daemon_child {
        if is_daemon_child {
            daemon::redirect_stdio();
        }

        _log_guard = Some(init_logging(&opts.log_file));

        if is_daemon_child {
            tracing::info!("Daemon child mode, stdio redirected to NUL, logging to file");
        } else {
            tracing::info!("No-daemonize mode, skipping fork and stdio redirect");
        }
    } else {
        env::set_var("MORSH_DAEMON_CHILD", "1");
        env::set_var("MORSH_DAEMON_PORT", port.to_string());
        env::set_var("MORSH_KEY", key.printable_key());

        if daemon::fork_and_detach(port) {
            _log_guard = Some(init_logging(&opts.log_file));
            tracing::info!("Child process continuing after fork, detaching from SSH");

            eprintln!("morsh-server: logging to ~/.morsh/logs/morsh-server.log");
            daemon::redirect_stdio();
            tracing::info!("Stdio redirected to /dev/null");
        }
    }

    tracing::info!(
        "morsh-server {} starting on port {port} (PID: {})",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    if let Err(e) = rt.block_on(server::run_server(
        port,
        desired_ip.as_deref(),
        key,
        shell,
        opts.command_args,
        opts.locale_vars,
    )) {
        tracing::error!("Server error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without `-p`, the server used to bind port 0 and land in the OS
    /// ephemeral range (~49152+ on Windows), outside mosh firewall rules.
    #[test]
    fn no_requested_range_uses_moshs_default() {
        let port = pick_port("127.0.0.1", 0, 0).unwrap();
        assert!((PORT_RANGE_LOW..=PORT_RANGE_HIGH).contains(&port), "{port}");
    }

    #[test]
    fn an_explicit_range_is_honoured() {
        let port = pick_port("127.0.0.1", 61500, 61510).unwrap();
        assert!((61500..=61510).contains(&port), "{port}");
    }
}
