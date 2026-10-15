//! Expand a host argument through the user's ssh configuration.
//!
//! `morsh myalias` must mean what `ssh myalias` means: the alias may name a
//! different `HostName`, `User`, `Port` or `IdentityFile`, none of which a DNS
//! lookup of "myalias" can know about. Rather than parse `~/.ssh/config`
//! ourselves (and get `Include`, `Match` and friends wrong), we ask OpenSSH
//! with `ssh -G`, which prints the fully evaluated configuration and exits
//! without connecting.

use std::process::{Command, Stdio};

/// The parts of `ssh -G` output that morsh cares about.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SshConfig {
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    /// In the order ssh would try them; paths may still start with `~`.
    pub identity_files: Vec<String>,
    /// The host is only reachable through another hop, so neither a direct
    /// russh connection nor a local DNS lookup is meaningful on its own.
    pub uses_proxy: bool,
    /// `StrictHostKeyChecking`, as `ssh -G` prints it (`true`, `false`,
    /// `accept-new`, `ask`).
    pub strict_host_key_checking: Option<String>,
    /// `HostKeyAlias`: the name the host key is recorded under.
    pub host_key_alias: Option<String>,
}

/// Run `<ssh_command> -G <host>` and parse its output.
///
/// Returns `None` when ssh is missing or too old to know `-G` (OpenSSH < 6.8);
/// callers then fall back to treating `host` literally.
pub fn query(ssh_command: &str, host: &str) -> Option<SshConfig> {
    let mut args = ssh_command.split_whitespace();
    let program = args.next()?;

    let out = Command::new(program)
        .args(args)
        .arg("-G")
        .arg(host)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !out.status.success() {
        tracing::info!("`{ssh_command} -G {host}` failed; using the host name as given");
        return None;
    }

    let config = parse(&String::from_utf8_lossy(&out.stdout));
    tracing::info!("ssh config for {host}: {config:?}");
    Some(config)
}

/// Parse `ssh -G` output: one lowercase `key value` pair per line.
pub fn parse(output: &str) -> SshConfig {
    let mut config = SshConfig::default();

    for line in output.lines() {
        let Some((key, value)) = line.trim().split_once(' ') else {
            continue;
        };
        let value = value.trim();
        match key {
            "hostname" => config.hostname = Some(value.to_string()),
            "user" => config.user = Some(value.to_string()),
            "port" => config.port = value.parse().ok(),
            "identityfile" => config.identity_files.push(value.to_string()),
            "stricthostkeychecking" => config.strict_host_key_checking = Some(value.to_string()),
            "hostkeyalias" => config.host_key_alias = Some(value.to_string()),
            // `ssh -G` prints "proxyjump none" / omits it when unset.
            "proxycommand" | "proxyjump" if !value.eq_ignore_ascii_case("none") => {
                config.uses_proxy = true;
            }
            _ => {}
        }
    }

    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_alias_expands_to_its_hostname_user_port_and_keys() {
        let config = parse(
            "user alice\n\
             hostname 203.0.113.9\n\
             port 2222\n\
             addressfamily any\n\
             identityfile ~/.ssh/work_ed25519\n\
             identityfile ~/.ssh/id_rsa\n",
        );
        assert_eq!(config.hostname.as_deref(), Some("203.0.113.9"));
        assert_eq!(config.user.as_deref(), Some("alice"));
        assert_eq!(config.port, Some(2222));
        assert_eq!(
            config.identity_files,
            vec!["~/.ssh/work_ed25519".to_string(), "~/.ssh/id_rsa".to_string()]
        );
        assert!(!config.uses_proxy);
    }

    #[test]
    fn host_key_settings_are_read() {
        let config = parse("stricthostkeychecking accept-new\nhostkeyalias web\n");
        assert_eq!(config.strict_host_key_checking.as_deref(), Some("accept-new"));
        assert_eq!(config.host_key_alias.as_deref(), Some("web"));
    }

    #[test]
    fn a_jump_host_is_reported_as_a_proxy() {
        assert!(parse("hostname internal\nproxyjump bastion\n").uses_proxy);
        assert!(parse("hostname internal\nproxycommand nc %h %p\n").uses_proxy);
    }

    #[test]
    fn an_explicit_none_is_not_a_proxy() {
        assert!(!parse("hostname h\nproxycommand none\nproxyjump none\n").uses_proxy);
    }

    #[test]
    fn windows_line_endings_are_tolerated() {
        let config = parse("hostname example.org\r\nport 22\r\n");
        assert_eq!(config.hostname.as_deref(), Some("example.org"));
        assert_eq!(config.port, Some(22));
    }
}
