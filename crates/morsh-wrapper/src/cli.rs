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

    /// Prediction mode: adaptive (default), always, never, or experimental.
    #[arg(long, overrides_with_all = ["predict", "always", "never"])]
    predict: Option<String>,

    /// Use local echo even on fast links (--predict=always).
    #[arg(short = 'a', overrides_with_all = ["predict", "always", "never"])]
    always: bool,

    /// Never use local echo (--predict=never).
    #[arg(short = 'n', overrides_with_all = ["predict", "always", "never"])]
    never: bool,

    /// Prediction overwrites existing text instead of inserting.
    #[arg(long, short = 'o', overrides_with_all = ["predict_overwrite", "no_predict_overwrite"])]
    predict_overwrite: bool,

    /// Do not request prediction overwrite (preserves an inherited setting).
    #[arg(long, alias = "nopredict-overwrite", overrides_with_all = ["predict_overwrite", "no_predict_overwrite"])]
    no_predict_overwrite: bool,

    /// Do not send terminal initialization and restoration sequences.
    #[arg(long, alias = "noinit", overrides_with = "init")]
    pub(crate) no_init: bool,

    /// Send terminal initialization and restoration sequences (default).
    #[arg(long, overrides_with = "no_init")]
    init: bool,

    /// Address family: inet, inet6, auto, all, prefer-inet, or prefer-inet6.
    #[arg(long, default_value = "prefer-inet", overrides_with_all = ["ipv4", "ipv6"])]
    family: crate::family::Family,

    /// Use IPv4 only.
    #[arg(short = '4', overrides_with_all = ["family", "ipv6"])]
    ipv4: bool,

    /// Use IPv6 only.
    #[arg(short = '6', overrides_with_all = ["family", "ipv4"])]
    ipv6: bool,

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

impl Args {
    pub(crate) fn family(&self) -> crate::family::Family {
        if self.ipv4 {
            crate::family::Family::Inet
        } else if self.ipv6 {
            crate::family::Family::Inet6
        } else {
            self.family
        }
    }

    pub(crate) fn prediction_mode(&self, inherited: Option<&str>) -> Result<String, String> {
        // mosh.pl validates after GetOptions, so the final explicit option
        // overrides earlier values and even an invalid inherited mode.
        let explicit = if self.always {
            Some("always")
        } else if self.never {
            Some("never")
        } else {
            self.predict.as_deref()
        };
        let mode = explicit.or(inherited).unwrap_or("adaptive");
        if matches!(mode, "adaptive" | "always" | "never" | "experimental") {
            return Ok(mode.to_string());
        }
        let explanation = if explicit.is_none() && inherited.is_some() {
            " (MOSH_PREDICTION_DISPLAY in environment)"
        } else {
            ""
        };
        Err(format!("Unknown mode \"{mode}\"{explanation}."))
    }

    pub(crate) fn configure_client(&self, command: &mut std::process::Command, prediction: &str) {
        command.env("MOSH_PREDICTION_DISPLAY", prediction);
        if self.predict_overwrite && !self.no_predict_overwrite {
            command.env("MOSH_PREDICTION_OVERWRITE", "yes");
        }
        // mosh.pl preserves an inherited value unless --no-init is selected.
        if self.no_init && !self.init {
            command.env("MOSH_NO_TERM_INIT", "1");
        }
    }
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
    fn stock_prediction_flags_are_accepted() {
        for flags in [
            vec!["--predict=adaptive"],
            vec!["--predict=always"],
            vec!["--predict=never"],
            vec!["--predict=experimental"],
            vec!["-a"],
            vec!["-n"],
            vec!["--predict-overwrite"],
            vec!["-o"],
        ] {
            let parsed = Args::try_parse_from(["morsh"].into_iter().chain(flags).chain(["host"]));
            assert!(parsed.is_ok(), "{parsed:?}");
        }
    }

    #[test]
    fn prediction_modes_use_the_final_option_then_environment_then_default() {
        for (flags, inherited, expected) in [
            (vec![], None, "adaptive"),
            (vec![], Some("experimental"), "experimental"),
            (vec![], Some("never"), "never"),
            (vec!["--predict=adaptive"], Some("always"), "adaptive"),
            (vec!["--predict=experimental"], None, "experimental"),
            (vec!["-a"], Some("invalid"), "always"),
            (vec!["-n"], Some("invalid"), "never"),
            (vec!["-an"], None, "never"),
            (vec!["-na"], None, "always"),
            (vec!["-a", "-a"], None, "always"),
            (vec!["-n", "--predict=experimental"], None, "experimental"),
            (vec!["--predict=invalid", "-a"], None, "always"),
            (vec!["--predict=invalid", "--predict=never"], None, "never"),
        ] {
            let args =
                Args::try_parse_from(["morsh"].into_iter().chain(flags).chain(["host"])).unwrap();
            let mode = args.prediction_mode(inherited).unwrap();
            assert_eq!(mode, expected);
            let mut command = std::process::Command::new("morsh-client");
            args.configure_client(&mut command, &mode);
            assert_eq!(
                command
                    .get_envs()
                    .find(|(key, _)| *key == "MOSH_PREDICTION_DISPLAY")
                    .unwrap()
                    .1
                    .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn prediction_mode_errors_match_mosh_pl() {
        let args = Args::try_parse_from(["morsh", "host"]).unwrap();
        for mode in ["", "ADAPTIVE", "invalid"] {
            assert_eq!(
                args.prediction_mode(Some(mode)).unwrap_err(),
                format!("Unknown mode \"{mode}\" (MOSH_PREDICTION_DISPLAY in environment).")
            );
            let args = Args::try_parse_from(["morsh", "-a", "--predict", mode, "host"]).unwrap();
            assert_eq!(
                args.prediction_mode(Some("never")).unwrap_err(),
                format!("Unknown mode \"{mode}\".")
            );
        }
    }

    #[test]
    fn prediction_overwrite_sets_yes_only_when_requested() {
        for (flags, overwrite) in [
            (vec![], false),
            (vec!["--predict-overwrite"], true),
            (vec!["-o"], true),
            (vec!["-oo"], true),
            (vec!["--predict-overwrite", "--no-predict-overwrite"], false),
            (vec!["-o", "--nopredict-overwrite"], false),
            (vec!["--no-predict-overwrite", "-o"], true),
        ] {
            let args =
                Args::try_parse_from(["morsh"].into_iter().chain(flags).chain(["host"])).unwrap();
            let mut command = std::process::Command::new("morsh-client");
            args.configure_client(&mut command, "adaptive");
            let value = command
                .get_envs()
                .find(|(key, _)| *key == "MOSH_PREDICTION_OVERWRITE");
            assert_eq!(
                value.map(|(_, value)| value.unwrap()),
                overwrite.then_some(std::ffi::OsStr::new("yes"))
            );
            // Like mosh.pl, a disabled overwrite flag does not unset the
            // parent's existing MOSH_PREDICTION_OVERWRITE value.
            let mut inherited = std::process::Command::new("morsh-client");
            inherited.env("MOSH_PREDICTION_OVERWRITE", "yes");
            args.configure_client(&mut inherited, "adaptive");
            assert_eq!(
                inherited
                    .get_envs()
                    .find(|(key, _)| *key == "MOSH_PREDICTION_OVERWRITE")
                    .unwrap()
                    .1
                    .unwrap(),
                "yes"
            );
        }
    }

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
    fn stock_terminal_init_flags_are_accepted() {
        assert!(Args::try_parse_from(["morsh", "--no-init", "host"]).is_ok());
        assert!(Args::try_parse_from(["morsh", "--noinit", "host"]).is_ok());
        assert!(Args::try_parse_from(["morsh", "--init", "host"]).is_ok());
    }

    #[test]
    fn no_init_is_forwarded_only_to_the_client_and_last_flag_wins() {
        for (flags, expected) in [
            (vec![], false),
            (vec!["--no-init"], true),
            (vec!["--noinit"], true),
            (vec!["--init"], false),
            (vec!["--no-init", "--init"], false),
            (vec!["--init", "--no-init"], true),
        ] {
            let args =
                Args::try_parse_from(["morsh"].into_iter().chain(flags).chain(["host"])).unwrap();
            let mut command = std::process::Command::new("morsh-client");
            args.configure_client(&mut command, "adaptive");
            let value = command
                .get_envs()
                .find(|(key, _)| *key == "MOSH_NO_TERM_INIT");
            assert_eq!(value.is_some(), expected);
            if let Some((_, value)) = value {
                assert_eq!(value.unwrap(), "1");
            }
        }
    }

    #[test]
    fn stock_family_options_are_accepted() {
        for value in [
            "inet",
            "inet6",
            "auto",
            "all",
            "prefer-inet",
            "prefer-inet6",
        ] {
            assert!(Args::try_parse_from(["morsh", "--family", value, "host"]).is_ok());
        }
        for flag in ["-4", "-6"] {
            assert!(Args::try_parse_from(["morsh", flag, "host"]).is_ok());
        }
    }

    #[test]
    fn last_family_option_wins() {
        use crate::family::Family;
        for (flags, expected) in [
            (vec![], Family::PreferInet),
            (vec!["-4", "-6"], Family::Inet6),
            (vec!["-6", "--family", "all"], Family::All),
            (vec!["--family", "inet6", "-4"], Family::Inet),
            (vec!["--family", "INET6"], Family::Inet6),
        ] {
            let args =
                Args::try_parse_from(["morsh"].into_iter().chain(flags).chain(["host"])).unwrap();
            assert_eq!(args.family(), expected);
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
