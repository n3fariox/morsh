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

    tracing::info!("Spawned shell: {shell}");

    let (pty_tx, pty_rx) = mpsc::channel::<PtyEvent>(64);
    // Clone pty_tx so the Windows exit-watcher thread can send
    // PtyEvent::Exited through the same channel as the reader thread.
    #[cfg(windows)]
    let pty_tx_exit = pty_tx.clone();

    let mut pty_reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("Failed to clone PTY reader: {e}"))?;

    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) => {
                    tracing::info!("PTY reader: EOF");
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
                Err(e) => {
                    tracing::info!("PTY reader: read error (likely slave closed): {e}");
                    let _ = pty_tx.blocking_send(PtyEvent::Exited(
                        ExitStatus::with_exit_code(0),
                    ));
                    break;
                }
            }
        }
    });
    // On Windows, the ConPTY output pipe write end never closes (the
    // HPCON inside pair.master stays alive for resize), so the reader
    // thread can't detect child exit via EOF/error.  Wait on the child
    // process handle directly and send PtyEvent::Exited when it signals.
    #[cfg(windows)]
    {
        match child.as_raw_handle() {
            Some(handle) => {
                tracing::info!("Exit watcher: got child process handle");
                use windows_sys::Win32::Foundation::{
                    CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE, STILL_ACTIVE,
                };
                use windows_sys::Win32::System::Threading::{
                    GetCurrentProcess, GetExitCodeProcess, WaitForSingleObject, INFINITE,
                };
                let mut dup: HANDLE = unsafe { std::mem::zeroed() };
                let ok = unsafe {
                    DuplicateHandle(
                        GetCurrentProcess(),
                        handle,
                        GetCurrentProcess(),
                        &mut dup,
                        0,
                        0,
                        DUPLICATE_SAME_ACCESS,
                    )
                };
                if ok != 0 {
                    tracing::info!("Exit watcher: DuplicateHandle succeeded, spawning watcher thread");
                    // HANDLE = *mut c_void which is !Send; cast through usize.
                    let dup_raw = dup as usize;
                    std::thread::spawn(move || {
                        let dup = dup_raw as HANDLE;
                        tracing::info!("Exit watcher: waiting on child process handle");
                        unsafe { WaitForSingleObject(dup, INFINITE); }
                        tracing::info!("Exit watcher: child process handle signaled");
                        let mut exit_code: u32 = 0;
                        unsafe { GetExitCodeProcess(dup, &mut exit_code); }
                        if exit_code == STILL_ACTIVE as u32 {
                            exit_code = 0;
                        }
                        tracing::info!("Exit watcher: sending PtyEvent::Exited (code={})", exit_code);
                        let _ = pty_tx_exit.blocking_send(PtyEvent::Exited(
                            ExitStatus::with_exit_code(exit_code),
                        ));
                        unsafe { CloseHandle(dup); }
                    });
                } else {
                    tracing::info!("Exit watcher: DuplicateHandle failed, relying on keepalive timer");
                }
            }
            None => {
                tracing::info!("Exit watcher: child.as_raw_handle() returned None");
            }
        }
    }

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
