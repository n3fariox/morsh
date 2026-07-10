use morsh_terminal::{DisplayDiff, MoshTerminal, ScreenSnapshot};

/// Represents the complete terminal state.
///
/// Wraps a ghostty-vt terminal emulator and provides diff_from() / apply_string()
/// for the state synchronization protocol. VT parsing is handled entirely by
/// ghostty-vt — no hand-written parser.
pub struct Complete {
    terminal: MoshTerminal,
    echo_ack: u64,
}

impl Complete {
    pub fn new(cols: u16, rows: u16) -> Result<Self, String> {
        let terminal = MoshTerminal::new(cols, rows, 0)?;
        Ok(Self {
            terminal,
            echo_ack: 0,
        })
    }

    /// Compute the VT escape sequences needed to transform `old_state` into
    /// the current terminal state.
    pub fn diff_from(&self, old_state: &ScreenSnapshot) -> Vec<u8> {
        let current = self.terminal.snapshot().unwrap_or_else(|_| {
            ScreenSnapshot::new(self.terminal.dimensions().0, self.terminal.dimensions().1)
        });
        DisplayDiff::diff(old_state, &current).into_bytes()
    }

    /// Apply a VT byte string to update the terminal state.
    pub fn apply_string(&mut self, diff: &[u8]) {
        self.terminal.write(diff);
    }

    /// Get the echo acknowledgment number.
    pub fn echo_ack(&self) -> u64 {
        self.echo_ack
    }

    /// Set the echo acknowledgment number.
    pub fn set_echo_ack(&mut self, ack: u64) {
        self.echo_ack = ack;
    }

    /// Get the current terminal state as a snapshot.
    pub fn snapshot(&self) -> ScreenSnapshot {
        self.terminal
            .snapshot()
            .unwrap_or_else(|_| {
                ScreenSnapshot::new(self.terminal.dimensions().0, self.terminal.dimensions().1)
            })
    }

    /// Get the current terminal dimensions.
    pub fn dimensions(&self) -> (u16, u16) {
        self.terminal.dimensions()
    }

    /// Access the underlying MoshTerminal (for resize, etc.)
    pub fn terminal(&self) -> &MoshTerminal {
        &self.terminal
    }

    /// Access the underlying MoshTerminal mutably.
    pub fn terminal_mut(&mut self) -> &mut MoshTerminal {
        &mut self.terminal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_complete() {
        let c = Complete::new(80, 24);
        assert!(c.is_ok());
        assert_eq!(c.unwrap().dimensions(), (80, 24));
    }

    #[test]
    fn apply_text_to_snapshot() {
        let mut c = Complete::new(80, 1).unwrap();
        c.apply_string(b"Hello");
        let snap = c.snapshot();
        assert_eq!(snap.rows[0][0].text, "H");
        assert_eq!(snap.rows[0][1].text, "e");
        assert_eq!(snap.rows[0][4].text, "o");
    }

    #[test]
    fn apply_cursor_positioning() {
        let mut c = Complete::new(10, 3).unwrap();
        // Move to row 2, col 3 (1-based)
        c.apply_string(b"\x1b[2;3HX");
        let snap = c.snapshot();
        assert_eq!(snap.rows[1][2].text, "X");
    }

    #[test]
    fn apply_color_escape() {
        let mut c = Complete::new(10, 1).unwrap();
        // Bold red text
        c.apply_string(b"\x1b[1;31mX\x1b[0m");
        let snap = c.snapshot();
        assert!(snap.rows[0][0].style.bold);
    }

    #[test]
    fn diff_from_empty_to_text() {
        let empty = Complete::new(80, 1).unwrap();
        let old_snap = empty.snapshot();

        let mut with_text = Complete::new(80, 1).unwrap();
        with_text.apply_string(b"Hi");

        let diff = with_text.diff_from(&old_snap);
        assert!(!diff.is_empty());
    }

    #[test]
    fn diff_same_states() {
        let mut a = Complete::new(80, 1).unwrap();
        a.apply_string(b"Hello");
        let snap_a = a.snapshot();

        let mut b = Complete::new(80, 1).unwrap();
        b.apply_string(b"Hello");

        let diff = b.diff_from(&snap_a);
        // Should be empty or minimal (just cursor positioning)
        assert!(diff.len() < 10);
    }

    #[test]
    fn dec_private_mode_not_visible() {
        // \x1b[?2004l = disable bracketed paste mode
        // This should be consumed by ghostty-vt, not written as visible text
        let mut c = Complete::new(80, 1).unwrap();
        c.apply_string(b"\x1b[?2004l");
        let snap = c.snapshot();
        // Row should be empty — the sequence is a mode change, not text
        assert!(snap.rows[0][0].text.is_empty());
    }

    #[test]
    fn dec_private_mode_with_text() {
        // Shell outputs bracketed paste mode then prints text
        let mut c = Complete::new(80, 1).unwrap();
        c.apply_string(b"\x1b[?2004h$ ");
        let snap = c.snapshot();
        assert_eq!(snap.rows[0][0].text, "$");
        assert_eq!(snap.rows[0][1].text, " ");
    }

    #[test]
    fn cursor_visibility_sequence() {
        // \x1b[?25l = hide cursor, \x1b[?25h = show cursor
        let mut c = Complete::new(10, 1).unwrap();
        c.apply_string(b"\x1b[?25l");
        // Should not write "?25l" as text
        assert!(c.snapshot().rows[0][0].text.is_empty());
    }
}
