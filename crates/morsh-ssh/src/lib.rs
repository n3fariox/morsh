use std::io::{BufRead, Write};
use std::os::fd::FromRawFd;
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
    /// Connect to `host:port` using the given username and private key path.
    pub fn connect(
        addr: String,
        user: impl Into<String> + Send,
        key_path: impl AsRef<std::path::Path> + Send,
    ) -> Result<Self> {
        let user = user.into();
        let key_path = key_path.as_ref().to_path_buf();

        let rt = tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?;

        let session = rt.block_on(async move {
            let key_pair = load_key_with_passphrase_prompt(&key_path)
                .context("Failed to load SSH private key")?;

            let config = Arc::new(client::Config {
                inactivity_timeout: Some(Duration::from_secs(30)),
                ..<_>::default()
            });

            let mut session = client::connect(config, addr, SshHandler)
                .await
                .context("Failed to connect to SSH server")?;

            let auth_res = session
                .authenticate_publickey(
                    user,
                    PrivateKeyWithHashAlg::new(
                        Arc::new(key_pair),
                        session.best_supported_rsa_hash().await?.flatten(),
                    ),
                )
                .await
                .context("Publickey authentication failed")?;

            if !auth_res.success() {
                bail!("SSH connection was refused (authentication check failed)");
            }

            Ok::<_, anyhow::Error>(session)
        })?;

        Ok(Self {
            session: Some(session),
            rt,
        })
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

fn load_key_with_passphrase_prompt(path: &std::path::Path) -> Result<PrivateKey> {
    match load_secret_key(path, None) {
        Ok(kp) => return Ok(kp),
        Err(keys::Error::KeyIsEncrypted) => {}
        Err(e) => return Err(e.into()),
    }

    let passphrase = prompt_passphrase(path)?;
    load_secret_key(path, Some(&passphrase)).context("Failed to decrypt key with provided passphrase; is the passphrase correct?")
}

fn prompt_passphrase(path: &std::path::Path) -> Result<String> {
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

impl Drop for SshSession {
    fn drop(&mut self) {
        let _ = self.disconnect();
    }
}
