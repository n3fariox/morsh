//! Server host key verification against OpenSSH `known_hosts` files.
//!
//! Matching and recording use russh's own `known_hosts` helpers, which handle
//! hashed (`|1|…`) and `[host]:port` entries. They do not understand wildcard
//! patterns, `!negation`, `@cert-authority` or `@revoked` lines.

use std::borrow::Cow;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use russh::keys::known_hosts::{check_known_hosts_path, known_host_keys_path, learn_known_hosts_path};
use russh::keys::{self, ssh_key, Algorithm, HashAlg};
use russh::Preferred;

/// What to do with a host whose key is not recorded yet, after ssh's
/// `StrictHostKeyChecking`. A *changed* key is always refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictHostKeyChecking {
    /// Refuse it (`yes`).
    Yes,
    /// Record it without asking (`accept-new`; also used for `no`, which in
    /// ssh would additionally let a changed key through).
    AcceptNew,
    /// Show the fingerprint and ask (`ask`, ssh's default).
    Ask,
}

impl StrictHostKeyChecking {
    /// Parse a value as `ssh -G` prints it.
    pub fn from_ssh(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "yes" | "true" => Self::Yes,
            "accept-new" | "no" | "false" | "off" => Self::AcceptNew,
            _ => Self::Ask,
        }
    }
}

/// How to verify one server's host key.
#[derive(Debug, Clone)]
pub struct HostKeyPolicy {
    /// The name the key is recorded under: `HostKeyAlias`, else the host name.
    pub host: String,
    pub port: u16,
    /// Files to look the key up in, in order. Missing ones are skipped.
    pub known_hosts: Vec<PathBuf>,
    /// Where a newly accepted key is recorded.
    pub learn_into: PathBuf,
    pub strict: StrictHostKeyChecking,
}

impl HostKeyPolicy {
    /// `host` or `[host]:port`, the way ssh shows it.
    fn display_host(&self) -> String {
        if self.port == 22 {
            self.host.clone()
        } else {
            format!("[{}]:{}", self.host, self.port)
        }
    }

    /// russh's default preferences, narrowed to the key types already recorded
    /// for this host. Otherwise russh might negotiate, say, ed25519 for a host
    /// we only know an RSA key for, and the host would look unknown. ssh does
    /// the same.
    pub fn preferred(&self) -> Preferred {
        let recorded: Vec<Algorithm> = self
            .known_hosts
            .iter()
            .filter_map(|p| known_host_keys_path(&self.host, self.port, p).ok())
            .flatten()
            .map(|(_, key)| key.algorithm())
            .collect();

        let mut preferred = Preferred::default();
        let narrowed: Vec<Algorithm> = preferred
            .key
            .iter()
            .filter(|a| recorded.iter().any(|r| same_key_type(a, r)))
            .cloned()
            .collect();
        if !narrowed.is_empty() {
            preferred.key = Cow::Owned(narrowed);
        }
        preferred
    }

    /// Decide whether to trust `key`, telling the user why not. May prompt.
    pub fn verify(&self, key: &ssh_key::PublicKey) -> bool {
        let host = self.display_host();
        let fingerprint = key.fingerprint(HashAlg::Sha256);

        for path in &self.known_hosts {
            match check_known_hosts_path(&self.host, self.port, key, path) {
                Ok(true) => return true,
                Ok(false) => {}
                Err(keys::Error::KeyChanged { line }) => {
                    eprintln!(
                        "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
                         @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n\
                         @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
                         The {} host key for {host} does not match the one in\n\
                         {}:{line}. Someone could be intercepting the connection,\n\
                         or the host key was just changed. The key it sent has\n\
                         fingerprint {fingerprint}.\n\
                         Refusing to connect.",
                        key.algorithm(),
                        path.display(),
                    );
                    return false;
                }
                Err(e) => tracing::info!("can't use {}: {e}", path.display()),
            }
        }

        let accept = match self.strict {
            StrictHostKeyChecking::Yes => {
                eprintln!(
                    "No {} host key is known for {host} and StrictHostKeyChecking is set.",
                    key.algorithm()
                );
                false
            }
            StrictHostKeyChecking::AcceptNew => true,
            StrictHostKeyChecking::Ask => ask(&host, key, &fingerprint.to_string()),
        };
        if accept {
            match learn_known_hosts_path(&self.host, self.port, key, &self.learn_into) {
                Ok(()) => eprintln!(
                    "Warning: Permanently added '{host}' ({}) to the list of known hosts.",
                    key.algorithm()
                ),
                Err(e) => eprintln!(
                    "Warning: could not record the host key in {}: {e}",
                    self.learn_into.display()
                ),
            }
        }
        accept
    }
}

/// RSA keys are one type whatever signature hash is negotiated.
fn same_key_type(a: &Algorithm, b: &Algorithm) -> bool {
    matches!((a, b), (Algorithm::Rsa { .. }, Algorithm::Rsa { .. })) || a == b
}

/// ssh's first-connection question.
fn ask(host: &str, key: &ssh_key::PublicKey, fingerprint: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "The host key for {host} is not known, and there is no terminal to ask \
             whether to trust it. Connect once with ssh to record it."
        );
        return false;
    }

    eprint!(
        "The authenticity of host '{host}' can't be established.\n\
         {} key fingerprint is {fingerprint}.\n\
         Are you sure you want to continue connecting (yes/no)? ",
        key.algorithm()
    );
    let stdin = std::io::stdin();
    loop {
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        if stdin.lock().read_line(&mut answer).unwrap_or(0) == 0 {
            return false;
        }
        match answer.trim().to_ascii_lowercase().as_str() {
            "yes" => return true,
            "no" => return false,
            _ => eprint!("Please type 'yes' or 'no': "),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_g_values_map_onto_the_policy() {
        assert_eq!(StrictHostKeyChecking::from_ssh("true"), StrictHostKeyChecking::Yes);
        assert_eq!(StrictHostKeyChecking::from_ssh("false"), StrictHostKeyChecking::AcceptNew);
        assert_eq!(
            StrictHostKeyChecking::from_ssh("accept-new"),
            StrictHostKeyChecking::AcceptNew
        );
        assert_eq!(StrictHostKeyChecking::from_ssh("ask"), StrictHostKeyChecking::Ask);
    }

    #[test]
    fn rsa_keys_match_whatever_the_signature_hash() {
        assert!(same_key_type(
            &Algorithm::Rsa { hash: Some(keys::HashAlg::Sha512) },
            &Algorithm::Rsa { hash: None }
        ));
        assert!(!same_key_type(&Algorithm::Ed25519, &Algorithm::Rsa { hash: None }));
    }

    /// An ed25519 public key whose 32 bytes are all `byte`.
    fn ed25519(byte: u8) -> ssh_key::PublicKey {
        use ssh_key::public::{Ed25519PublicKey, KeyData};
        ssh_key::PublicKey::new(KeyData::Ed25519(Ed25519PublicKey([byte; 32])), "")
    }

    fn key() -> ssh_key::PublicKey {
        ed25519(1)
    }

    fn record(dir: &std::path::Path, key: &ssh_key::PublicKey) {
        let line = format!("example.org {}\n", key.to_openssh().unwrap());
        std::fs::write(dir.join("known_hosts"), line).unwrap();
    }

    fn policy(dir: &std::path::Path, strict: StrictHostKeyChecking) -> HostKeyPolicy {
        HostKeyPolicy {
            host: "example.org".into(),
            port: 22,
            known_hosts: vec![dir.join("known_hosts")],
            learn_into: dir.join("known_hosts"),
            strict,
        }
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("morsh-hostkeys-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_recorded_key_is_trusted() {
        let dir = tempdir("known");
        record(&dir, &key());
        assert!(policy(&dir, StrictHostKeyChecking::Yes).verify(&key()));
    }

    #[test]
    fn an_unknown_host_is_refused_when_strict() {
        let dir = tempdir("strict");
        assert!(!policy(&dir, StrictHostKeyChecking::Yes).verify(&key()));
    }

    #[test]
    fn accept_new_records_the_key_for_next_time() {
        let dir = tempdir("learn");
        assert!(policy(&dir, StrictHostKeyChecking::AcceptNew).verify(&key()));
        assert!(policy(&dir, StrictHostKeyChecking::Yes).verify(&key()));
    }

    #[test]
    fn a_changed_key_is_refused_even_with_accept_new() {
        let dir = tempdir("changed");
        record(&dir, &ed25519(2));
        assert!(!policy(&dir, StrictHostKeyChecking::AcceptNew).verify(&key()));
    }
}
