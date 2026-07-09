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
