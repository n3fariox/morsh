use portable_pty::{CommandBuilder, ExitStatus, PtySize};
use std::io::Read;
use tokio::sync::mpsc;

pub enum PtyEvent {
    Output(Vec<u8>),
    Exited(ExitStatus),
}

pub struct MorshPty {
    pub rx: mpsc::Receiver<PtyEvent>,
    pub child: Box<dyn portable_pty::Child + Send>,
    pub master: Box<dyn portable_pty::MasterPty + Send>,
    pub writer: Box<dyn std::io::Write + Send>,
}

pub fn spawn_pty(
    pty_size: PtySize,
    shell: &str,
    command_args: &[String],
    locale_vars: &[(String, String)],
) -> Result<MorshPty, Box<dyn std::error::Error>> {
    let mut cmd = if command_args.is_empty() {
        CommandBuilder::new(shell)
    } else {
        let full_cmd = command_args.join(" ");
        #[cfg(windows)]
        let mut c = CommandBuilder::new("cmd.exe");
        #[cfg(not(windows))]
        let mut c = CommandBuilder::new("/bin/sh");
        #[cfg(windows)]
        c.arg("/c");
        #[cfg(not(windows))]
        c.arg("-c");
        c.arg(full_cmd);
        c
    };
    cmd.env("TERM", "xterm-256color");
    cmd.env("LANG", "en_US.UTF-8");
    cmd.env("MORSH", env!("CARGO_PKG_VERSION"));
    for (name, value) in locale_vars {
        cmd.env(name, value);
    }

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(pty_size)
        .map_err(|e| format!("Failed to open PTY: {e}"))?;

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("Failed to spawn shell: {e}"))?;
    std::mem::drop(pair.slave);

    log::info!("Spawned shell: {shell}");

    let (pty_tx, pty_rx) = mpsc::channel::<PtyEvent>(64);

    let mut pty_reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("Failed to clone PTY reader: {e}"))?;

    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) => {
                    let _ = pty_tx.blocking_send(PtyEvent::Exited(
                        ExitStatus::with_exit_code(0),
                    ));
                    break;
                }
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    if pty_tx.blocking_send(PtyEvent::Output(data)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("Failed to get PTY writer: {e}"))?;

    Ok(MorshPty {
        rx: pty_rx,
        child,
        master: pair.master,
        writer,
    })
}
