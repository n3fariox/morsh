use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use russh::keys::{self, *};
use russh::*;

/// A blocking SSH session that executes commands on a remote host.
///
/// Runs a hidden tokio runtime internally so callers don't need to be async.
pub struct SshSession {
    session: Option<client::Handle<SshHandler>>,
    rt: tokio::runtime::Runtime,
    peer: std::net::SocketAddr,
}

struct SshHandler;

impl client::Handler for SshHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

impl SshSession {
    /// Connect to `host:port` as `user`, offering each private key in turn
    /// the way ssh does.
    ///
    /// Unencrypted keys are tried first; a passphrase is only asked for once
    /// none of them was accepted, so an unused encrypted key never prompts.
    pub fn connect(
        addr: String,
        user: impl Into<String> + Send,
        key_paths: &[std::path::PathBuf],
    ) -> Result<Self> {
        let user = user.into();

        let rt = tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?;

        let (session, peer) = rt.block_on(async move {
            let config = Arc::new(client::Config {
                inactivity_timeout: Some(Duration::from_secs(30)),
                ..<_>::default()
            });

            let socket = connect_tcp(&addr).await?;
            let peer = socket.peer_addr()?;
            let _ = socket.set_nodelay(true);

            let mut session = client::connect_stream(config, socket, SshHandler)
                .await
                .context("Failed to connect to SSH server")?;

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

    let passphrase = prompt_passphrase(path)?;
    load_secret_key(path, Some(&passphrase)).context("Failed to decrypt key with provided passphrase; is the passphrase correct?")
}

#[cfg(unix)]
fn prompt_passphrase(path: &std::path::Path) -> Result<String> {
    use std::io::{BufRead, Write};
    use std::os::fd::FromRawFd;

    let tty_fd = unsafe { libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR) };
    if tty_fd < 0 {
        bail!("Cannot open /dev/tty to prompt for SSH key passphrase");
    }

    let prompt = format!("Enter passphrase for {}: ", path.display());
    let _ = unsafe {
        libc::write(tty_fd, prompt.as_ptr().cast(), prompt.len())
    };

    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(tty_fd, termios.as_mut_ptr()) } != 0 {
        unsafe { libc::close(tty_fd) };
        bail!("tcgetattr failed");
    }
    let mut termios = unsafe { termios.assume_init() };
    let old_lflag = termios.c_lflag;
    termios.c_lflag &= !libc::ECHO;
    unsafe { libc::tcsetattr(tty_fd, libc::TCSANOW, &termios) };

    let tty_file = unsafe { std::fs::File::from_raw_fd(tty_fd) };
    let mut reader = std::io::BufReader::new(tty_file);
    let mut passphrase = String::new();
    reader.read_line(&mut passphrase).ok();
    drop(reader); // closes fd via File drop

    // Restore echo using a fresh /dev/tty handle.
    termios.c_lflag = old_lflag;
    let restore_fd = unsafe { libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR) };
    if restore_fd >= 0 {
        unsafe { libc::tcsetattr(restore_fd, libc::TCSANOW, &termios) };
        unsafe { libc::close(restore_fd) };
    }

    writeln!(std::io::stderr()).ok();
    Ok(passphrase.trim_end_matches('\n').to_string())
}

#[cfg(not(unix))]
fn prompt_passphrase(path: &std::path::Path) -> Result<String> {
    use std::io::Write;
    let prompt = format!("Enter passphrase for {}: ", path.display());
    print!("{}", prompt);
    std::io::stdout().flush().ok();
    let mut passphrase = String::new();
    std::io::stdin().read_line(&mut passphrase).ok();
    Ok(passphrase.trim_end_matches('\n').to_string())
}

impl Drop for SshSession {
    fn drop(&mut self) {
        let _ = self.disconnect();
    }
}
