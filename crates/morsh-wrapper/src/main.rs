use clap::Parser;
use std::io::{BufRead, BufReader, Read};
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

/// Resolve the shell value specified via `--remote-shell`.
/// `None` means "auto" — let the SSH backend detect it.
fn resolve_remote_shell_value(value: &str) -> Option<RemoteShell> {
    match value {
        "auto" => None,
        "posix" | "sh" | "bash" => Some(RemoteShell::Posix),
        "cmd" | "cmd.exe" => Some(RemoteShell::Cmd),
        "powershell" | "pwsh" => Some(RemoteShell::PowerShell),
        other => {
            eprintln!(
                "error: unknown --remote-shell '{other}' (expected auto, posix, cmd, or powershell)"
            );
            std::process::exit(1);
        }
    }
}

fn resolve_remote_shell_system(ssh_command: &str, host: &str) -> RemoteShell {
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
    let _guard = morsh_log::init("morsh");

    let args = Args::parse();

    let resolved_ip = match args.bind_server.as_str() {
        "any" => None,
        "ssh" => Some(resolve_host(&args.host)),
        ip => Some(ip.to_string()),
    };

    #[cfg(feature = "russh")]
    let info = try_launch_ssh(&args, &resolved_ip).unwrap_or_else(|| {
        eprintln!("Failed to get MOSH CONNECT from remote server");
        eprintln!("Make sure morsh-server or mosh-server is installed on the remote host");
        std::process::exit(1);
    });

    #[cfg(not(feature = "russh"))]
    let info = try_launch_server(&args.ssh_command, &args.host, &{
        let locale_vars = collect_locale_vars();
        let rust_log = std::env::var("RUST_LOG").ok();
        let shell = resolve_remote_shell_value(&args.remote_shell)
            .unwrap_or_else(|| resolve_remote_shell_system(&args.ssh_command, &args.host));
        build_remote_cmd(&args, &resolved_ip, shell, rust_log, &locale_vars)
    }).unwrap_or_else(|| {
        eprintln!("Failed to get MOSH CONNECT from remote server");
        eprintln!("Make sure morsh-server or mosh-server is installed on the remote host");
        std::process::exit(1);
    });

    let server_ip = resolved_ip.unwrap_or_else(|| resolve_host(&args.host));

    tracing::info!("Connecting to {}:{} with key {}", server_ip, info.port, &info.key[..8]);

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

/// Collect locale environment variables (LANG, LC_*) from the local process.
/// These are used both as -l arguments to morsh-server and as export
/// statements for stock mosh-server on Posix remotes.
fn collect_locale_vars() -> Vec<(String, String)> {
    let names = [
        "LANG", "LC_CTYPE", "LC_NUMERIC", "LC_TIME", "LC_COLLATE",
        "LC_MONETARY", "LC_MESSAGES", "LC_PAPER", "LC_NAME", "LC_ADDRESS",
        "LC_TELEPHONE", "LC_MEASUREMENT", "LC_IDENTIFICATION", "LC_ALL",
    ];
    let mut vars = Vec::new();
    for name in &names {
        if let Ok(val) = std::env::var(name) {
            vars.push((name.to_string(), val));
        }
    }
    vars
}
fn build_remote_cmd(
    args: &Args,
    resolved_ip: &Option<String>,
    shell: RemoteShell,
    rust_log: Option<String>,
    locale_vars: &[(String, String)],
) -> String {
    if args.server_path == "morsh-server" {
        let first = build_server_args("morsh-server", resolved_ip, &args.server_log_file, args.no_daemonize, &args.command, locale_vars);
        let second = build_server_args("mosh-server", resolved_ip, &args.server_log_file, args.no_daemonize, &args.command, locale_vars);
        render_remote_command(shell, first, Some(second), rust_log, locale_vars)
    } else {
        let only = build_server_args(&args.server_path, resolved_ip, &args.server_log_file, args.no_daemonize, &args.command, locale_vars);
        render_remote_command(shell, only, None, rust_log, locale_vars)
    }
}

/// Build the argv for a single server invocation (no shell quoting yet).
fn build_server_args(
    server_path: &str,
    resolved_ip: &Option<String>,
    server_log_file: &Option<String>,
    no_daemonize: bool,
    command: &[String],
    locale_vars: &[(String, String)],
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

    for (name, val) in locale_vars {
        parts.push("-l".to_string());
        parts.push(format!("{name}={val}"));
    }

    if let Some(ref path) = server_log_file {
        parts.push("--log-file".to_string());
        parts.push(path.clone());
    }

    if no_daemonize {
        parts.push("-D".to_string());
    }

    if !command.is_empty() {
        parts.push("-e".to_string());
        parts.extend(command.iter().cloned());
    }

    parts
}

/// Render the full remote launch command for the given remote shell.
///
/// `first` is always run; when `second` is provided it is used as a
/// `locale_vars` is a list of (name, value) pairs for locale settings
/// (LANG, LC_*) that are exported on Posix remotes.
fn render_remote_command(
    shell: RemoteShell,
    first: Vec<String>,
    second: Option<Vec<String>>,
    rust_log: Option<String>,
    locale_vars: &[(String, String)],
) -> String {
    match shell {
        RemoteShell::Posix => {
            // Build locale export prefix.  stock mosh-server requires a
            // UTF-8 locale and gets confused when SSH doesn't forward any.
            let locale_export = if locale_vars.is_empty() {
                // No locale info from the local environment — C.UTF-8 is
                // available on glibc ≥ 2.37 (Debian 12+, Ubuntu 22.04+,
                // Fedora 34+, and most modern distros).
                "export LC_ALL=C.UTF-8".to_string()
            } else {
                let exports: Vec<String> = locale_vars.iter()
                    .filter(|(_, v)| !v.is_empty())
                    .map(|(n, v)| format!("{n}={v}"))
                    .collect();
                format!("export {}", exports.join(" "))
            };

            let first = first.join(" ");
            let base = match second {
                Some(second) => {
                    let second = second.join(" ");
                    if let Some(v) = &rust_log {
                        format!("(env RUST_LOG={v} {first} 2>/dev/null) || {second}")
                    } else {
                        format!("({first} 2>/dev/null) || {second}")
                    }
                }
                None => {
                    if let Some(v) = &rust_log {
                        format!("env RUST_LOG={v} {first}")
                    } else {
                        first
                    }
                }
            };
            format!("{locale_export}; {base}")
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

/// Run a remote command via system SSH and extract MOSH CONNECT info from stdout.
fn try_launch_server(
    ssh_command: &str,
    host: &str,
    remote_cmd: &str,
) -> Option<ConnectInfo> {
    let mut ssh_args: Vec<String> = ssh_command.split_whitespace().map(String::from).collect();
    ssh_args.push(host.to_string());
    ssh_args.push(remote_cmd.to_string());

    tracing::info!("SSH: {ssh_command} {host}");
    tracing::info!("Remote command: {}", ssh_args.last().unwrap());

    let mut ssh_child = Command::new(&ssh_args[0])
        .args(&ssh_args[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;

    let stdout = ssh_child.stdout.take()?;
    let stderr = ssh_child.stderr.take()?;
    let reader = BufReader::new(stdout);

    let mut connect_info = None;

    for line in reader.lines() {
        let line = line.ok()?;
        tracing::debug!("SSH stdout: {line}");

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

    // If we never found the MOSH CONNECT line, the SSH command likely
    // failed. Surface its stderr so the user can see what went wrong.
    if connect_info.is_none() {
        let mut err = String::new();
        if BufReader::new(stderr).read_to_string(&mut err).is_ok() && !err.trim().is_empty() {
            eprint!("{err}");
        }
    }

    connect_info
}

fn resolve_host(host: &str) -> String {
    let hostname = host.split('@').next_back().unwrap_or(host);
    let hostname = hostname.split(':').next().unwrap_or(hostname);

    tracing::info!("Resolving hostname: {hostname}");

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
fn try_launch_ssh(args: &Args, resolved_ip: &Option<String>) -> Option<ConnectInfo> {
    let mode = args.ssh_mode;

    if mode == SshMode::Russh || mode == SshMode::Auto {
        if let Some(ref key_path) = find_default_key() {
            tracing::info!("Connecting via russh (key: {})", key_path.display());
            match try_launch_russh(args, resolved_ip, key_path) {
                Some(info) => return Some(info),
                None if mode == SshMode::Russh => return None,
                None => tracing::info!("russh connection failed, falling back to system ssh"),
            }
        } else if mode == SshMode::Russh {
            eprintln!("No SSH private key found; use --ssh-mode system to use system ssh");
            return None;
        } else {
            tracing::info!("No SSH private key found, using system ssh");
        }
    }

    tracing::info!("using system ssh");
    let locale_vars = collect_locale_vars();
    let rust_log = std::env::var("RUST_LOG").ok();
    let shell = resolve_remote_shell_value(&args.remote_shell)
        .unwrap_or_else(|| resolve_remote_shell_system(&args.ssh_command, &args.host));
    let remote_cmd = build_remote_cmd(args, resolved_ip, shell, rust_log, &locale_vars);
    try_launch_server(&args.ssh_command, &args.host, &remote_cmd)
}

#[cfg(feature = "russh")]
fn try_launch_russh(args: &Args, resolved_ip: &Option<String>, key_path: &std::path::Path) -> Option<ConnectInfo> {
    use morsh_ssh::SshSession;

    let (user, hostname) = parse_user_host(&args.host);

    let port = hostname
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .unwrap_or(22);

    let addr = format!("{hostname}:{port}");

    tracing::info!("russh: connecting to {addr} as {user}");

    let mut session = SshSession::connect(addr, user, key_path).ok()?;

    // Probe the remote shell using the same session.
    let shell = match resolve_remote_shell_value(&args.remote_shell) {
        Some(s) => s,
        None => {
            let (exit_code, probe_out) = session.exec_raw("uname -s").ok()?;
            if exit_code == 0 && !probe_out.is_empty() {
                RemoteShell::Posix
            } else {
                RemoteShell::Cmd
            }
        }
    };

    let rust_log = std::env::var("RUST_LOG").ok();
    let locale_vars = collect_locale_vars();
    let remote_cmd = build_remote_cmd(args, resolved_ip, shell, rust_log, &locale_vars);

    let connect_info = std::sync::Arc::new(std::sync::Mutex::new(None::<ConnectInfo>));
    let connect_info_clone = connect_info.clone();

    let exit_code = session
        .exec_with_handler(
            &remote_cmd,
            move |line: String| {
                tracing::debug!("russh stdout: {line}");
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

    tracing::info!("russh: remote command exited with code {exit_code}");
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
