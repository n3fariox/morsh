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
    pub fn diff(old: &ScreenSnapshot, new: &ScreenSnapshot) -> String {
        if old.cols != new.cols || old.rows_count != new.rows_count {
            return Self::full_redraw(new);
        }

        let mut out = String::with_capacity(new.cols as usize * new.rows_count as usize * 2);
        let mut last_style = CellStyle::default_style();

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

            // Find changed run within this row
            let mut x = 0usize;
            while x < new.cols as usize {
                if old_row.get(x) == new_row.get(x) {
                    x += 1;
                    continue;
                }

                // Found a changed cell. Position cursor.
                out.push_str(&format!("\x1b[{};{}H", y + 1, x + 1));

                // Emit cells until we hit an unchanged cell or end of row
                let mut need_style_reset = false;
                while x < new.cols as usize && old_row.get(x) != new_row.get(x) {
                    let cell = &new_row[x];
                    let old_cell = &old_row[x];

                    // If old cell was styled and we haven't reset yet, reset first
                    if need_style_reset || (!old_cell.style.is_default() && cell.style.is_default()) {
                        out.push_str("\x1b[0m");
                        last_style = CellStyle::default_style();
                        need_style_reset = false;
                    }

                    // Apply new style if different from what's active
                    if cell.style != last_style {
                        if !cell.style.is_default() {
                            out.push_str(&style_on(&cell.style));
                        }
                        last_style = cell.style.clone();
                    }

                    if cell.text.is_empty() {
                        out.push(' ');
                    } else {
                        out.push_str(&cell.text);
                    }

                    // If cell is wide, skip the next (placeholder) cell
                    if cell.wide && x + 1 < new.cols as usize {
                        x += 1;
                    }
                    x += 1;
                }
            }
        }

        // Reset style at end if non-default
        if !last_style.is_default() {
            out.push_str("\x1b[0m");
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
}
