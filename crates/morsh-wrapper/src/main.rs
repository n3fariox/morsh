use clap::Parser;
use std::io::{BufRead, BufReader};
use std::net::ToSocketAddrs;
use std::process::{Command, Stdio};

mod banner;
mod fake_proxy;
mod ssh_config;
use ssh_config::SshConfig;

/// Which SSH backend to use for connecting to the remote host.
#[cfg_attr(not(feature = "russh"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq)]
enum SshMode {
    /// Use the system ssh command, or russh when no ssh is installed.
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
    #[arg(long = "predict", default_value = "adaptive", value_parser = ["adaptive", "always", "never"])]
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

    /// SSH backend: "auto" (system ssh, or russh when no ssh is installed),
    /// "system" (system ssh command), or "russh" (pure Rust).
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
#[derive(Clone, Copy, Debug, PartialEq)]
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

/// Run on the remote to tell its shell apart: PowerShell expands it to
/// `Desktop` or `Core`, cmd.exe echoes it verbatim, and a POSIX shell expands
/// the unset `$PSVersionTable` to nothing, leaving `.PSEdition`.
/// `uname -s` can't do this, since it fails under both Windows shells.
const SHELL_PROBE: &str = "echo $PSVersionTable.PSEdition";

/// Classify the remote shell from the output of [`SHELL_PROBE`].
///
/// A failed probe keeps the old answer of cmd.exe, which is what a Windows
/// server runs unless `DefaultShell` says otherwise.
fn classify_shell_probe(success: bool, stdout: &str) -> RemoteShell {
    let out = stdout.trim();
    if !success || out.is_empty() || out.contains("$PSVersionTable") {
        RemoteShell::Cmd
    } else if out == "Desktop" || out == "Core" {
        RemoteShell::PowerShell
    } else {
        RemoteShell::Posix
    }
}

/// Detect the remote shell for the system-ssh backend.
///
/// The server's SSH banner, read before any login, settles the common case:
/// anything but a Windows SSH server gets a POSIX shell, as mosh always
/// assumes. Only Windows servers, and hosts we can't reach directly, need a
/// second ssh login to ask.
fn resolve_remote_shell_system(ssh_command: &str, host: &str, cfg: Option<&SshConfig>) -> RemoteShell {
    if !cfg.is_some_and(|c| c.uses_proxy) {
        let hostname = target_hostname(host, cfg);
        let port = target_port(host, cfg);
        match banner::fetch(&hostname, port) {
            Some(b) if !banner::is_windows(&b) => {
                tracing::info!("SSH banner {b:?}: assuming a POSIX shell");
                return RemoteShell::Posix;
            }
            Some(b) => tracing::info!("SSH banner {b:?}: Windows, probing the shell"),
            None => tracing::info!("no SSH banner from {hostname}:{port}, probing the shell"),
        }
    }

    let mut args: Vec<String> = ssh_command.split_whitespace().map(String::from).collect();
    args.push(host.to_string());
    args.push(SHELL_PROBE.to_string());

    let shell = match Command::new(&args[0]).args(&args[1..]).output() {
        Ok(out) => classify_shell_probe(out.status.success(), &String::from_utf8_lossy(&out.stdout)),
        Err(_) => RemoteShell::Cmd,
    };
    tracing::info!("remote shell probe: {shell:?}");
    shell
}

/// Where the remote server should bind its UDP socket.
#[derive(Clone, Debug, PartialEq, Eq)]
enum BindServer {
    /// `-s`: the server takes the address from `$SSH_CONNECTION`.
    SshConnection,
    /// No bind argument: the server listens on all addresses.
    Any,
    /// `-i ADDR`.
    Address(String),
}

/// Parsed MOSH CONNECT line from server.
struct ConnectInfo {
    port: u16,
    key: String,
    /// The address the SSH connection actually reached (mosh's `MOSH IP`),
    /// when the backend could observe it.
    ip: Option<String>,
}

fn main() {
    // ssh runs us as its ProxyCommand; see fake_proxy.rs.
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some(fake_proxy::FLAG) {
        fake_proxy::run(&argv[2..]);
    }

    let _guard = morsh_log::init("morsh");

    let args = Args::parse();

    // Which address the *server* should bind to. "ssh" means "let the server
    // read it out of $SSH_CONNECTION" — that is what `-s` is for, and it is
    // the only option that gets the right answer when the name we resolve
    // locally is not the address the server actually sees (NAT, split-horizon
    // DNS, multi-homed hosts).
    let bind_arg = match args.bind_server.as_str() {
        "any" => BindServer::Any,
        "ssh" => BindServer::SshConnection,
        ip => BindServer::Address(ip.to_string()),
    };

    // Expand ssh aliases (`Host` blocks in ~/.ssh/config) the way ssh itself
    // would, so `morsh alias` reaches the same machine as `ssh alias`.
    let ssh_cfg = query_ssh_config(&args.ssh_command, &args.host);

    #[cfg(feature = "russh")]
    let info = try_launch_ssh(&args, &bind_arg, ssh_cfg.as_ref()).unwrap_or_else(|| {
        eprintln!("Failed to get MOSH CONNECT from remote server");
        eprintln!("Make sure morsh-server or mosh-server is installed on the remote host");
        std::process::exit(1);
    });

    #[cfg(not(feature = "russh"))]
    let info = try_launch_server(&args.ssh_command, &args.host, ssh_cfg.as_ref(), &{
        let locale_vars = collect_locale_vars();
        let rust_log = std::env::var("RUST_LOG").ok();
        let shell = resolve_remote_shell_value(&args.remote_shell)
            .unwrap_or_else(|| resolve_remote_shell_system(&args.ssh_command, &args.host, ssh_cfg.as_ref()));
        build_remote_cmd(&args, &bind_arg, shell, rust_log, &locale_vars)
    }).unwrap_or_else(|| {
        eprintln!("Failed to get MOSH CONNECT from remote server");
        eprintln!("Make sure morsh-server or mosh-server is installed on the remote host");
        std::process::exit(1);
    });

    // Where *we* send packets. Like mosh, prefer the address the SSH
    // connection itself reached; only when the backend couldn't observe it
    // (a ProxyJump/ProxyCommand host) fall back to resolving it ourselves.
    let server_ip = match (info.ip.clone(), &bind_arg) {
        (Some(ip), _) => ip,
        (None, BindServer::Address(ip)) => ip.clone(),
        (None, _) => resolve_host(&target_hostname(&args.host, ssh_cfg.as_ref())),
    };
    // Bracket IPv6 literals so the port can be appended.
    let server_addr = match server_ip.parse::<std::net::IpAddr>() {
        Ok(ip) => std::net::SocketAddr::new(ip, info.port).to_string(),
        Err(_) => format!("{server_ip}:{}", info.port),
    };

    tracing::info!("Connecting to {server_addr} with key {}", &info.key[..8]);

    let client_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("morsh-client")))
        .unwrap_or_else(|| "morsh-client".into());

    let exit_status = Command::new(&client_path)
        .arg(&server_addr)
        .env("MORSH_KEY", &info.key)
        .env("MOSH_PREDICTION_DISPLAY", &args.predict)
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
    bind: &BindServer,
    shell: RemoteShell,
    rust_log: Option<String>,
    locale_vars: &[(String, String)],
) -> String {
    if args.server_path == "morsh-server" {
        let first = build_server_args("morsh-server", bind, &args.port_range, &args.server_log_file, args.no_daemonize, &args.command, locale_vars);
        let second = build_server_args("mosh-server", bind, &args.port_range, &args.server_log_file, args.no_daemonize, &args.command, locale_vars);
        render_remote_command(shell, first, Some(second), rust_log, locale_vars)
    } else {
        let only = build_server_args(&args.server_path, bind, &args.port_range, &args.server_log_file, args.no_daemonize, &args.command, locale_vars);
        render_remote_command(shell, only, None, rust_log, locale_vars)
    }
}

/// Build the argv for a single server invocation (no shell quoting yet).
fn build_server_args(
    server_path: &str,
    bind: &BindServer,
    port_range: &Option<String>,
    server_log_file: &Option<String>,
    no_daemonize: bool,
    command: &[String],
    locale_vars: &[(String, String)],
) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    parts.push(server_path.to_string());
    parts.push("new".to_string());

    match bind {
        BindServer::SshConnection => parts.push("-s".to_string()),
        BindServer::Address(ip) => {
            parts.push("-i".to_string());
            parts.push(ip.clone());
        }
        BindServer::Any => {}
    }

    if let Some(ref range) = port_range {
        parts.push("-p".to_string());
        parts.push(range.clone());
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
        // `--` is the separator both servers accept; neither has ever had a
        // `-e`, so `morsh host <cmd>` was broken end to end.
        parts.push("--".to_string());
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
                let exports: Vec<String> = locale_vars
                    .iter()
                    .filter(|(_, v)| !v.is_empty())
                    .map(|(n, v)| format!("{n}={}", shell_quote(v)))
                    .collect();
                format!("export {}", exports.join(" "))
            };

            let first = shell_quote_all(&first);
            let base = match second {
                Some(second) => {
                    let second = shell_quote_all(&second);
                    if let Some(v) = &rust_log {
                        format!(
                            "(env RUST_LOG={} {first} 2>/dev/null) || {second}",
                            shell_quote(v)
                        )
                    } else {
                        format!("({first} 2>/dev/null) || {second}")
                    }
                }
                None => {
                    if let Some(v) = &rust_log {
                        format!("env RUST_LOG={} {first}", shell_quote(v))
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

/// Quote an argument for a Posix shell.
///
/// `mosh.pl` runs every argument through `shell_quote` before handing it to
/// ssh; joining with spaces instead breaks on any locale value or command
/// argument containing a space, a quote, or a shell metacharacter.
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@%+=:,./-_".contains(&b))
    {
        return s.to_string();
    }
    // Single quotes protect everything except a single quote itself, which is
    // spliced in as '\''.
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn shell_quote_all(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| shell_quote(p))
        .collect::<Vec<_>>()
        .join(" ")
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
    cfg: Option<&SshConfig>,
    remote_cmd: &str,
) -> Option<ConnectInfo> {
    let mut ssh_args: Vec<String> = ssh_command.split_whitespace().map(String::from).collect();

    // mosh's default `--experimental-remote-ip=proxy`: make ssh connect
    // through `morsh --fake-proxy`, which reports the address it reached.
    // Unlike mosh, keep a ProxyJump/ProxyCommand the user configured: ours
    // would replace it and the host would become unreachable.
    let proxy = match cfg {
        Some(c) if c.uses_proxy => None,
        _ => fake_proxy::proxy_command(),
    };
    if let Some(proxy) = &proxy {
        // `-S none`: a shared ControlMaster connection would skip the proxy.
        ssh_args.extend(["-S".into(), "none".into(), "-o".into(), format!("ProxyCommand={proxy}")]);
    }
    ssh_args.push(host.to_string());
    ssh_args.push(remote_cmd.to_string());

    tracing::info!("SSH: {}", ssh_args[..ssh_args.len() - 1].join(" "));
    tracing::info!("Remote command: {}", ssh_args.last().unwrap());

    let mut command = Command::new(&ssh_args[0]);
    command
        .args(&ssh_args[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    // ssh runs ProxyCommand with `$SHELL -c`; mosh pins it to /bin/sh because
    // exotic shells and noisy rc files break the proxy.
    #[cfg(unix)]
    if proxy.is_some() {
        command.env("SHELL", "/bin/sh");
    }
    let mut ssh_child = command.spawn().ok()?;

    let stdout = ssh_child.stdout.take()?;
    let stderr = ssh_child.stderr.take()?;
    let reader = BufReader::new(stdout);

    // The fake proxy reports on stderr. Read it as it arrives, or a chatty
    // ssh could fill the pipe and stall. The address is handed over as soon
    // as it is seen, so we never wait on the proxy process to exit.
    let (ip_tx, ip_rx) = std::sync::mpsc::channel::<String>();
    let stderr_reader = std::thread::spawn(move || {
        let mut rest = String::new();
        let mut seen_ip = false;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            match fake_proxy::parse_ip_line(&line) {
                Some(addr) if !seen_ip => {
                    seen_ip = true;
                    let _ = ip_tx.send(addr.to_string());
                }
                _ => {
                    rest.push_str(&line);
                    rest.push('\n');
                }
            }
        }
        rest
    });

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
                        ip: None,
                    });
                    break;
                }
            }
        }
    }

    let _ = ssh_child.wait();

    match &mut connect_info {
        // The proxy spoke before ssh could even authenticate, so the address
        // is already waiting if there is one.
        Some(info) => {
            info.ip = ip_rx.try_recv().ok();
            if proxy.is_some() && info.ip.is_none() {
                tracing::info!("fake proxy reported no address; resolving locally");
            }
        }
        // If we never found the MOSH CONNECT line, the SSH command likely
        // failed. Surface its stderr so the user can see what went wrong.
        None => {
            if let Ok(err) = stderr_reader.join() {
                if !err.trim().is_empty() {
                    eprint!("{err}");
                }
            }
        }
    }

    connect_info
}

/// The host part of a `[user@]host[:port]` argument.
fn host_part(host: &str) -> &str {
    host.rsplit_once('@').map_or(host, |(_, h)| h)
}

/// Ask ssh how it would expand `host`, unless the argument uses morsh's own
/// `host:port` form, which ssh does not understand.
fn query_ssh_config(ssh_command: &str, host: &str) -> Option<SshConfig> {
    if host_part(host).contains(':') {
        return None;
    }
    ssh_config::query(ssh_command, host)
}

/// The real host name behind the argument: ssh's `HostName` for an alias,
/// otherwise the argument minus any `user@` and `:port`.
fn target_hostname(host: &str, cfg: Option<&SshConfig>) -> String {
    if let Some(hostname) = cfg.and_then(|c| c.hostname.as_deref()) {
        return hostname.to_string();
    }
    let hostname = host_part(host);
    hostname.split(':').next().unwrap_or(hostname).to_string()
}

/// The SSH port: an explicit `:port` in the argument, then ssh config, then 22.
fn target_port(host: &str, cfg: Option<&SshConfig>) -> u16 {
    host_part(host)
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .or_else(|| cfg.and_then(|c| c.port))
        .unwrap_or(22)
}

fn resolve_host(hostname: &str) -> String {
    tracing::info!("Resolving hostname: {hostname}");

    let addrs: Vec<_> = (hostname, 0u16)
        .to_socket_addrs()
        .unwrap_or_else(|e| {
            eprintln!("Failed to resolve hostname '{hostname}': {e}");
            std::process::exit(1);
        })
        .collect();

    if let Some(addr) = addrs.iter().find(|a| a.ip().is_ipv4()).or(addrs.first()) {
        return addr.ip().to_string();
    }

    eprintln!("No addresses found for '{hostname}'");
    std::process::exit(1);
}

// ── russh (pure-Rust) backend ──────────────────────────────────────────────────

/// Can the ssh command be run at all?
#[cfg(feature = "russh")]
fn ssh_installed(ssh_command: &str) -> bool {
    let Some(program) = ssh_command.split_whitespace().next() else {
        return false;
    };
    Command::new(program)
        .arg("-V")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

#[cfg(feature = "russh")]
fn try_launch_ssh(args: &Args, bind: &BindServer, cfg: Option<&SshConfig>) -> Option<ConnectInfo> {
    // System ssh first: it knows the user's whole configuration, agent and
    // trust prompts. russh is only for machines without it. A failing ssh is
    // not retried with russh, which would bury ssh's error and, after a
    // host key mismatch, undo the point of checking.
    let mode = match args.ssh_mode {
        SshMode::Auto if ssh_installed(&args.ssh_command) => SshMode::System,
        SshMode::Auto => {
            tracing::info!("{} not found, using the built-in ssh client", args.ssh_command);
            SshMode::Russh
        }
        mode => mode,
    };

    if mode == SshMode::Russh {
        let key_paths = find_keys(cfg);
        if key_paths.is_empty() {
            eprintln!("No SSH private key found for the built-in ssh client");
            return None;
        }
        tracing::info!("Connecting via russh (keys: {key_paths:?})");
        return try_launch_russh(args, bind, cfg, &key_paths);
    }

    tracing::info!("using system ssh");
    let locale_vars = collect_locale_vars();
    let rust_log = std::env::var("RUST_LOG").ok();
    let shell = resolve_remote_shell_value(&args.remote_shell)
        .unwrap_or_else(|| resolve_remote_shell_system(&args.ssh_command, &args.host, cfg));
    let remote_cmd = build_remote_cmd(args, bind, shell, rust_log, &locale_vars);
    try_launch_server(&args.ssh_command, &args.host, cfg, &remote_cmd)
}

#[cfg(feature = "russh")]
fn try_launch_russh(
    args: &Args,
    bind: &BindServer,
    cfg: Option<&SshConfig>,
    key_paths: &[std::path::PathBuf],
) -> Option<ConnectInfo> {
    use morsh_ssh::SshSession;

    let (user, hostname, port) = russh_target(&args.host, cfg);
    let addr = format!("{hostname}:{port}");

    tracing::info!("russh: connecting to {addr} as {user}");

    let host_keys = host_key_policy(&hostname, port, cfg);
    let mut session = SshSession::connect(addr, user, key_paths, host_keys)
        .inspect_err(|e| eprintln!("morsh: {e:#}"))
        .ok()?;

    // Probe the remote shell using the same session.
    let shell = match resolve_remote_shell_value(&args.remote_shell) {
        Some(s) => s,
        None => {
            let (exit_code, probe_out) = session.exec_raw(SHELL_PROBE).ok()?;
            let shell = classify_shell_probe(exit_code == 0, &probe_out);
            tracing::info!("remote shell probe: {shell:?}");
            shell
        }
    };

    let rust_log = std::env::var("RUST_LOG").ok();
    let locale_vars = collect_locale_vars();
    let remote_cmd = build_remote_cmd(args, bind, shell, rust_log, &locale_vars);

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
                                ip: None,
                            });
                        }
                    }
                }
            },
        )
        .ok()?;

    tracing::info!("russh: remote command exited with code {exit_code}");
    let result = connect_info.lock().unwrap().take();
    // We made the TCP connection ourselves, so we know where it went: the
    // russh equivalent of mosh's fake proxy.
    result.map(|info| ConnectInfo {
        ip: Some(session.peer_addr().ip().to_string()),
        ..info
    })
}

/// User, host name and port for a direct russh connection.
///
/// An explicit `user@` / `:port` in the argument wins, then ssh config, then
/// the same defaults ssh uses (the local user name, port 22).
#[cfg(feature = "russh")]
fn russh_target(host: &str, cfg: Option<&SshConfig>) -> (String, String, u16) {
    let explicit_user = host.rsplit_once('@').map(|(u, _)| u.to_string());

    let user = explicit_user
        .or_else(|| cfg.and_then(|c| c.user.clone()))
        .or_else(|| std::env::var("USER").ok())
        .or_else(|| std::env::var("USERNAME").ok())
        .unwrap_or_else(|| "root".to_string());

    (user, target_hostname(host, cfg), target_port(host, cfg))
}

/// How russh should verify `hostname`'s key: the same files and settings
/// ssh would use. Custom `UserKnownHostsFile`s are not read, because
/// `ssh -G` prints them unquoted and a path with a space can't be recovered.
#[cfg(feature = "russh")]
fn host_key_policy(hostname: &str, port: u16, cfg: Option<&SshConfig>) -> morsh_ssh::HostKeyPolicy {
    use morsh_ssh::{HostKeyPolicy, StrictHostKeyChecking};
    use std::path::PathBuf;

    let user_file = PathBuf::from(tilde_expand("~/.ssh/known_hosts"));
    let mut known_hosts = vec![user_file.clone(), PathBuf::from(tilde_expand("~/.ssh/known_hosts2"))];
    if cfg!(windows) {
        if let Ok(data) = std::env::var("ProgramData") {
            known_hosts.push(PathBuf::from(&data).join("ssh").join("ssh_known_hosts"));
        }
    } else {
        known_hosts.push(PathBuf::from("/etc/ssh/ssh_known_hosts"));
        known_hosts.push(PathBuf::from("/etc/ssh/ssh_known_hosts2"));
    }

    HostKeyPolicy {
        // ssh records host names in lower case.
        host: cfg
            .and_then(|c| c.host_key_alias.clone())
            .unwrap_or_else(|| hostname.to_ascii_lowercase()),
        port,
        known_hosts,
        learn_into: user_file,
        strict: cfg
            .and_then(|c| c.strict_host_key_checking.as_deref())
            .map_or(StrictHostKeyChecking::Ask, StrictHostKeyChecking::from_ssh),
    }
}

#[cfg(feature = "russh")]
fn tilde_expand(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        // HOME is usually unset on Windows; USERPROFILE is its equivalent.
        if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

/// The private keys to offer, in order: the existing `IdentityFile`s from
/// ssh config (which already lists ssh's defaults when none is configured),
/// otherwise the usual default locations.
#[cfg(feature = "russh")]
fn find_keys(cfg: Option<&SshConfig>) -> Vec<std::path::PathBuf> {
    const DEFAULTS: [&str; 4] = [
        "~/.ssh/id_ed25519",
        "~/.ssh/id_rsa",
        "~/.ssh/id_ecdsa",
        "~/.ssh/identity",
    ];
    let configured: Vec<&str> = cfg
        .map(|c| c.identity_files.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let candidates = if configured.is_empty() { DEFAULTS.to_vec() } else { configured };
    candidates
        .into_iter()
        .map(|p| std::path::PathBuf::from(tilde_expand(p)))
        .filter(|p| p.is_file())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PowerShell remote failed `uname -s` and was sent cmd.exe syntax,
    /// whose `&&`/`||` Windows PowerShell 5.1 can't parse.
    #[test]
    fn shell_probe_recognises_powershell() {
        assert_eq!(classify_shell_probe(true, "Desktop
"), RemoteShell::PowerShell);
        assert_eq!(classify_shell_probe(true, "Core
"), RemoteShell::PowerShell);
    }

    #[test]
    fn shell_probe_recognises_cmd_and_posix() {
        assert_eq!(classify_shell_probe(true, "$PSVersionTable.PSEdition
"), RemoteShell::Cmd);
        assert_eq!(classify_shell_probe(true, ".PSEdition
"), RemoteShell::Posix);
        assert_eq!(classify_shell_probe(false, ""), RemoteShell::Cmd);
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// `morsh host <cmd>` used to emit `-e`, which neither server accepts.
    #[test]
    fn a_remote_command_is_introduced_with_a_double_dash() {
        let args = build_server_args(
            "morsh-server",
            &BindServer::SshConnection,
            &None,
            &None,
            false,
            &strings(&["htop", "-d", "5"]),
            &[],
        );
        assert!(!args.contains(&"-e".to_string()), "{args:?}");
        let dashdash = args.iter().position(|a| a == "--").expect("-- separator");
        assert_eq!(&args[dashdash + 1..], &strings(&["htop", "-d", "5"])[..]);
    }

    /// The default `--bind-server ssh` must let the *server* pick the address
    /// out of $SSH_CONNECTION, rather than passing whatever our local resolver
    /// happened to return.
    #[test]
    fn bind_server_ssh_emits_dash_s() {
        let args = build_server_args("morsh-server", &BindServer::SshConnection, &None, &None, false, &[], &[]);
        assert!(args.contains(&"-s".to_string()), "{args:?}");
        assert!(!args.contains(&"-i".to_string()), "{args:?}");
    }

    /// `--port` used to be parsed and then dropped, so the server always
    /// picked its own port.
    #[test]
    fn port_range_is_forwarded_as_dash_p() {
        let args = build_server_args(
            "morsh-server",
            &BindServer::SshConnection,
            &Some("60000:60010".to_string()),
            &None,
            false,
            &[],
            &[],
        );
        let p = args.iter().position(|a| a == "-p").expect("-p");
        assert_eq!(args[p + 1], "60000:60010");
    }

    #[test]
    fn no_port_range_emits_no_dash_p() {
        let args = build_server_args("morsh-server", &BindServer::SshConnection, &None, &None, false, &[], &[]);
        assert!(!args.contains(&"-p".to_string()), "{args:?}");
    }

    #[test]
    fn explicit_bind_address_emits_dash_i() {
        let args = build_server_args(
            "morsh-server",
            &BindServer::Address("10.0.0.7".to_string()),
            &None,
            &None,
            false,
            &[],
            &[],
        );
        let i = args.iter().position(|a| a == "-i").expect("-i");
        assert_eq!(args[i + 1], "10.0.0.7");
        assert!(!args.contains(&"-s".to_string()));
    }

    #[test]
    fn bind_server_any_emits_neither() {
        let args = build_server_args("morsh-server", &BindServer::Any, &None, &None, false, &[], &[]);
        assert!(!args.contains(&"-s".to_string()));
        assert!(!args.contains(&"-i".to_string()));
    }

    #[test]
    fn the_generated_invocation_matches_mosh_pls_shape() {
        let args = build_server_args(
            "mosh-server",
            &BindServer::SshConnection,
            &None,
            &None,
            false,
            &[],
            &[("LANG".to_string(), "en_US.UTF-8".to_string())],
        );
        assert_eq!(
            args,
            strings(&["mosh-server", "new", "-s", "-l", "LANG=en_US.UTF-8"])
        );
    }

    #[test]
    fn shell_quoting_leaves_ordinary_words_alone() {
        assert_eq!(shell_quote("mosh-server"), "mosh-server");
        assert_eq!(shell_quote("LANG=en_US.UTF-8"), "LANG=en_US.UTF-8");
        assert_eq!(shell_quote("/usr/bin/mosh-server"), "/usr/bin/mosh-server");
    }

    /// Anything with a space or a metacharacter has to be quoted, or the
    /// remote shell re-splits it.
    #[test]
    fn shell_quoting_protects_spaces_and_metacharacters() {
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("a;rm -rf /"), "'a;rm -rf /'");
        assert_eq!(shell_quote("$(whoami)"), "'$(whoami)'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn posix_command_quotes_locale_values_and_arguments() {
        let cmd = render_remote_command(
            RemoteShell::Posix,
            strings(&["morsh-server", "new", "-s", "--", "echo", "hello world"]),
            None,
            None,
            &[("LANG".to_string(), "en_US.UTF-8".to_string())],
        );
        assert!(cmd.contains("'hello world'"), "{cmd}");
        assert!(cmd.starts_with("export LANG=en_US.UTF-8;"), "{cmd}");
    }

    #[test]
    fn an_alias_resolves_to_its_configured_hostname() {
        let cfg = SshConfig {
            hostname: Some("203.0.113.9".to_string()),
            ..Default::default()
        };
        assert_eq!(target_hostname("myalias", Some(&cfg)), "203.0.113.9");
        assert_eq!(target_hostname("bob@myalias", Some(&cfg)), "203.0.113.9");
    }

    #[test]
    fn without_ssh_config_the_argument_is_taken_literally() {
        assert_eq!(target_hostname("bob@example.org:2222", None), "example.org");
        assert_eq!(target_hostname("example.org", None), "example.org");
    }

    #[test]
    fn morshs_host_port_form_is_not_sent_to_ssh() {
        assert!(query_ssh_config("ssh", "bob@example.org:2222").is_none());
    }

    #[cfg(feature = "russh")]
    #[test]
    fn russh_uses_user_and_port_from_ssh_config() {
        let cfg = SshConfig {
            hostname: Some("203.0.113.9".to_string()),
            user: Some("alice".to_string()),
            port: Some(2222),
            ..Default::default()
        };
        assert_eq!(
            russh_target("myalias", Some(&cfg)),
            ("alice".to_string(), "203.0.113.9".to_string(), 2222)
        );
    }

    #[cfg(feature = "russh")]
    #[test]
    fn an_explicit_user_and_port_override_ssh_config() {
        let cfg = SshConfig {
            hostname: Some("h".to_string()),
            user: Some("alice".to_string()),
            port: Some(2222),
            ..Default::default()
        };
        let (user, _, _) = russh_target("bob@myalias", Some(&cfg));
        assert_eq!(user, "bob");
        let (_, host, port) = russh_target("example.org:2200", None);
        assert_eq!((host.as_str(), port), ("example.org", 2200));
    }

    #[test]
    fn posix_command_quotes_a_locale_value_containing_a_space() {
        let cmd = render_remote_command(
            RemoteShell::Posix,
            strings(&["morsh-server", "new", "-s"]),
            None,
            None,
            &[("LC_ALL".to_string(), "weird value".to_string())],
        );
        assert!(cmd.contains("LC_ALL='weird value'"), "{cmd}");
    }
}
