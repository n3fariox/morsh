use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use russh::keys::{self, *};
use russh::*;

mod host_keys;
pub use host_keys::{HostKeyPolicy, StrictHostKeyChecking};

/// A blocking SSH session that executes commands on a remote host.
///
/// Runs a hidden tokio runtime internally so callers don't need to be async.
pub struct SshSession {
    session: Option<client::Handle<SshHandler>>,
    rt: tokio::runtime::Runtime,
    peer: std::net::SocketAddr,
}

struct SshHandler {
    host_keys: Arc<HostKeyPolicy>,
}

impl client::Handler for SshHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // Verifying may read files and ask the user; keep it off the runtime.
        let policy = self.host_keys.clone();
        let key = server_public_key.clone();
        let trusted = tokio::task::spawn_blocking(move || policy.verify(&key))
            .await
            .unwrap_or(false);
        tracing::debug!("host key trusted: {trusted}");
        Ok(trusted)
    }
}

impl SshSession {
    /// Connect to `host:port` as `user`, offering each private key in turn
    /// the way ssh does, and verifying the server against `host_keys`.
    ///
    /// Unencrypted keys are tried first; a passphrase is only asked for once
    /// none of them was accepted, so an unused encrypted key never prompts.
    pub fn connect(
        addr: String,
        user: impl Into<String> + Send,
        key_paths: &[std::path::PathBuf],
        host_keys: HostKeyPolicy,
    ) -> Result<Self> {
        let user = user.into();

        let rt = tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?;

        let (session, peer) = rt.block_on(async move {
            let config = Arc::new(client::Config {
                inactivity_timeout: Some(Duration::from_secs(30)),
                preferred: host_keys.preferred(),
                ..<_>::default()
            });
            let handler = SshHandler {
                host_keys: Arc::new(host_keys),
            };

            let socket = connect_tcp(&addr).await?;
            let peer = socket.peer_addr()?;
            let _ = socket.set_nodelay(true);

            let mut session = client::connect_stream(config, socket, handler)
                .await
                .context("Failed to connect to SSH server")?;
            tracing::debug!("connected to {peer}");

            let mut encrypted = Vec::new();
            for path in key_paths {
                match load_secret_key(path, None) {
                    Ok(key) => {
                        if try_key(&mut session, &user, key).await? {
                            tracing::info!("authenticated with {}", path.display());
                            return Ok((session, peer));
                        }
                    }
                    Err(keys::Error::KeyIsEncrypted) => encrypted.push(path),
                    Err(e) => tracing::info!("skipping key {}: {e}", path.display()),
                }
            }
            for path in encrypted {
                let key = load_key_with_passphrase_prompt(path)
                    .context("Failed to load SSH private key")?;
                if try_key(&mut session, &user, key).await? {
                    tracing::info!("authenticated with {}", path.display());
                    return Ok((session, peer));
                }
            }

            bail!("SSH connection was refused (no key was accepted)");
        })?;

        Ok(Self {
            session: Some(session),
            rt,
            peer,
        })
    }

    /// The address the SSH connection actually reached. Like mosh's proxy
    /// mode, the UDP session should go to this same address.
    pub fn peer_addr(&self) -> std::net::SocketAddr {
        self.peer
    }

    /// Execute a command and return all stdout + the exit code.
    ///
    /// Stderr is forwarded to local stderr.
    /// The captured output does not include a trailing newline.
    pub fn exec_raw(&mut self, command: &str) -> Result<(u32, String)> {
        let session = self
            .session
            .as_mut()
            .expect("SshSession used after disconnect");

        self.rt.block_on(async move {
            let mut channel = session
                .channel_open_session()
                .await
                .context("Failed to open SSH exec channel")?;

            // exec requires Into<Vec<u8>>; a &str satisfies that.
            channel
                .exec(true, command)
                .await
                .context("Failed to exec SSH command")?;

            let mut code = None;
            let mut output = String::new();

            loop {
                let Some(msg) = channel.wait().await else {
                    break;
                };
                match msg {
                    ChannelMsg::Data { data } => {
                        output.push_str(&String::from_utf8_lossy(&data));
                    }
                    ChannelMsg::ExtendedData { data, .. } => {
                        std::io::Write::write_all(&mut std::io::stderr(), &data).ok();
                    }
                    ChannelMsg::ExitStatus { exit_status } => {
                        code = Some(exit_status);
                    }
                    _ => {}
                }
            }

            let code = code.expect("Child exited without exit status");
            let output = output.trim_end_matches('\n').to_string();
            Ok::<_, anyhow::Error>((code, output))
        })
    }

    /// Execute a command, passing each stdout line to the given function.
    ///
    /// Stderr is forwarded to local stderr. Returns the exit code.
    pub fn exec_with_handler<H: FnMut(String) + Send + 'static>(
        &mut self,
        command: &str,
        mut handler: H,
    ) -> Result<u32> {
        let session = self
            .session
            .as_ref()
            .expect("SshSession used after disconnect");

        self.rt.block_on(async move {
            let mut channel = session
                .channel_open_session()
                .await
                .context("Failed to open SSH exec channel")?;

            channel
                .exec(true, command)
                .await
                .context("Failed to exec SSH command")?;

            let mut code = None;

            loop {
                let Some(msg) = channel.wait().await else {
                    break;
                };
                match msg {
                    ChannelMsg::Data { data } => {
                        for line in String::from_utf8_lossy(&data).lines() {
                            handler(line.to_string());
                        }
                    }
                    ChannelMsg::ExtendedData { data, .. } => {
                        std::io::Write::write_all(&mut std::io::stderr(), &data).ok();
                    }
                    ChannelMsg::ExitStatus { exit_status } => {
                        code = Some(exit_status);
                    }
                    _ => {}
                }
            }

            Ok::<u32, anyhow::Error>(code.expect("Child exited without exit status"))
        })
    }

    /// Close the SSH session gracefully.
    pub fn disconnect(&mut self) -> Result<()> {
        if let Some(session) = self.session.take() {
            self.rt.block_on(async move {
                let _ = session
                    .disconnect(Disconnect::ByApplication, "", "English")
                    .await;
                Ok::<_, anyhow::Error>(())
            })?;
        }
        Ok(())
    }
}

/// Connect to the first reachable address of `host:port`, IPv4 first
/// (mosh's default `--family=prefer-inet`).
async fn connect_tcp(addr: &str) -> Result<tokio::net::TcpStream> {
    let mut addrs: Vec<_> = tokio::net::lookup_host(addr)
        .await
        .with_context(|| format!("Failed to resolve {addr}"))?
        .collect();
    addrs.sort_by_key(|a| !a.is_ipv4());

    let mut last_err = None;
    for a in addrs {
        match tokio::net::TcpStream::connect(a).await {
            Ok(s) => return Ok(s),
            Err(e) => last_err = Some(e),
        }
    }
    match last_err {
        Some(e) => Err(e).with_context(|| format!("Failed to connect to {addr}")),
        None => bail!("No addresses found for {addr}"),
    }
}

/// Offer one key; `Ok(false)` means the server declined it and another may
/// be tried on the same session.
async fn try_key(
    session: &mut client::Handle<SshHandler>,
    user: &str,
    key: PrivateKey,
) -> Result<bool> {
    tracing::debug!("offering {} key", key.algorithm());
    let hash = session.best_supported_rsa_hash().await?.flatten();
    let auth_res = session
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
        .await
        .context("Publickey authentication failed")?;
    Ok(auth_res.success())
}

fn load_key_with_passphrase_prompt(path: &std::path::Path) -> Result<PrivateKey> {
    match load_secret_key(path, None) {
        Ok(kp) => return Ok(kp),
        Err(keys::Error::KeyIsEncrypted) => {}
        Err(e) => return Err(e.into()),
    }

    // rpassword reads from the terminal with echo off (/dev/tty on Unix, the
    // console on Windows) and strips the line ending, `\r\n` included.
    let passphrase = rpassword::prompt_password(format!("Enter passphrase for {}: ", path.display()))
        .context("Cannot prompt for SSH key passphrase")?;
    load_secret_key(path, Some(&passphrase)).context("Failed to decrypt key with provided passphrase; is the passphrase correct?")
}

impl Drop for SshSession {
    fn drop(&mut self) {
        let _ = self.disconnect();
    }
}
