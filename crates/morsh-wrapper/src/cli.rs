use clap::Parser;

/// Which SSH backend to use for connecting to the remote host.
#[cfg_attr(not(feature = "russh"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SshMode {
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
pub(crate) struct Args {
    /// SSH command to use (default: "ssh")
    #[arg(long = "ssh", default_value = "ssh", value_parser = crate::ssh_command::validate)]
    pub(crate) ssh_command: String,

    /// UDP port range for morsh-server (e.g., "60000:61000")
    #[arg(long = "port", short = 'p', value_parser = parse_port_range)]
    pub(crate) port_range: Option<String>,

    /// Predict the future (enable prediction engine)
    #[arg(long = "predict", default_value = "adaptive", value_parser = ["adaptive", "always", "never"])]
    pub(crate) predict: String,

    /// Control the IP address that morsh-server binds to:
    ///   ssh  - bind to the IP from SSH_CONNECTION (default)
    ///   any  - bind to 0.0.0.0
    ///   IP   - bind to the specified IP address
    #[arg(long = "bind-server", default_value = "ssh")]
    pub(crate) bind_server: String,

    /// Path on the remote host for morsh-server debug logs
    #[arg(long = "server-log-file")]
    pub(crate) server_log_file: Option<String>,

    /// Run morsh-server in foreground (don't daemonize)
    #[arg(long = "no-daemonize", short = 'D')]
    pub(crate) no_daemonize: bool,

    /// Path to morsh-server on the remote host (default: morsh-server, looked up in PATH)
    #[arg(long = "server", short = 'S', default_value = "morsh-server")]
    pub(crate) server_path: String,

    /// SSH backend: "auto" (system ssh, or russh when no ssh is installed),
    /// "system" (system ssh command), or "russh" (pure Rust).
    /// Only available when compiled with the "russh" feature.
    #[cfg(feature = "russh")]
    #[arg(long = "ssh-mode", default_value = "auto")]
    pub(crate) ssh_mode: SshMode,

    /// Remote user@host
    pub(crate) host: String,

    /// Command to run on remote (default: shell)
    #[arg(trailing_var_arg = true)]
    pub(crate) command: Vec<String>,

    /// Shell on the remote host that interprets the launch command.
    /// auto (detect), posix (sh/bash), cmd (Windows cmd.exe), or powershell (pwsh).
    /// Default: auto.
    #[arg(long = "remote-shell", default_value = "auto")]
    pub(crate) remote_shell: String,
}

fn parse_port_range(value: &str) -> Result<String, String> {
    let bad = || format!("invalid UDP port or range '{value}' (expected PORT or LOW:HIGH)");
    let parse = |part: &str| -> Result<u16, String> {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        part.parse().map_err(|_| bad())
    };
    match value.split_once(':') {
        Some((low, high)) => {
            let (low, high) = (parse(low)?, parse(high)?);
            if low == 0 || low > high {
                return Err(bad());
            }
        }
        None => {
            parse(value)?;
        }
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stock_mosh_short_port_flag_is_accepted() {
        let args = Args::try_parse_from(["morsh", "-p", "60000:60010", "host"]).unwrap();
        assert_eq!(args.port_range.as_deref(), Some("60000:60010"));
    }

    #[test]
    fn command_options_belong_to_the_remote_command() {
        let args = Args::try_parse_from(["morsh", "host", "sh", "-c", "echo hi"]).unwrap();
        assert_eq!(args.command, ["sh", "-c", "echo hi"]);
    }

    #[test]
    fn empty_or_malformed_ssh_commands_are_rejected() {
        for value in ["", "   ", "ssh -o 'unterminated"] {
            assert!(Args::try_parse_from(["morsh", "--ssh", value, "host"]).is_err());
        }
    }

    #[test]
    fn bad_ports_are_rejected_before_ssh() {
        for value in ["", "65536", "0:1", "2:1", "1:", "a", "1:2:3"] {
            assert!(Args::try_parse_from(["morsh", "-p", value, "host"]).is_err());
        }
        for value in ["0", "1", "65535", "60000:60010"] {
            assert!(Args::try_parse_from(["morsh", "-p", value, "host"]).is_ok());
        }
    }
}
