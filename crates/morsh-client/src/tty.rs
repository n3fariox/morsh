//! The local terminal's mode while the client runs: raw input, the
//! alternate screen, the cursor hidden. Taken over once, and handed back on
//! every way out (normal exit, error, panic) and around a suspend.

use crossterm::{cursor, execute, terminal};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

/// The terminal is in our mode and needs handing back. Shared with the
/// panic hook, so the terminal is handed back once, not again as the
/// panic unwinds past the `Tty`: a second "leave the alternate screen"
/// moves the cursor back over the panic message.
static ACTIVE: AtomicBool = AtomicBool::new(false);

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
        execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)
    }

    /// Hand the terminal back as we found it. Does nothing if it already has
    /// been.
    pub fn restore(&mut self) {
        if !ACTIVE.swap(false, Ordering::SeqCst) {
            return;
        }
        let _ = execute!(io::stdout(), cursor::Show, terminal::LeaveAlternateScreen);
        // The saved input mode is the raw one, so put it back before leaving raw mode.
        #[cfg(windows)]
        crate::input::restore_input_mode(self.input_mode.take());
        let _ = terminal::disable_raw_mode();
        #[cfg(windows)]
        restore_console_mode(self.console_mode.take());
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
            let _ = execute!(io::stdout(), cursor::Show, terminal::LeaveAlternateScreen);
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
