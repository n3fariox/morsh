//! Terminal emulation adapter for mosh-rust.
//!
//! Wraps `libghostty-vt` to provide the VT sequence processing needed
//! by Mosh's state synchronization protocol.

use libghostty_vt::{Terminal, TerminalOptions};

/// The terminal emulator, wrapping libghostty-vt.
pub struct MoshTerminal {
    terminal: Terminal<'static, 'static>,
    cols: u16,
    rows: u16,
    frame: u64,
}

impl MoshTerminal {
    /// Create a new terminal with the given dimensions.
    pub fn new(cols: u16, rows: u16, max_scrollback: usize) -> Result<Self, String> {
        let terminal = Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback,
        })
        .map_err(|e| format!("Failed to create terminal: {e:?}"))?;

        Ok(Self {
            terminal,
            cols,
            rows,
            frame: 0,
        })
    }

    /// Feed raw VT output bytes from the server (or PTY) into the terminal.
    pub fn write(&mut self, data: &[u8]) {
        self.terminal.vt_write(data);
    }

    /// Resize the terminal (cell_width_px/cell_height_px default to 0 when unknown).
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        self.cols = cols;
        self.rows = rows;
        self.terminal
            .resize(cols, rows, 0, 0)
            .map_err(|e| format!("Resize failed: {e:?}"))
    }

    /// Get current terminal dimensions.
    pub fn dimensions(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Increment and return the frame counter.
    pub fn next_frame(&mut self) -> u64 {
        let f = self.frame;
        self.frame += 1;
        f
    }

    /// Access the underlying libghostty-vt Terminal.
    pub fn inner(&self) -> &Terminal<'static, 'static> {
        &self.terminal
    }

    /// Access the underlying libghostty-vt Terminal mutably.
    pub fn inner_mut(&mut self) -> &mut Terminal<'static, 'static> {
        &mut self.terminal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_terminal() {
        let term = MoshTerminal::new(80, 24, 10000);
        assert!(term.is_ok());
        let term = term.unwrap();
        assert_eq!(term.dimensions(), (80, 24));
    }

    #[test]
    fn write_vt_data() {
        let mut term = MoshTerminal::new(80, 24, 10000).unwrap();
        term.write(b"Hello, world!\r\n");
    }

    #[test]
    fn resize_terminal() {
        let mut term = MoshTerminal::new(80, 24, 10000).unwrap();
        assert_eq!(term.dimensions(), (80, 24));
        term.resize(120, 40).unwrap();
        assert_eq!(term.dimensions(), (120, 40));
    }

    #[test]
    fn frame_counter() {
        let mut term = MoshTerminal::new(80, 24, 10000).unwrap();
        assert_eq!(term.next_frame(), 0);
        assert_eq!(term.next_frame(), 1);
    }
}
