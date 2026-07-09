use crate::snapshot::{CellStyle, ScreenSnapshot, StyleColor};

/// Generates VT escape sequence strings from screen snapshots.
pub struct DisplayDiff;

impl DisplayDiff {
    /// Generate VT escape sequences to render a full snapshot from scratch.
    pub fn full_redraw(snap: &ScreenSnapshot) -> String {
        let mut out = String::with_capacity(snap.cols as usize * snap.rows_count as usize * 4);
        // Hide cursor during redraw
        out.push_str("\x1b[?25l");
        // Clear screen
        out.push_str("\x1b[2J");
        // Move to top-left
        out.push_str("\x1b[H");

        let mut last_style = CellStyle::default_style();

        for (y, row) in snap.rows.iter().enumerate() {
            if y > 0 {
                out.push_str("\r\n");
            }
            for cell in row {
                if !cell.style.is_default() && cell.style != last_style {
                    out.push_str(&style_on(&cell.style));
                    last_style = cell.style.clone();
                } else if cell.style.is_default() && !last_style.is_default() {
                    out.push_str("\x1b[0m");
                    last_style = CellStyle::default_style();
                }
                if cell.text.is_empty() {
                    out.push(' ');
                } else {
                    out.push_str(&cell.text);
                }
            }
            // Reset style at end of row
            if !last_style.is_default() {
                out.push_str("\x1b[0m");
                last_style = CellStyle::default_style();
            }
        }

        // Position cursor
        out.push_str(&format!(
            "\x1b[{};{}H",
            snap.cursor_y + 1,
            snap.cursor_x + 1
        ));
        // Show/hide cursor
        if snap.cursor_visible {
            out.push_str("\x1b[?25h");
        }

        out
    }

    /// Compute minimal VT escape sequence string to transform `old` into `new`.
    ///
    /// Compares cell-by-cell and emits only the necessary changes.
    /// Uses cursor positioning and style changes to minimize output.
    ///
    /// To correctly handle escape sequences that change the terminal's internal
    /// SGR state (e.g. `\x1b[0m`) without changing visible cells between
    /// snapshots, we visit every position in each changed row and track the
    /// "pen state" — the terminal's current SGR style. When the pen state
    /// diverges from what the new snapshot expects, we emit a reset to
    /// synchronize. This catches escape sequences that happened at unchanged
    /// positions between snapshots.
    pub fn diff(old: &ScreenSnapshot, new: &ScreenSnapshot) -> String {
        if old.cols != new.cols || old.rows_count != new.rows_count {
            return Self::full_redraw(new);
        }

        let mut out = String::with_capacity(new.cols as usize * new.rows_count as usize * 2);

        for y in 0..new.rows_count as usize {
            let old_row = &old.rows[y];
            let new_row = &new.rows[y];
            let mut any_changed = false;

            // Quick check: skip unchanged rows
            for x in 0..new.cols as usize {
                if old_row.get(x) != new_row.get(x) {
                    any_changed = true;
                    break;
                }
            }
            if !any_changed {
                continue;
            }

            // Reset SGR state at start of row to establish a known baseline.
            // This ensures the pen state is synchronized before we process
            // any cells.
            let mut pen_state = CellStyle::default_style();
            out.push_str("\x1b[0m");

            // Visit EVERY position in this row to track pen state.
            // Escape sequences can change the pen state at unchanged positions,
            // so we must check for divergence even where cells are identical.
            let mut x = 0usize;
            let mut cursor_pos: Option<(usize, usize)> = None; // (y, x) of last cursor position
            while x < new.cols as usize {
                let new_cell = &new_row[x];
                let is_changed = old_row.get(x) != new_row.get(x);

                // Check if pen state diverges from what the new snapshot expects.
                if pen_state != new_cell.style {
                    // Position cursor
                    cursor_pos = Some((y + 1, x + 1));
                    out.push_str(&format!("\x1b[{};{}H", y + 1, x + 1));
                    // Reset to default, then apply new style if non-default
                    out.push_str("\x1b[0m");
                    if !new_cell.style.is_default() {
                        out.push_str(&style_on(&new_cell.style));
                    }
                    pen_state = new_cell.style.clone();
                }

                // Emit changed cell content
                if is_changed {
                    // Position cursor if not already at the right position
                    let needed = (y + 1, x + 1);
                    if cursor_pos != Some(needed) {
                        out.push_str(&format!("\x1b[{};{}H", y + 1, x + 1));
                    }
                    // After writing this char, cursor moves to x+2
                    cursor_pos = Some((y + 1, x + 2));

                    if new_cell.text.is_empty() {
                        out.push(' ');
                    } else {
                        out.push_str(&new_cell.text);
                    }
                } else {
                    // Unchanged cell — cursor is no longer tracked
                    cursor_pos = None;
                }

                pen_state = new_cell.style.clone();

                if new_cell.wide && x + 1 < new.cols as usize {
                    x += 1;
                    pen_state = new_row[x].style.clone();
                }
                x += 1;
            }
        }

        // Position cursor
        out.push_str(&format!(
            "\x1b[{};{}H",
            new.cursor_y + 1,
            new.cursor_x + 1
        ));

        // Show/hide cursor
        if old.cursor_visible != new.cursor_visible {
            if new.cursor_visible {
                out.push_str("\x1b[?25h");
            } else {
                out.push_str("\x1b[?25l");
            }
        }

        out
    }
}

/// Generate SGR escape sequence to enable the given style attributes.
fn style_on(style: &CellStyle) -> String {
    let mut params = Vec::new();

    if style.bold {
        params.push(1);
    }
    if style.faint {
        params.push(2);
    }
    if style.italic {
        params.push(3);
    }
    if style.underline {
        params.push(4);
    }
    if style.blink {
        params.push(5);
    }
    if style.inverse {
        params.push(7);
    }
    if style.invisible {
        params.push(8);
    }
    if style.strikethrough {
        params.push(9);
    }
    if style.overline {
        params.push(53);
    }

    // Foreground color
    match style.fg {
        StyleColor::Rgb(c) => {
            params.push(38);
            params.push(2);
            params.push(c.r as usize);
            params.push(c.g as usize);
            params.push(c.b as usize);
        }
        StyleColor::Palette(idx) => {
            params.push(38);
            params.push(5);
            params.push(idx as usize);
        }
        StyleColor::Default => {}
    }

    // Background color
    match style.bg {
        StyleColor::Rgb(c) => {
            params.push(48);
            params.push(2);
            params.push(c.r as usize);
            params.push(c.g as usize);
            params.push(c.b as usize);
        }
        StyleColor::Palette(idx) => {
            params.push(48);
            params.push(5);
            params.push(idx as usize);
        }
        StyleColor::Default => {}
    }

    if params.is_empty() {
        String::new()
    } else {
        let param_str: Vec<String> = params.iter().map(|p| p.to_string()).collect();
        format!("\x1b[{}m", param_str.join(";"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::CellStyle;

    #[test]
    fn full_redraw_basic() {
        let mut snap = ScreenSnapshot::new(10, 2);
        snap.rows[0][0].text = "H".into();
        snap.rows[0][1].text = "i".into();

        let vt = DisplayDiff::full_redraw(&snap);
        assert!(vt.contains("\x1b[2J")); // clear screen
        assert!(vt.contains("\x1b[H"));  // move to top-left
        assert!(vt.contains("Hi"));
    }

    #[test]
    fn diff_detects_changes() {
        let mut old = ScreenSnapshot::new(10, 1);
        let mut new = ScreenSnapshot::new(10, 1);
        new.rows[0][0].text = "A".into();

        let vt = DisplayDiff::diff(&old, &new);
        assert!(vt.contains("A"));

        // Now make old == new
        old.rows[0][0].text = "A".into();
        let vt = DisplayDiff::diff(&old, &new);
        assert!(vt.is_empty() || vt.ends_with("\x1b[1;1H")); // just cursor positioning
    }

    #[test]
    fn diff_different_size_does_full_redraw() {
        let old = ScreenSnapshot::new(10, 1);
        let new = ScreenSnapshot::new(20, 2);
        let vt = DisplayDiff::diff(&old, &new);
        assert!(vt.contains("\x1b[2J")); // full redraw
    }

    #[test]
    fn style_generation() {
        let style = CellStyle {
            fg: StyleColor::Palette(1),
            bold: true,
            ..CellStyle::default_style()
        };
        let s = style_on(&style);
        assert!(s.contains("1"));   // bold
        assert!(s.contains("38"));  // fg 256-color
        assert!(s.contains("5"));   // palette mode
    }

    #[test]
    fn diff_resets_style() {
        let mut old = ScreenSnapshot::new(5, 1);
        let mut new = ScreenSnapshot::new(5, 1);
        old.rows[0][0].style.bold = true;
        new.rows[0][0].style.bold = false;
        new.rows[0][0].text = "X".into();

        let vt = DisplayDiff::diff(&old, &new);
        // Should contain a reset sequence
        assert!(vt.contains("\x1b[0m"));
    }

    #[test]
    fn diff_pen_state_divergence_at_unchanged_position() {
        // Escape sequence changes pen state at unchanged position.
        // Old: yellow "$" at pos 0, default at pos 1-2
        // New: yellow "$" at pos 0, default at pos 1, "X" at pos 2
        // The pen state at pos 1 diverges (old=yellow, new=default)
        // because \x1b[0m was applied at pos 1 between snapshots.
        let mut old = ScreenSnapshot::new(10, 1);
        let mut new = ScreenSnapshot::new(10, 1);

        old.rows[0][0].style.fg = StyleColor::Palette(3);
        old.rows[0][0].text = "$".into();
        new.rows[0][0].style.fg = StyleColor::Palette(3);
        new.rows[0][0].text = "$".into();

        old.rows[0][1].text = " ".into();
        new.rows[0][1].text = " ".into();

        new.rows[0][2].text = "X".into();

        let vt = DisplayDiff::diff(&old, &new);

        // Should contain reset for pen state divergence at pos 1
        // and "X" at pos 2
        assert!(vt.contains("\x1b[0m"));
        assert!(vt.contains("X"));
    }

    #[test]
    fn diff_two_changed_runs_with_pen_divergence_between() {
        // Two changed runs in same row. Between them, pen state diverges.
        // Old: yellow "Hi" at 0-1, yellow "XY" at 3-4
        // New: yellow "Hi" at 0-1, default "ZW" at 3-4
        // "Hi" is unchanged so not emitted; "ZW" is the changed run.
        // Pen state diverges at pos 2 (old=yellow, new=default) → reset emitted.
        let mut old = ScreenSnapshot::new(10, 1);
        let mut new = ScreenSnapshot::new(10, 1);

        old.rows[0][0].style.fg = StyleColor::Palette(3);
        old.rows[0][0].text = "H".into();
        old.rows[0][1].style.fg = StyleColor::Palette(3);
        old.rows[0][1].text = "i".into();
        new.rows[0][0].style.fg = StyleColor::Palette(3);
        new.rows[0][0].text = "H".into();
        new.rows[0][1].style.fg = StyleColor::Palette(3);
        new.rows[0][1].text = "i".into();

        old.rows[0][3].style.fg = StyleColor::Palette(3);
        old.rows[0][3].text = "X".into();
        old.rows[0][4].style.fg = StyleColor::Palette(3);
        old.rows[0][4].text = "Y".into();
        new.rows[0][3].style.fg = StyleColor::Default;
        new.rows[0][3].text = "Z".into();
        new.rows[0][4].style.fg = StyleColor::Default;
        new.rows[0][4].text = "W".into();

        let vt = DisplayDiff::diff(&old, &new);

        // "ZW" should be in output with a pen state reset
        assert!(vt.contains("ZW"));
        assert!(vt.contains("\x1b[0m"));
    }

    #[test]
    fn diff_multiple_rows_each_resets_sgr() {
        let mut old = ScreenSnapshot::new(5, 2);
        let mut new = ScreenSnapshot::new(5, 2);

        old.rows[0][0].style.fg = StyleColor::Palette(1);
        old.rows[0][0].text = "A".into();
        new.rows[0][0].style.fg = StyleColor::Palette(1);
        new.rows[0][0].text = "A".into();

        old.rows[1][0].text = "B".into();
        new.rows[1][0].text = "C".into();

        let vt = DisplayDiff::diff(&old, &new);

        // Row 0 is skipped (all cells identical)
        // Row 1 has change, should start with SGR reset
        assert!(vt.contains("\x1b[0m"));
        assert!(vt.contains("C"));
    }

    #[test]
    fn diff_no_unnecessary_resets_for_identical_rows() {
        let old = ScreenSnapshot::new(5, 2);
        let new = ScreenSnapshot::new(5, 2);

        let vt = DisplayDiff::diff(&old, &new);
        // Only cursor positioning, no SGR resets
        assert!(!vt.contains("\x1b[0m"));
    }
}
