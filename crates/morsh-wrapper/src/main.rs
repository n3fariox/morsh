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
        "any" => None,  // server will bind to 0.0.0.0, client uses resolved hostname
        "ssh" => Some(resolve_host(&args.host)),
        ip => Some(ip.to_string()),
    };

    // Determine the command to run on remote
    let server_name = &args.server_path;
    let remote_cmd = if args.command.is_empty() {
        server_name.clone()
    } else {
        let cmd = args.command.join(" ");
        format!("{server_name} -e {cmd}")
    };

    // Add bind-server flag to the remote command
    let remote_cmd = if let Some(ref ip) = resolved_ip {
        format!("{remote_cmd} -a {ip}")
    } else {
        format!("{remote_cmd} -s")
    };

    // Add server log file if specified
    let remote_cmd = if let Some(ref path) = args.server_log_file {
        format!("{remote_cmd} -l {path}")
    } else {
        remote_cmd
    };

    // Add no-daemonize flag if specified
    let remote_cmd = if args.no_daemonize {
        format!("{remote_cmd} -D")
    } else {
        remote_cmd
    };

    // Forward RUST_LOG to remote so server debug logs appear on stderr (with -D)
    let remote_cmd = if let Ok(val) = std::env::var("RUST_LOG") {
        format!("RUST_LOG={val} {remote_cmd}")
    } else {
        remote_cmd
    };

    // Build SSH command
    let mut ssh_args: Vec<String> = args.ssh_command.split_whitespace().map(String::from).collect();
    ssh_args.push(args.host.clone());
    ssh_args.push(remote_cmd);

    log::info!("SSH: {} {}", args.ssh_command, args.host);
    log::info!("Remote command: {}", ssh_args.last().unwrap());

    // Spawn SSH process
    let mut ssh_child = Command::new(&ssh_args[0])
        .args(&ssh_args[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .stdin(Stdio::null())
        .spawn()
        .expect("Failed to start SSH. Is ssh installed?");

    // Read stdout to find "MOSH CONNECT" line
    let stdout = ssh_child.stdout.take().expect("Failed to capture SSH stdout");
    let reader = BufReader::new(stdout);

    let mut connect_info: Option<ConnectInfo> = None;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Error reading SSH output: {e}");
                std::process::exit(1);
            }
        };

        log::debug!("SSH stdout: {line}");

        if line.starts_with("MOSH CONNECT") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            // Stock mosh format: MOSH CONNECT port key
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

    let info = match connect_info {
        Some(i) => i,
        None => {
            eprintln!("Failed to get MOSH CONNECT from remote server");
            eprintln!("Make sure morsh-server is installed on the remote host");
            std::process::exit(1);
        }
    };

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

/// Resolve a hostname to an IP address via DNS.
fn resolve_host(host: &str) -> String {
    let hostname = host.split('@').last().unwrap_or(host);
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
