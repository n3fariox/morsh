use mosh_terminal::ScreenSnapshot;

/// Represents the complete terminal state (server side).
///
/// Tracks a screen snapshot and provides diff_from() / apply_string()
/// for the state synchronization protocol.
#[derive(Clone)]
pub struct Complete {
    snapshot: ScreenSnapshot,
    echo_ack: u64,
}

impl Complete {
    pub fn new(cols: u16, rows: u16) -> Result<Self, String> {
        let snapshot = ScreenSnapshot::new(cols, rows);
        Ok(Self { snapshot, echo_ack: 0 })
    }

    /// Compute the VT escape sequences needed to transform `existing` into `self`.
    pub fn diff_from(&self, existing: &Complete) -> Vec<u8> {
        use mosh_terminal::DisplayDiff;
        let vt = DisplayDiff::diff(&existing.snapshot, &self.snapshot);
        vt.into_bytes()
    }

    /// Apply a VT byte string to update the terminal state.
    pub fn apply_string(&mut self, diff: &[u8]) {
        // Simple VT parser: apply escape sequences to the snapshot
        apply_vt_to_snapshot(&mut self.snapshot, diff);
    }

    /// Get the echo acknowledgment number.
    pub fn echo_ack(&self) -> u64 {
        self.echo_ack
    }

    /// Set the echo acknowledgment number.
    pub fn set_echo_ack(&mut self, ack: u64) {
        self.echo_ack = ack;
    }

    /// Get the current snapshot for comparison.
    pub fn snapshot(&self) -> &ScreenSnapshot {
        &self.snapshot
    }

    /// Get mutable access to the snapshot.
    pub fn snapshot_mut(&mut self) -> &mut ScreenSnapshot {
        &mut self.snapshot
    }

    /// Get the current terminal dimensions.
    pub fn dimensions(&self) -> (u16, u16) {
        (self.snapshot.cols, self.snapshot.rows_count)
    }
}

/// Minimal VT parser that applies escape sequences to a ScreenSnapshot.
///
/// This handles the common VT sequences that Mosh's diff engine produces:
/// - CUP (cursor position): ESC [ Pl ; Pc H
/// - SGR (graphics): ESC [ ... m
/// - ED (erase display): ESC [ 2 J
/// - EL (erase line): ESC [ 2 K
/// - Text output
fn apply_vt_to_snapshot(snap: &mut ScreenSnapshot, data: &[u8]) {
    let mut i = 0;
    let mut cursor_x = snap.cursor_x as usize;
    let mut cursor_y = snap.cursor_y as usize;
    let mut current_fg = mosh_terminal::StyleColor::Default;
    let mut current_bg = mosh_terminal::StyleColor::Default;
    let mut bold = false;
    let mut italic = false;
    let mut underline = false;

    while i < data.len() {
        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'[' {
            // CSI sequence
            i += 2;
            let mut params = Vec::new();
            let mut param_str = String::new();

            while i < data.len() && (data[i].is_ascii_digit() || data[i] == b';') {
                if data[i] == b';' {
                    params.push(param_str.parse::<u16>().unwrap_or(0));
                    param_str.clear();
                } else {
                    param_str.push(data[i] as char);
                }
                i += 1;
            }
            if !param_str.is_empty() {
                params.push(param_str.parse::<u16>().unwrap_or(0));
            }

            // Handle DEC private mode: ESC [ ? <params> <final>
            // The '?' prefix was not consumed in the param loop (it's not a digit/';').
            if i < data.len() && data[i] == b'?' {
                i += 1; // skip '?'
                // Skip mode number digits
                while i < data.len() && data[i].is_ascii_digit() {
                    i += 1;
                }
                // Skip final character (h/l for set/reset)
                if i < data.len() {
                    i += 1;
                }
            } else if i < data.len() {
                match data[i] {
                    b'H' | b'f' => {
                        // CUP: cursor position (1-based)
                        let row = params.first().copied().unwrap_or(1).saturating_sub(1) as usize;
                        let col = params.get(1).copied().unwrap_or(1).saturating_sub(1) as usize;
                        cursor_y = row.min(snap.rows_count as usize - 1);
                        cursor_x = col.min(snap.cols as usize - 1);
                    }
                    b'm' => {
                        // SGR: set graphics mode
                        if params.is_empty() {
                            // ESC[m = reset
                            current_fg = mosh_terminal::StyleColor::Default;
                            current_bg = mosh_terminal::StyleColor::Default;
                            bold = false;
                            italic = false;
                            underline = false;
                        } else {
                            for &p in &params {
                                match p {
                                    0 => {
                                        current_fg = mosh_terminal::StyleColor::Default;
                                        current_bg = mosh_terminal::StyleColor::Default;
                                        bold = false;
                                        italic = false;
                                        underline = false;
                                    }
                                    1 => bold = true,
                                    3 => italic = true,
                                    4 => underline = true,
                                    22 => bold = false,
                                    23 => italic = false,
                                    24 => underline = false,
                                    38 => {
                                        // FG color: ESC[38;5;N or ESC[38;2;R;G;B
                                        if i + 1 < data.len() && data[i + 1] == b';' {
                                            i += 2;
                                            if i < data.len() && data[i] == b'5' {
                                                i += 1; // skip '5'
                                                if i < data.len() && data[i] == b';' {
                                                    i += 1;
                                                    let mut idx_str = String::new();
                                                    while i < data.len() && data[i].is_ascii_digit() {
                                                        idx_str.push(data[i] as char);
                                                        i += 1;
                                                    }
                                                    let idx = idx_str.parse::<u8>().unwrap_or(0);
                                                    current_fg = mosh_terminal::StyleColor::Palette(idx);
                                                    if i < data.len() && data[i] == b'm' { i += 1; }
                                                    continue;
                                                }
                                            } else if i < data.len() && data[i] == b'2' {
                                                i += 1; // skip '2'
                                                // Parse R;G;B
                                                let mut rgb = [0u8; 3];
                                                for k in 0..3 {
                                                    if i < data.len() && data[i] == b';' {
                                                        i += 1;
                                                    }
                                                    let mut val_str = String::new();
                                                    while i < data.len() && data[i].is_ascii_digit() {
                                                        val_str.push(data[i] as char);
                                                        i += 1;
                                                    }
                                                    rgb[k] = val_str.parse::<u8>().unwrap_or(0);
                                                }
                                                current_fg = mosh_terminal::StyleColor::Rgb(mosh_terminal::RgbColor { r: rgb[0], g: rgb[1], b: rgb[2] });
                                                if i < data.len() && data[i] == b'm' { i += 1; }
                                                continue;
                                            }
                                        }
                                    }
                                    48 => {
                                        // BG color: same as FG but for background
                                        if i + 1 < data.len() && data[i + 1] == b';' {
                                            i += 2;
                                            if i < data.len() && data[i] == b'5' {
                                                i += 1;
                                                if i < data.len() && data[i] == b';' {
                                                    i += 1;
                                                    let mut idx_str = String::new();
                                                    while i < data.len() && data[i].is_ascii_digit() {
                                                        idx_str.push(data[i] as char);
                                                        i += 1;
                                                    }
                                                    let idx = idx_str.parse::<u8>().unwrap_or(0);
                                                    current_bg = mosh_terminal::StyleColor::Palette(idx);
                                                    if i < data.len() && data[i] == b'm' { i += 1; }
                                                    continue;
                                                }
                                            } else if i < data.len() && data[i] == b'2' {
                                                i += 1;
                                                let mut rgb = [0u8; 3];
                                                for k in 0..3 {
                                                    if i < data.len() && data[i] == b';' { i += 1; }
                                                    let mut val_str = String::new();
                                                    while i < data.len() && data[i].is_ascii_digit() {
                                                        val_str.push(data[i] as char);
                                                        i += 1;
                                                    }
                                                    rgb[k] = val_str.parse::<u8>().unwrap_or(0);
                                                }
                                                current_bg = mosh_terminal::StyleColor::Rgb(mosh_terminal::RgbColor { r: rgb[0], g: rgb[1], b: rgb[2] });
                                                if i < data.len() && data[i] == b'm' { i += 1; }
                                                continue;
                                            }
                                        }
                                    }
                                    _ => {} // Ignore other SGR codes for now
                                }
                            }
                        }
                    }
                    b'J' => {
                        // ED: erase display
                        if params.first() == Some(&2) {
                            // Clear entire screen
                            for row in &mut snap.rows {
                                for cell in row {
                                    *cell = mosh_terminal::CellData::empty();
                                }
                            }
                            cursor_x = 0;
                            cursor_y = 0;
                        }
                    }
                    b'K' => {
                        // EL: erase line
                        if let Some(row) = snap.rows.get_mut(cursor_y) {
                            for cell in row.iter_mut().skip(cursor_x) {
                                *cell = mosh_terminal::CellData::empty();
                            }
                        }
                    }
                    _ => {} // Ignore other CSI sequences
                }
                i += 1;
            }
        } else if data[i] == b'\r' {
            cursor_x = 0;
            i += 1;
        } else if data[i] == b'\n' {
            cursor_y = (cursor_y + 1).min(snap.rows_count as usize - 1);
            i += 1;
        } else if data[i] == b'\x08' {
            // Backspace
            cursor_x = cursor_x.saturating_sub(1);
            i += 1;
        } else {
            // Text character
            let ch = data[i] as char;
            if cursor_y < snap.rows.len() && cursor_x < snap.cols as usize {
                let style = mosh_terminal::CellStyle {
                    fg: current_fg,
                    bg: current_bg,
                    bold,
                    italic,
                    underline,
                    ..mosh_terminal::CellStyle::default_style()
                };
                snap.rows[cursor_y][cursor_x] = mosh_terminal::CellData {
                    text: ch.to_string(),
                    wide: false,
                    style,
                };
            }
            cursor_x += 1;
            if cursor_x >= snap.cols as usize {
                cursor_x = 0;
                cursor_y = (cursor_y + 1).min(snap.rows_count as usize - 1);
            }
            i += 1;
        }
    }

    snap.cursor_x = cursor_x as u16;
    snap.cursor_y = cursor_y as u16;
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
        let mut with_text = Complete::new(80, 1).unwrap();
        with_text.apply_string(b"Hi");

        let diff = empty.diff_from(&with_text);
        assert!(!diff.is_empty());
    }

    #[test]
    fn diff_same_states() {
        let mut a = Complete::new(80, 1).unwrap();
        a.apply_string(b"Hello");
        let mut b = Complete::new(80, 1).unwrap();
        b.apply_string(b"Hello");

        let diff = a.diff_from(&b);
        // Should be empty or minimal (just cursor positioning)
        assert!(diff.len() < 10);
    }

    #[test]
    fn dec_private_mode_not_visible() {
        // \x1b[?2004l = disable bracketed paste mode
        // This should be consumed by the VT parser, not written as visible text
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
