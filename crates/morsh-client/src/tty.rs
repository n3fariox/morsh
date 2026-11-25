//! The local terminal's mode while the client runs: raw input, the
//! alternate screen (unless `MOSH_NO_TERM_INIT` is set), the cursor hidden. Taken over once, and handed back on
//! every way out (normal exit, error, panic) and around a suspend.

use crossterm::{cursor, queue, terminal};
use std::ffi::OsStr;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// The terminal is in our mode and needs handing back. Shared with the
/// panic hook, so the terminal is handed back once, not again as the
/// panic unwinds past the `Tty`: a second "leave the alternate screen"
/// moves the cursor back over the panic message.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether we switch to the alternate screen; set once by [`Tty::enter`].
static ALTERNATE_SCREEN: AtomicBool = AtomicBool::new(true);

/// Like stock mosh, `MOSH_NO_TERM_INIT` set to anything (the wrapper's
/// `--no-init`) keeps the session on the main screen, so its output stays
/// in the local scrollback.
fn use_alternate_screen(no_term_init: Option<&OsStr>) -> bool {
    no_term_init.is_none()
}

/// What takes the screen over. Without the alternate screen, clear the main
/// one so it matches the blank screen the first frame is diffed against
/// (mosh's first frame clears it the same way).
fn write_init(out: &mut impl Write, alternate_screen: bool) -> io::Result<()> {
    if alternate_screen {
        queue!(out, terminal::EnterAlternateScreen)?;
    } else {
        queue!(
            out,
            cursor::MoveTo(0, 0),
            terminal::Clear(terminal::ClearType::All)
        )?;
    }
    queue!(out, cursor::Hide)?;
    out.flush()
}

/// What hands the screen back.
fn write_deinit(out: &mut impl Write, alternate_screen: bool) -> io::Result<()> {
    queue!(out, cursor::Show)?;
    if alternate_screen {
        queue!(out, terminal::LeaveAlternateScreen)?;
    }
    out.flush()
}

pub struct Tty {
    #[cfg(windows)]
    console_mode: Option<u32>,
    #[cfg(windows)]
    input_mode: Option<u32>,
}

impl Tty {
    /// Take over the terminal. It is handed back when the `Tty` is dropped,
    /// and by a panic, so the message is readable and the shell usable.
    pub fn enter() -> io::Result<Self> {
        let mut tty = Self {
            #[cfg(windows)]
            console_mode: None,
            #[cfg(windows)]
            input_mode: None,
        };
        ALTERNATE_SCREEN.store(
            use_alternate_screen(std::env::var_os("MOSH_NO_TERM_INIT").as_deref()),
            Ordering::SeqCst,
        );
        tty.take_over()?;
        restore_on_panic();
        Ok(tty)
    }

    fn take_over(&mut self) -> io::Result<()> {
        terminal::enable_raw_mode()?;
        // From here on there is something to undo, even if a step fails.
        ACTIVE.store(true, Ordering::SeqCst);
        #[cfg(windows)]
        {
            self.console_mode = enable_delayed_wrap();
            self.input_mode = crate::input::enable_vt_input();
        }
        write_init(&mut io::stdout(), ALTERNATE_SCREEN.load(Ordering::SeqCst))
    }

    /// Hand the terminal back as we found it. Does nothing if it already has
    /// been.
    pub fn restore(&mut self) {
        if !ACTIVE.swap(false, Ordering::SeqCst) {
            return;
        }
        let _ = write_deinit(&mut io::stdout(), ALTERNATE_SCREEN.load(Ordering::SeqCst));
        // The saved input mode is the raw one, so put it back before leaving raw mode.
        #[cfg(windows)]
        crate::input::restore_input_mode(self.input_mode.take());
        let _ = terminal::disable_raw_mode();
        #[cfg(windows)]
        restore_console_mode(self.console_mode.take());
    }
}

#[cfg(unix)]
impl Tty {
    /// Take the terminal over again after the process was stopped and
    /// continued: whoever had it meanwhile (the shell, for one) will have put
    /// it back in cooked mode and on the main screen. The caller must
    /// repaint. Mosh's `STMClient::resume`.
    pub fn resume(&mut self) -> io::Result<()> {
        // crossterm skips enabling raw mode while it thinks it is still on.
        // Forgetting it this way writes nothing, so a resume of a terminal
        // that never left doesn't flicker.
        let _ = terminal::disable_raw_mode();
        self.take_over()
    }

    /// Hand the terminal back and stop the process group (as mosh does)
    /// until the shell continues it, then take the terminal over again. The
    /// caller must repaint. The escape key's Ctrl-Z.
    pub fn suspend(&mut self) -> io::Result<()> {
        self.restore();
        let mut out = io::stdout();
        out.write_all(b"\n\x1b[37;44m[morsh is suspended.]\x1b[m\n")?;
        out.flush()?;
        // SAFETY: kill(2) has no memory-safety preconditions.
        unsafe {
            libc::kill(0, libc::SIGSTOP);
        }
        self.resume()
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Hand the terminal back before a panic message is printed: in raw mode on
/// the alternate screen it would be garbled, then wiped.
fn restore_on_panic() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if ACTIVE.swap(false, Ordering::SeqCst) {
            let _ = write_deinit(&mut io::stdout(), ALTERNATE_SCREEN.load(Ordering::SeqCst));
            let _ = terminal::disable_raw_mode();
        }
        default(info);
    }));
}

/// Windows console: make the cursor wait at the right margin instead of
/// wrapping as soon as the last column is written (which scrolls the screen
/// when on the bottom row). Returns the original mode to restore on exit.
#[cfg(windows)]
fn enable_delayed_wrap() -> Option<u32> {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, DISABLE_NEWLINE_AUTO_RETURN,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
    };
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return None;
        }
        let new_mode = mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN;
        if SetConsoleMode(handle, new_mode) == 0 {
            tracing::warn!("Failed to set console mode {new_mode:#x}");
            return None;
        }
        Some(mode)
    }
}

#[cfg(windows)]
fn restore_console_mode(mode: Option<u32>) {
    use windows_sys::Win32::System::Console::{GetStdHandle, SetConsoleMode, STD_OUTPUT_HANDLE};
    if let Some(mode) = mode {
        unsafe {
            SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), mode);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(f: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> String {
        let mut out = Vec::new();
        f(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn mosh_no_term_init_keeps_the_main_screen() {
        for (setting, alternate) in [(None, true), (Some(""), false), (Some("1"), false)] {
            let alt = use_alternate_screen(setting.map(OsStr::new));
            assert_eq!(alt, alternate, "{setting:?}");
            let init = written(|out| write_init(out, alt));
            let deinit = written(|out| write_deinit(out, alt));
            assert_eq!(
                init.contains("\x1b[?1049h"),
                alternate,
                "{setting:?}: {init:?}"
            );
            assert_eq!(
                deinit.contains("\x1b[?1049l"),
                alternate,
                "{setting:?}: {deinit:?}"
            );
            // Either way the cursor is hidden while we run and shown after.
            assert!(init.contains("\x1b[?25l") && deinit.contains("\x1b[?25h"));
        }
        // On the main screen, start from a blank one, as the model assumes.
        assert!(written(|out| write_init(out, false)).contains("\x1b[2J"));
    }
}
