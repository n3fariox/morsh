use clap::Parser;
use std::io::{BufRead, BufReader};
use std::net::ToSocketAddrs;
use std::process::{Command, Stdio};

/// Morsh: mobile (rust) shell - connects to a remote host via SSH and starts a morsh-server.
#[derive(Parser, Debug)]
#[command(name = "morsh", about = "Remote, stateless, mobile (rust) shell")]
struct Args {
    /// SSH command to use (default: "ssh")
    #[arg(long = "ssh", default_value = "ssh")]
    ssh_command: String,

    /// UDP port range for morsh-server (e.g., "60000:61000")
    #[arg(long = "port")]
    port_range: Option<String>,

    /// Predict the future (enable prediction engine)
    #[arg(long = "predict", default_value = "adaptive")]
    predict: String,

    /// Control the IP address that morsh-server binds to:
    ///   ssh  - bind to the IP from SSH_CONNECTION (default)
    ///   any  - bind to 0.0.0.0
    ///   IP   - bind to the specified IP address
    #[arg(long = "bind-server", default_value = "ssh")]
    bind_server: String,

    /// Path on the remote host for morsh-server debug logs
    #[arg(long = "server-log-file")]
    server_log_file: Option<String>,

    /// Run morsh-server in foreground (don't daemonize)
    #[arg(long = "no-daemonize", short = 'D')]
    no_daemonize: bool,

    /// Path to morsh-server on the remote host (default: morsh-server, looked up in PATH)
    #[arg(long = "server", short = 'S', default_value = "morsh-server")]
    server_path: String,

    /// Remote user@host
    host: String,

    /// Command to run on remote (default: shell)
    command: Vec<String>,
}

/// Parsed MOSH CONNECT line from server.
struct ConnectInfo {
    port: u16,
    key: String,
}

fn main() {
    env_logger::init();

    let args = Args::parse();

    // Determine the server bind IP from the hostname or user-provided value.
    // This IP is used both for telling the server where to bind and for
    // the client's connection target, so they always agree.
    let resolved_ip = match args.bind_server.as_str() {
        "any" => None,
        "ssh" => Some(resolve_host(&args.host)),
        ip => Some(ip.to_string()),
    };

    // Try the configured server path first, then fall back to stock mosh-server.
    let mut server_paths = vec![args.server_path.as_str()];
    if args.server_path == "morsh-server" {
        server_paths.push("mosh-server");
    }

    let mut info: Option<ConnectInfo> = None;
    for (i, &path) in server_paths.iter().enumerate() {
        if i > 0 {
            eprintln!("morsh-server not found, trying {path}...");
        }
        info = try_launch_server(
            &args.ssh_command,
            &args.host,
            path,
            &resolved_ip,
            &args.server_log_file,
            args.no_daemonize,
            &args.command,
        );
        if info.is_some() {
            break;
        }
    }

    let info = info.unwrap_or_else(|| {
        eprintln!("Failed to get MOSH CONNECT from remote server");
        eprintln!("Make sure morsh-server or mosh-server is installed on the remote host");
        std::process::exit(1);
    });

    // Use the resolved IP for the client connection (same IP the server bound to)
    let server_ip = resolved_ip.unwrap_or_else(|| resolve_host(&args.host));

    log::info!("Connecting to {}:{} with key {}", server_ip, info.port, &info.key[..8]);

    let client_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("morsh-client")))
        .unwrap_or_else(|| "morsh-client".into());

    let exit_status = Command::new(&client_path)
        .arg(format!("{}:{}", server_ip, info.port))
        .env("MORSH_KEY", &info.key)
        .status()
        .expect("Failed to start morsh-client. Is it in PATH?");

    std::process::exit(exit_status.code().unwrap_or(1));
}

/// Try to launch a mosh server on the remote host via SSH and parse the MOSH CONNECT line.
fn try_launch_server(
    ssh_command: &str,
    host: &str,
    server_path: &str,
    resolved_ip: &Option<String>,
    server_log_file: &Option<String>,
    no_daemonize: bool,
    command: &[String],
) -> Option<ConnectInfo> {
    let mut parts: Vec<String> = Vec::new();
    parts.push(server_path.to_string());
    parts.push("new".to_string());

    if let Some(ref ip) = resolved_ip {
        parts.push("-i".to_string());
        parts.push(ip.clone());
    } else {
        parts.push("-s".to_string());
    }

    // Locale vars from client (like stock mosh, before --)
    let locale_vars = ["LANG", "LC_CTYPE", "LC_NUMERIC", "LC_TIME", "LC_COLLATE",
        "LC_MONETARY", "LC_MESSAGES", "LC_PAPER", "LC_NAME", "LC_ADDRESS",
        "LC_TELEPHONE", "LC_MEASUREMENT", "LC_IDENTIFICATION", "LC_ALL"];
    for var in &locale_vars {
        if let Ok(val) = std::env::var(var) {
            parts.push("-l".to_string());
            parts.push(format!("{var}={val}"));
        }
    }

    if let Some(ref path) = server_log_file {
        parts.push("--log-file".to_string());
        parts.push(path.clone());
    }

    if no_daemonize {
        parts.push("-D".to_string());
    }

    if !command.is_empty() {
        parts.push("--".to_string());
        parts.extend(command.iter().cloned());
    }

    let mut remote_cmd = parts.join(" ");

    if let Ok(val) = std::env::var("RUST_LOG") {
        remote_cmd = format!("RUST_LOG={val} {remote_cmd}");
    }

    let mut ssh_args: Vec<String> = ssh_command.split_whitespace().map(String::from).collect();
    ssh_args.push(host.to_string());
    ssh_args.push(remote_cmd);

    log::info!("SSH: {ssh_command} {host}");
    log::info!("Remote command: {}", ssh_args.last().unwrap());

    let mut ssh_child = Command::new(&ssh_args[0])
        .args(&ssh_args[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;

    let stdout = ssh_child.stdout.take()?;
    let reader = BufReader::new(stdout);

    let mut connect_info = None;

    for line in reader.lines() {
        let line = line.ok()?;
        log::debug!("SSH stdout: {line}");

        if line.starts_with("MOSH CONNECT") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 {
                if let Ok(port) = parts[2].parse::<u16>() {
                    connect_info = Some(ConnectInfo {
                        port,
                        key: parts[3].to_string(),
                    });
                    break;
                }
            }
        }
    }

    let _ = ssh_child.wait();
    connect_info
}

/// Resolve a hostname to an IP address via DNS.
fn resolve_host(host: &str) -> String {
    let hostname = host.split('@').next_back().unwrap_or(host);
    let hostname = hostname.split(':').next().unwrap_or(hostname);

    log::info!("Resolving hostname: {hostname}");

    let addrs = format!("{hostname}:0")
        .to_socket_addrs()
        .unwrap_or_else(|e| {
            eprintln!("Failed to resolve hostname '{hostname}': {e}");
            std::process::exit(1);
        });

    // Prefer IPv4 for compatibility
    for addr in addrs {
        if addr.ip().is_ipv4() {
            return addr.ip().to_string();
        }
    }

    // Fall back to whatever we got
    let addrs: Vec<_> = format!("{hostname}:0")
        .to_socket_addrs()
        .expect("No addresses found")
        .collect();

    if let Some(addr) = addrs.first() {
        return addr.ip().to_string();
    }

    eprintln!("No addresses found for '{hostname}'");
    std::process::exit(1);
}
