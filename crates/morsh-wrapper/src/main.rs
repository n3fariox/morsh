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

    /// Shell on the remote host that interprets the launch command.
    /// auto (detect), posix (sh/bash), cmd (Windows cmd.exe), or powershell (pwsh).
    /// Default: auto.
    #[arg(long = "remote-shell", default_value = "auto")]
    remote_shell: String,
}

/// Shell used on the remote host to interpret the launch command.
#[derive(Clone, Copy, PartialEq)]
enum RemoteShell {
    Posix,
    Cmd,
    PowerShell,
}

fn resolve_remote_shell(value: &str, ssh_command: &str, host: &str) -> RemoteShell {
    match value {
        "auto" => detect_remote_shell(ssh_command, host),
        "posix" | "sh" | "bash" => RemoteShell::Posix,
        "cmd" | "cmd.exe" => RemoteShell::Cmd,
        "powershell" | "pwsh" => RemoteShell::PowerShell,
        other => {
            eprintln!(
                "error: unknown --remote-shell '{other}' (expected auto, posix, cmd, or powershell)"
            );
            std::process::exit(1);
        }
    }
}

/// Detect the remote shell with a cheap probe.
///
/// `uname` exists on every Unix-like remote but not on Windows, so we use it
/// to distinguish POSIX from Windows. OpenSSH for Windows defaults its shell
/// to cmd.exe, so a detected Windows remote is rendered as Cmd (pass
/// `--remote-shell powershell` explicitly if the remote's default shell is
/// PowerShell). If the probe itself fails we fall back to Posix, since that is
/// the most common remote.
fn detect_remote_shell(ssh_command: &str, host: &str) -> RemoteShell {
    let mut args: Vec<String> = ssh_command.split_whitespace().map(String::from).collect();
    args.push(host.to_string());
    args.push("uname -s".to_string());

    match Command::new(&args[0]).args(&args[1..]).output() {
        Ok(out) if out.status.success() && !out.stdout.is_empty() => RemoteShell::Posix,
        _ => RemoteShell::Cmd,
    }
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

    // The launch command is interpreted by the *remote* shell, which may be
    // POSIX sh/bash on Linux, or cmd.exe / PowerShell on a Windows OpenSSH host.
    // Build the command in the syntax that matches the remote shell.
    let shell = resolve_remote_shell(&args.remote_shell, &args.ssh_command, &args.host);
    let rust_log = std::env::var("RUST_LOG").ok();

    // Build a single remote command that tries morsh-server first,
    // then falls back to mosh-server — all in one SSH connection.
    let remote_cmd = if args.server_path == "morsh-server" {
        let first = build_server_args("morsh-server", &resolved_ip, &args.server_log_file, args.no_daemonize, &args.command);
        let second = build_server_args("mosh-server", &resolved_ip, &args.server_log_file, args.no_daemonize, &args.command);
        render_remote_command(shell, first, Some(second), rust_log)
    } else {
        let only = build_server_args(&args.server_path, &resolved_ip, &args.server_log_file, args.no_daemonize, &args.command);
        render_remote_command(shell, only, None, rust_log)
    };

    let info = try_launch_server(&args.ssh_command, &args.host, &remote_cmd).unwrap_or_else(|| {
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

/// Build the argv for a single server invocation (no shell quoting yet).
fn build_server_args(
    server_path: &str,
    resolved_ip: &Option<String>,
    server_log_file: &Option<String>,
    no_daemonize: bool,
    command: &[String],
) -> Vec<String> {
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

    parts
}

/// Render the full remote launch command for the given remote shell.
///
/// `first` is always run; when `second` is provided it is used as a
/// fallback if `first` exits non-zero. `rust_log`, if present, sets the
/// RUST_LOG environment variable for the server process.
fn render_remote_command(
    shell: RemoteShell,
    first: Vec<String>,
    second: Option<Vec<String>>,
    rust_log: Option<String>,
) -> String {
    match shell {
        RemoteShell::Posix => {
            let first = first.join(" ");
            let base = match second {
                Some(second) => format!("({first} 2>/dev/null) || {}", second.join(" ")),
                None => first,
            };
            match rust_log {
                Some(v) => format!("RUST_LOG={v} {base}"),
                None => base,
            }
        }
        RemoteShell::Cmd => {
            let first = first.iter().map(|s| cmd_quote(s)).collect::<Vec<_>>().join(" ");
            let run = match second {
                Some(second) => {
                    let second = second.iter().map(|s| cmd_quote(s)).collect::<Vec<_>>().join(" ");
                    format!("({first} 2>nul) || {second}")
                }
                None => first,
            };
            match rust_log {
                Some(v) => format!("set \"RUST_LOG={v}\" && ({run})"),
                None => run,
            }
        }
        RemoteShell::PowerShell => {
            let first = first.iter().map(|s| pwsh_quote(s)).collect::<Vec<_>>().join(" ");
            let run = match second {
                Some(second) => {
                    let second = second.iter().map(|s| pwsh_quote(s)).collect::<Vec<_>>().join(" ");
                    format!("& {first} 2>$null; if ($LASTEXITCODE -ne 0) {{ & {second} }}")
                }
                None => format!("& {first}"),
            };
            match rust_log {
                Some(v) => format!("$env:RUST_LOG='{}'; {run}", v.replace('\'', "''")),
                None => run,
            }
        }
    }
}

/// Quote an argument for cmd.exe (double quotes only when needed).
fn cmd_quote(s: &str) -> String {
    if s.contains(' ') || s.contains('\t') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Quote an argument for PowerShell (single-quoted literal string).
fn pwsh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Run a remote command via SSH and extract MOSH CONNECT info from stdout.
fn try_launch_server(
    ssh_command: &str,
    host: &str,
    remote_cmd: &str,
) -> Option<ConnectInfo> {
    let mut ssh_args: Vec<String> = ssh_command.split_whitespace().map(String::from).collect();
    ssh_args.push(host.to_string());
    ssh_args.push(remote_cmd.to_string());

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
