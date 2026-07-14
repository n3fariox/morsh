use clap::Parser;
use std::io::{BufRead, BufReader};
use std::net::ToSocketAddrs;
use std::process::{Command, Stdio};

/// Which SSH backend to use for connecting to the remote host.
#[cfg_attr(not(feature = "russh"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq)]
enum SshMode {
    /// Try russh (pure Rust), fall back to system ssh if no private key is found.
    Auto,
    /// Use the system ssh command.
    System,
    /// Use russh (pure Rust). Error if no private key is available.
    Russh,
}

impl std::str::FromStr for SshMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(SshMode::Auto),
            "russh" => Ok(SshMode::Russh),
            "system" => Ok(SshMode::System),
            other => Err(format!(
                "unknown SSH mode '{other}'. Use 'auto', 'russh', or 'system'"
            )),
        }
    }
}

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

    /// SSH backend: "auto" (try russh, fall back to system ssh),
    /// "russh" (pure Rust), or "system" (system ssh command).
    /// Only available when compiled with the "russh" feature.
    #[cfg(feature = "russh")]
    #[arg(long = "ssh-mode", default_value = "auto")]
    ssh_mode: SshMode,

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

    let resolved_ip = match args.bind_server.as_str() {
        "any" => None,
        "ssh" => Some(resolve_host(&args.host)),
        ip => Some(ip.to_string()),
    };

    let remote_cmd = if args.server_path == "morsh-server" {
        let first = build_server_cmd("morsh-server", &resolved_ip, &args.server_log_file, args.no_daemonize, &args.command);
        let second = build_server_cmd("mosh-server", &resolved_ip, &args.server_log_file, args.no_daemonize, &args.command);
        format!("({first} 2>/dev/null) || {second}")
    } else {
        build_server_cmd(&args.server_path, &resolved_ip, &args.server_log_file, args.no_daemonize, &args.command)
    };

    #[cfg(feature = "russh")]
    let info = try_launch_ssh(&args, &remote_cmd).unwrap_or_else(|| {
        eprintln!("Failed to get MOSH CONNECT from remote server");
        eprintln!("Make sure morsh-server or mosh-server is installed on the remote host");
        std::process::exit(1);
    });

    #[cfg(not(feature = "russh"))]
    let info = try_launch_server(&args.ssh_command, &args.host, &remote_cmd).unwrap_or_else(|| {
        eprintln!("Failed to get MOSH CONNECT from remote server");
        eprintln!("Make sure morsh-server or mosh-server is installed on the remote host");
        std::process::exit(1);
    });

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

fn build_server_cmd(
    server_path: &str,
    resolved_ip: &Option<String>,
    server_log_file: &Option<String>,
    no_daemonize: bool,
    command: &[String],
) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(server_path.to_string());
    parts.push("new".to_string());

    if let Some(ref ip) = resolved_ip {
        parts.push("-i".to_string());
        parts.push(ip.clone());
    } else {
        parts.push("-s".to_string());
    }

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

    parts.join(" ")
}

/// Run a remote command via system SSH and extract MOSH CONNECT info from stdout.
fn try_launch_server(
    ssh_command: &str,
    host: &str,
    remote_cmd: &str,
) -> Option<ConnectInfo> {
    let mut remote_cmd = remote_cmd.to_string();

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

    for addr in addrs {
        if addr.ip().is_ipv4() {
            return addr.ip().to_string();
        }
    }

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

// ── russh (pure-Rust) backend ──────────────────────────────────────────────────

#[cfg(feature = "russh")]
fn try_launch_ssh(args: &Args, remote_cmd: &str) -> Option<ConnectInfo> {
    let mode = args.ssh_mode;

    if mode == SshMode::Russh || mode == SshMode::Auto {
        if let Some(ref key_path) = find_default_key() {
            log::info!("Connecting via russh (key: {})", key_path.display());
            match try_launch_russh(&args.host, remote_cmd, key_path) {
                Some(info) => return Some(info),
                None if mode == SshMode::Russh => return None,
                None => log::info!("russh connection failed, falling back to system ssh"),
            }
        } else if mode == SshMode::Russh {
            eprintln!("No SSH private key found; use --ssh-mode system to use system ssh");
            return None;
        } else {
            log::info!("No SSH private key found, using system ssh");
        }
    }

    log::info!("using system ssh");
    try_launch_server(&args.ssh_command, &args.host, remote_cmd)
}

#[cfg(feature = "russh")]
fn try_launch_russh(host: &str, remote_cmd: &str, key_path: &std::path::Path) -> Option<ConnectInfo> {
    use morsh_ssh::SshSession;

    let (user, hostname) = parse_user_host(host);

    let port = hostname
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .unwrap_or(22);

    let addr = format!("{hostname}:{port}");

    log::info!("russh: connecting to {addr} as {user}");

    let mut modified_cmd = remote_cmd.to_string();
    if let Ok(val) = std::env::var("RUST_LOG") {
        modified_cmd = format!("RUST_LOG={val} {modified_cmd}");
    }

    let mut session = SshSession::connect(addr, user, key_path).ok()?;

    let connect_info = std::sync::Arc::new(std::sync::Mutex::new(None::<ConnectInfo>));
    let connect_info_clone = connect_info.clone();

    let exit_code = session
        .exec_with_handler(
            &modified_cmd,
            move |line: String| {
                log::debug!("russh stdout: {line}");
                if line.starts_with("MOSH CONNECT") {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 4 {
                        if let Ok(port) = parts[2].parse::<u16>() {
                            let mut guard = connect_info_clone.lock().unwrap();
                            *guard = Some(ConnectInfo {
                                port,
                                key: parts[3].to_string(),
                            });
                        }
                    }
                }
            },
        )
        .ok()?;

    log::info!("russh: remote command exited with code {exit_code}");
    let result = connect_info.lock().unwrap().take();
    result
}

#[cfg(feature = "russh")]
fn parse_user_host(s: &str) -> (String, String) {
    if let Some(at_pos) = s.rfind('@') {
        let user = s[..at_pos].to_string();
        let host = s[at_pos + 1..].to_string();
        (user, host)
    } else {
        ("root".to_string(), s.to_string())
    }
}

#[cfg(feature = "russh")]
fn tilde_expand(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

#[cfg(feature = "russh")]
fn find_default_key() -> Option<std::path::PathBuf> {
    let candidates = [
        "~/.ssh/id_ed25519",
        "~/.ssh/id_rsa",
        "~/.ssh/id_ecdsa",
        "~/.ssh/identity",
    ];
    for pattern in &candidates {
        let path = tilde_expand(pattern);
        let path = std::path::PathBuf::from(&path);
        if path.exists() {
            return Some(path);
        }
    }
    None
}
