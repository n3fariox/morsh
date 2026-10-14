use crate::snapshot::{CellData, CellStyle, MouseEncoding, MouseReporting, ScreenSnapshot, StyleColor};
use std::fmt::Write as _;

/// Generates VT escape sequence strings from screen snapshots.
///
/// The rough equivalent of mosh's `Terminal::Display`
/// (`src/terminal/terminaldisplay.cc`).
pub struct DisplayDiff;

/// Emit a DEC private mode set/reset.
fn mode(out: &mut String, number: u16, on: bool) {
    let _ = write!(out, "\x1b[?{}{}", number, if on { 'h' } else { 'l' });
}

/// Emit the window title. BEL-terminated, which is more widely supported than
/// ST — the same choice mosh makes.
fn title(out: &mut String, text: &str) {
    let _ = write!(out, "\x1b]0;{text}\x07");
}

/// Emit the mode changes needed to go from `old` to `new`.
fn state_changes(out: &mut String, old: Option<&ScreenSnapshot>, new: &ScreenSnapshot) {
    let changed = |f: fn(&ScreenSnapshot) -> bool| old.map(f) != Some(f(new));

    match old {
        Some(o) if o.title == new.title => {}
        // Don't announce an empty title on a first paint; the host has not set
        // one and clobbering the user's terminal title would be rude.
        None if new.title.is_empty() => {}
        _ => title(out, &new.title),
    }

    if changed(|s| s.bracketed_paste) {
        mode(out, 2004, new.bracketed_paste);
    }
    if changed(|s| s.mouse_focus_event) {
        mode(out, 1004, new.mouse_focus_event);
    }
    if changed(|s| s.mouse_alternate_scroll) {
        mode(out, 1007, new.mouse_alternate_scroll);
    }

    if old.map(|o| o.mouse_reporting) != Some(new.mouse_reporting) {
        for m in [
            MouseReporting::X10,
            MouseReporting::VT200,
            MouseReporting::ButtonEvent,
            MouseReporting::AnyEvent,
        ] {
            if m != new.mouse_reporting {
                mode(out, m as u16, false);
            }
        }
        if new.mouse_reporting != MouseReporting::None {
            mode(out, new.mouse_reporting as u16, true);
        }
    }

    if old.map(|o| o.mouse_encoding) != Some(new.mouse_encoding) {
        for e in [
            MouseEncoding::Utf8,
            MouseEncoding::Sgr,
            MouseEncoding::Urxvt,
        ] {
            if e != new.mouse_encoding {
                mode(out, e as u16, false);
            }
        }
        if new.mouse_encoding != MouseEncoding::Default {
            mode(out, new.mouse_encoding as u16, true);
        }
    }
}

/// How far the screen scrolled between two frames, if it did.
struct Scroll {
    /// Number of lines the content moved up by.
    lines: usize,
    /// Number of rows at the top of the new frame the scroll accounts for.
    height: usize,
}

/// Detect that the display moved up by some number of lines.
///
/// Ported from the "shortcut -- has display moved up by a certain number of
/// lines?" block in mosh's `Display::new_frame`. Without it, a single scrolled
/// line rewrites every row on the screen.
fn detect_scroll(old_rows: &[Vec<CellData>], new: &ScreenSnapshot) -> Option<Scroll> {
    let height = new.rows_count as usize;
    if height == 0 || old_rows.len() < height {
        return None;
    }

    for row in 0..height {
        if new.rows[0] != old_rows[row] {
            continue;
        }
        // Row 0 matching row 0 means we're looking at ourselves: no scroll.
        if row == 0 {
            return None;
        }

        let lines = row;
        let mut scroll_height = 1;
        let mut region_height = 1;
        while lines + region_height < height {
            if new.rows[region_height] == old_rows[lines + region_height] {
                scroll_height = region_height + 1;
                region_height += 1;
            } else {
                break;
            }
        }
        return Some(Scroll {
            lines,
            height: scroll_height,
        });
    }
    None
}

impl DisplayDiff {
    /// Generate VT escape sequences to render a full snapshot from scratch.
    pub fn full_redraw(snap: &ScreenSnapshot) -> String {
        let mut out = String::with_capacity(snap.cols as usize * snap.rows_count as usize * 4);
        // Hide cursor during redraw
        out.push_str("\x1b[?25l");
        // Reset the scrolling region, style, and screen
        out.push_str("\x1b[r\x1b[0m\x1b[H\x1b[2J");

        // Every mode is written explicitly rather than only on change, so a
        // full redraw is enough to reconstruct the terminal's whole state.
        state_changes(&mut out, None, snap);

        let mut last_style = CellStyle::default_style();

        for (y, row) in snap.rows.iter().enumerate() {
            // Absolute positioning per row rather than "\r\n": terminals differ
            // on wrap/newline behavior at the right margin (notably the
            // Windows console), and CUP is unambiguous.
            let _ = write!(out, "\x1b[{};1H", y + 1);
            let mut x = 0usize;
            while x < row.len() {
                let cell = &row[x];
                if cell.style != last_style {
                    // Reset first so attributes of the previous style don't leak
                    out.push_str("\x1b[0m");
                    if !cell.style.is_default() {
                        out.push_str(&style_on(&cell.style));
                    }
                    last_style = cell.style;
                }
                if cell.text.is_empty() {
                    out.push(' ');
                } else {
                    out.push_str(&cell.text);
                }
                // A wide glyph already covered the column to its right.
                x += if cell.wide { 2 } else { 1 };
            }
            // Reset style at end of row
            if !last_style.is_default() {
                out.push_str("\x1b[0m");
                last_style = CellStyle::default_style();
            }
        }

        // Position cursor
        let _ = write!(out, "\x1b[{};{}H", snap.cursor_y + 1, snap.cursor_x + 1);
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

        state_changes(&mut out, Some(old), new);

        // A scrolled screen is reproduced by scrolling, not by rewriting every
        // row. `old_rows` is then updated to reflect the shift so the row-by-row
        // pass below diffs against what is actually on the display.
        let mut old_rows = old.rows.clone();
        let mut first_row = 0usize;
        if let Some(scroll) = detect_scroll(&old_rows, new) {
            first_row = scroll.height;
            Self::emit_scroll(&mut out, &scroll, new, old.cursor_y);
            Self::shift_rows(&mut old_rows, &scroll, new);
        }

        let rows = new.rows_count as usize;
        for (y, (old_row, new_row)) in old_rows[..rows]
            .iter()
            .zip(new.rows[..rows].iter())
            .enumerate()
            .skip(first_row)
        {

            // Quick check: skip unchanged rows
            if old_row == new_row {
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
                    let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
                    // Reset to default, then apply new style if non-default
                    out.push_str("\x1b[0m");
                    if !new_cell.style.is_default() {
                        out.push_str(&style_on(&new_cell.style));
                    }
                }

                // Emit changed cell content
                if is_changed {
                    // Position cursor if not already at the right position
                    let needed = (y + 1, x + 1);
                    if cursor_pos != Some(needed) {
                        let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
                    }
                    // After writing this cell the cursor advances by its width.
                    let width = if new_cell.wide { 2 } else { 1 };
                    cursor_pos = Some((y + 1, x + 1 + width));

                    if new_cell.text.is_empty() {
                        out.push(' ');
                    } else {
                        out.push_str(&new_cell.text);
                    }
                } else {
                    // Unchanged cell — cursor is no longer tracked
                    cursor_pos = None;
                }

                pen_state = new_cell.style;

                // A wide glyph owns the column to its right; writing anything
                // there would overwrite half of it.
                if new_cell.wide && x + 1 < new.cols as usize {
                    x += 1;
                    pen_state = new_row[x].style;
                }
                x += 1;
            }
        }

        // Reposition the cursor only when it actually needs to move: either we
        // drew something (which left the cursor wherever the last cell was) or
        // the cursor itself moved. Emitting it unconditionally means every
        // idle frame writes a few bytes to the user's terminal forever.
        if !out.is_empty()
            || old.cursor_y != new.cursor_y
            || old.cursor_x != new.cursor_x
        {
            let _ = write!(out, "\x1b[{};{}H", new.cursor_y + 1, new.cursor_x + 1);
        }

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

    fn emit_scroll(out: &mut String, scroll: &Scroll, new: &ScreenSnapshot, old_cursor_y: u16) {
        let height = new.rows_count as usize;
        let bottom = scroll.lines + scroll.height - 1;

        out.push_str("\x1b[0m");

        if bottom + 1 == height && old_cursor_y as usize + 1 == height {
            // Common case: already on the bottom line, scrolling the whole
            // screen. A carriage return and some newlines will do it.
            out.push('\r');
            for _ in 0..scroll.lines {
                out.push('\n');
            }
        } else {
            let _ = write!(out, "\x1b[1;{}r", bottom + 1);
            let _ = write!(out, "\x1b[{};1H", bottom + 1);
            for _ in 0..scroll.lines {
                out.push('\n');
            }
            out.push_str("\x1b[r");
        }
    }

    fn shift_rows(old_rows: &mut [Vec<CellData>], scroll: &Scroll, new: &ScreenSnapshot) {
        let bottom = scroll.lines + scroll.height - 1;
        let blank: Vec<CellData> = (0..new.cols).map(|_| CellData::empty()).collect();
        for i in 0..=bottom {
            old_rows[i] = if i + scroll.lines <= bottom {
                old_rows[i + scroll.lines].clone()
            } else {
                blank.clone()
            };
        }
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

    fn text_row(cols: u16, text: &str) -> Vec<CellData> {
        let mut row: Vec<CellData> = (0..cols).map(|_| CellData::empty()).collect();
        for (i, ch) in text.chars().enumerate().take(cols as usize) {
            row[i].text = ch.to_string();
        }
        row
    }

    /// A scrolled screen must be reproduced by scrolling, not by repainting
    /// every row.
    #[test]
    fn scroll_by_one_line_emits_a_scroll_not_a_repaint() {
        let cols = 20u16;
        let height = 5u16;
        let mut old = ScreenSnapshot::new(cols, height);
        for (y, line) in ["one", "two", "three", "four", "five"].iter().enumerate() {
            old.rows[y] = text_row(cols, line);
        }
        old.cursor_y = height - 1;

        let mut new = ScreenSnapshot::new(cols, height);
        for (y, line) in ["two", "three", "four", "five", "six"].iter().enumerate() {
            new.rows[y] = text_row(cols, line);
        }
        new.cursor_y = height - 1;

        let vt = DisplayDiff::diff(&old, &new);

        // Cursor was on the bottom line and the whole screen scrolled: a CR
        // and one LF is the whole scroll.
        assert!(vt.contains("\r\n"), "expected a CR/LF scroll, got {vt:?}");
        assert!(!vt.contains("\x1b[2J"), "must not repaint: {vt:?}");
        // Only the newly exposed line is drawn.
        assert!(vt.contains("six"));
        assert!(!vt.contains("three"), "unchanged rows were repainted: {vt:?}");
    }

    #[test]
    fn partial_scroll_uses_a_scrolling_region() {
        let cols = 20u16;
        let height = 6u16;
        let mut old = ScreenSnapshot::new(cols, height);
        let mut new = ScreenSnapshot::new(cols, height);
        for (y, line) in ["a", "b", "c", "d", "e", "f"].iter().enumerate() {
            old.rows[y] = text_row(cols, line);
        }
        // Rows 0..3 scrolled up by one; the bottom rows are unrelated.
        for (y, line) in ["b", "c", "d", "X", "Y", "Z"].iter().enumerate() {
            new.rows[y] = text_row(cols, line);
        }
        old.cursor_y = 0;

        let vt = DisplayDiff::diff(&old, &new);
        assert!(vt.contains("\x1b[1;4r"), "expected a scrolling region: {vt:?}");
        assert!(vt.contains("\x1b[r"), "region must be reset: {vt:?}");
        assert!(!vt.contains("\x1b[2J"));
    }

    #[test]
    fn identical_frames_are_not_mistaken_for_a_scroll() {
        let cols = 10u16;
        let mut old = ScreenSnapshot::new(cols, 3);
        for y in 0..3 {
            old.rows[y] = text_row(cols, "same");
        }
        let new = old.clone();
        let vt = DisplayDiff::diff(&old, &new);
        assert!(!vt.contains('\n'), "no scroll should be emitted: {vt:?}");
    }

    /// A wide glyph covers two columns; emitting a filler space over the
    /// second one corrupts the line.
    #[test]
    fn wide_glyph_does_not_get_a_filler_space() {
        let mut old = ScreenSnapshot::new(6, 1);
        let mut new = ScreenSnapshot::new(6, 1);
        new.rows[0][0].text = "\u{4f60}".to_string();
        new.rows[0][0].wide = true;
        new.rows[0][1].spacer = true;
        new.rows[0][2].text = "!".to_string();
        old.rows[0][2].text = "?".to_string();

        let vt = DisplayDiff::diff(&old, &new);
        assert!(vt.contains('\u{4f60}'));
        assert!(
            !vt.contains("\u{4f60} "),
            "a space was written over the wide glyph's second column: {vt:?}"
        );

        let full = DisplayDiff::full_redraw(&new);
        assert!(full.contains("\u{4f60}!"), "{full:?}");
    }

    #[test]
    fn title_and_modes_are_emitted_when_they_change() {
        let old = ScreenSnapshot::new(10, 2);
        let mut new = old.clone();
        new.title = "build".to_string();
        new.bracketed_paste = true;
        new.mouse_reporting = MouseReporting::ButtonEvent;
        new.mouse_encoding = MouseEncoding::Sgr;

        let vt = DisplayDiff::diff(&old, &new);
        assert!(vt.contains("\x1b]0;build\x07"), "{vt:?}");
        assert!(vt.contains("\x1b[?2004h"));
        assert!(vt.contains("\x1b[?1002h"));
        assert!(vt.contains("\x1b[?1006h"));

        // Unchanged state is not re-announced.
        assert!(!DisplayDiff::diff(&new, &new).contains("\x1b]0;"));
    }

    #[test]
    fn full_redraw_states_every_mode_explicitly() {
        let mut snap = ScreenSnapshot::new(10, 2);
        snap.title = "t".to_string();
        let vt = DisplayDiff::full_redraw(&snap);
        assert!(vt.contains("\x1b]0;t\x07"));
        assert!(vt.contains("\x1b[?2004l"));
        assert!(vt.contains("\x1b[?1000l"));
        assert!(vt.contains("\x1b[r"), "scrolling region must be reset");
    }

    fn text_rows(snap: &ScreenSnapshot) -> Vec<String> {
        snap.rows.iter().map(|r| r.iter().map(|c| if c.text.is_empty() { " " } else { c.text.as_str() }).collect()).collect()
    }

    /// Replays a stock mosh-server session (from a real log) into a model
    /// terminal, renders it frame by frame into a second terminal, and checks
    /// the second terminal ends up identical — including after an absolute
    /// cursor move (Ctrl-A -> CSI 3;3 H), which exposed vertical drift when the
    /// raw bytes were written straight to the Windows console.
    #[test]
    fn rendering_reproduces_model_across_frames() {
        use crate::MoshTerminal;
        let (cols, rows) = (40u16, 10u16);
        let mut first = String::from("\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[?25l");
        for _ in 0..rows { first.push_str("\x1b[K\n"); }
        first.push_str("\x1b[K\x1b[1;1H\x1b[?25h\x1b[0m");
        let frames: Vec<&str> = vec![
            &first,
            "\n",
            "\x1b]0;captain@destroyer:~\x07\x1b[0;1;32mcaptain@destroyer\x1b[0m: \x1b[0;33m~\x1b[0m \x1b[?25l\r\n$ \x1b[?25h",
            "a", ";", "sk", "d", "j", " f", "\u{4e2d}x",
            "\x1b[3;3H",
            "\x1b[K",
            "fs",
        ];

        let mut model = MoshTerminal::new(cols, rows, 0).unwrap();
        let mut screen = MoshTerminal::new(cols, rows, 0).unwrap();
        let mut displayed: Option<ScreenSnapshot> = None;
        for f in frames {
            model.write(f.as_bytes());
            let snap = model.snapshot().unwrap();
            let out = match &displayed {
                Some(old) => DisplayDiff::diff(old, &snap),
                None => DisplayDiff::full_redraw(&snap),
            };
            screen.write(out.as_bytes());
            let shown = screen.snapshot().unwrap();
            assert_eq!(text_rows(&shown), text_rows(&snap), "after frame {f:?}");
            assert_eq!((shown.cursor_x, shown.cursor_y), (snap.cursor_x, snap.cursor_y), "cursor after frame {f:?}");
            displayed = Some(snap);
        }
        let last = displayed.unwrap();
        assert_eq!((last.cursor_x, last.cursor_y), (4, 2));
    }
}
