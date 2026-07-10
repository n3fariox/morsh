use clap::Parser;
use std::io::{BufRead, BufReader};
use std::net::ToSocketAddrs;
use std::process::{Command, Stdio};

/// Mosh: mobile shell - connects to a remote host via SSH and starts a mosh-server.
#[derive(Parser, Debug)]
#[command(name = "mosh", about = "Remote, stateless, mobile shell")]
struct Args {
    /// SSH command to use (default: "ssh")
    #[arg(long = "ssh", default_value = "ssh")]
    ssh_command: String,

    /// UDP port range for mosh-server (e.g., "60000:61000")
    #[arg(long = "port")]
    port_range: Option<String>,

    /// Predict the future (enable prediction engine)
    #[arg(long = "predict", default_value = "adaptive")]
    predict: String,

    /// Control the IP address that mosh-server binds to:
    ///   ssh  - bind to the IP from SSH_CONNECTION (default)
    ///   any  - bind to 0.0.0.0
    ///   IP   - bind to the specified IP address
    #[arg(long = "bind-server", default_value = "ssh")]
    bind_server: String,

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

    // Determine the command to run on remote
    let remote_cmd = if args.command.is_empty() {
        "mosh-server".to_string()
    } else {
        let cmd = args.command.join(" ");
        format!("mosh-server -e {cmd}")
    };

    // Add bind-server flag to the remote command
    let remote_cmd = match args.bind_server.as_str() {
        "any" => format!("{remote_cmd} -a 0.0.0.0"),
        "ssh" => format!("{remote_cmd} -s"),
        ip => format!("{remote_cmd} -a {ip}"),
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
            eprintln!("Make sure mosh-server is installed on the remote host");
            std::process::exit(1);
        }
    };

    // Resolve the hostname to an IP address for the client to connect to
    let server_ip = resolve_host(&args.host);

    log::info!("Connecting to {}:{} with key {}", server_ip, info.port, &info.key[..8]);

    let client_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("mosh-client")))
        .unwrap_or_else(|| "mosh-client".into());

    let exit_status = Command::new(&client_path)
        .arg(format!("{}:{}", server_ip, info.port))
        .env("MOSH_KEY", &info.key)
        .status()
        .expect("Failed to start mosh-client. Is it in PATH?");

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
