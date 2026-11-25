use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use russh::keys::{self, *};
use russh::*;

mod host_keys;
mod output;
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
    /// Connect as `user` using the caller's address filtering and preference
    /// order, offering each private key and verifying the server against
    /// `host_keys`. A passphrase is only requested after unencrypted keys fail.
    pub fn connect_addresses(
        addresses: Vec<std::net::SocketAddr>,
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

            let socket = connect_tcp(&addresses).await?;
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
            .context("SSH session is disconnected")?;

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

            let code = output::exit_status(code)?;
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
            .context("SSH session is disconnected")?;

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
            let mut lines = output::Lines::default();

            loop {
                let Some(msg) = channel.wait().await else {
                    break;
                };
                match msg {
                    ChannelMsg::Data { data } => {
                        lines.push(&data, &mut handler);
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

            lines.finish(&mut handler);
            output::exit_status(code)
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

/// Try resolved addresses in the caller's order; do not change its family policy.
async fn connect_tcp(addresses: &[std::net::SocketAddr]) -> Result<tokio::net::TcpStream> {
    let mut last_err = None;
    for &address in addresses {
        match tokio::net::TcpStream::connect(address).await {
            Ok(socket) => return Ok(socket),
            Err(error) => last_err = Some((address, error)),
        }
    }
    match last_err {
        Some((address, error)) => {
            Err(error).with_context(|| format!("Failed to connect to {address}"))
        }
        None => bail!("No addresses found for SSH connection"),
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
    let passphrase =
        rpassword::prompt_password(format!("Enter passphrase for {}: ", path.display()))
            .context("Cannot prompt for SSH key passphrase")?;
    load_secret_key(path, Some(&passphrase))
        .context("Failed to decrypt key with provided passphrase; is the passphrase correct?")
}

impl Drop for SshSession {
    fn drop(&mut self) {
        let _ = self.disconnect();
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;

    #[tokio::test]
    async fn caller_address_order_is_preserved() {
        let first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let preferred = second.local_addr().unwrap();
        let socket = connect_tcp(&[preferred, first.local_addr().unwrap()])
            .await
            .unwrap();
        assert_eq!(socket.peer_addr().unwrap(), preferred);
        assert!(connect_tcp(&[]).await.is_err());
    }
}
