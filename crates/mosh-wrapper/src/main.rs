use clap::Parser;
use std::io::{BufRead, BufReader};
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

    /// Remote user@host
    host: String,

    /// Command to run on remote (default: shell)
    command: Vec<String>,
}

/// Parsed MOSH CONNECT line from server.
struct ConnectInfo {
    address: String,
    port: u16,
    key: String,
}

fn main() {
    env_logger::init();

    let args = Args::parse();

    // Determine the command to run on remote
    let remote_cmd = if args.command.is_empty() {
        // No command specified - server will launch the user's shell
        "mosh-server".to_string()
    } else {
        // User specified a command - pass it to mosh-server
        let cmd = args.command.join(" ");
        format!("mosh-server -e {cmd}")
    };

    // Build SSH command
    let mut ssh_args = Vec::new();
    // Split the SSH command string (e.g., "ssh -o StrictHostKeyChecking=no")
    for arg in args.ssh_command.split_whitespace() {
        ssh_args.push(arg.to_string());
    }
    // Add the host
    ssh_args.push(args.host.clone());
    // Add the remote command
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

        // Log all lines for debugging
        log::debug!("SSH stdout: {line}");

        // Parse "MOSH CONNECT <IP> <PORT> <KEY>"
        if line.starts_with("MOSH CONNECT") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 {
                connect_info = Some(ConnectInfo {
                    address: parts[2].to_string(),
                    port: parts[3].parse().expect("Invalid port in MOSH CONNECT"),
                    key: parts[4].to_string(),
                });
                break;
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

    log::info!("Connecting to {}:{} with key {}", info.address, info.port, &info.key[..8]);

    // Set environment variables for mosh-client
    let client_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("mosh-client")))
        .unwrap_or_else(|| "mosh-client".into());

    // Exec mosh-client
    let exit_status = Command::new(&client_path)
        .arg(format!("{}:{}", info.address, info.port))
        .env("MOSH_KEY", &info.key)
        .status()
        .expect("Failed to start mosh-client. Is it in PATH?");

    std::process::exit(exit_status.code().unwrap_or(1));
}
