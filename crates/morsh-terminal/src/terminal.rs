use libghostty_vt::render::RenderState;
use libghostty_vt::style::RgbColor as VtRgbColor;
use libghostty_vt::screen::CellWide;
use libghostty_vt::terminal::Mode;
use libghostty_vt::{Terminal, TerminalOptions};

use crate::snapshot::{
    CellData, CellStyle, MouseEncoding, MouseReporting, ScreenSnapshot, StyleColor,
};

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

    /// Extract the current terminal state as a ScreenSnapshot.
    ///
    /// Creates a one-shot RenderState and reads all cell contents, cursor
    /// position, and colors into the snapshot format used by DisplayDiff.
    pub fn snapshot(&self) -> Result<ScreenSnapshot, String> {
        let mut render_state =
            RenderState::new().map_err(|e| format!("RenderState::new failed: {e:?}"))?;
        let snapshot = render_state
            .update(&self.terminal)
            .map_err(|e| format!("RenderState::update failed: {e:?}"))?;

        let cols = self.cols;
        let rows = self.rows;

        // Cursor position
        let (cursor_x, cursor_y) = match snapshot.cursor_viewport().ok().flatten() {
            Some(cv) => (cv.x.min(cols.saturating_sub(1)), cv.y.min(rows.saturating_sub(1))),
            None => (0, 0),
        };
        let cursor_visible = snapshot.cursor_visible().unwrap_or(true);

        // Colors
        let (fg, bg, palette) = match snapshot.colors() {
            Ok(colors) => {
                let fg = colors.foreground;
                let bg = colors.background;
                let mut palette = [VtRgbColor { r: 0, g: 0, b: 0 }; 256];
                for (i, entry) in colors.palette.iter().enumerate().take(256) {
                    palette[i] = *entry;
                }
                (fg, bg, palette)
            }
            Err(_) => (
                VtRgbColor { r: 255, g: 255, b: 255 },
                VtRgbColor { r: 0, g: 0, b: 0 },
                [VtRgbColor { r: 0, g: 0, b: 0 }; 256],
            ),
        };

        // Read all cells
        let mut screen_rows: Vec<Vec<CellData>> = Vec::with_capacity(rows as usize);
        let mut row_iter =
            libghostty_vt::render::RowIterator::new().map_err(|e| format!("RowIterator::new: {e:?}"))?;
        let mut cell_iter = libghostty_vt::render::CellIterator::new()
            .map_err(|e| format!("CellIterator::new: {e:?}"))?;

        let row_iteration = row_iter
            .update(&snapshot)
            .map_err(|e| format!("RowIterator::update: {e:?}"))?;

        // Iterate rows — RowIteration doesn't implement Iterator, use next() manually
        // We need to collect into a Vec first since RowIteration borrows row_iter
        let mut row_snapshots: Vec<(bool, Vec<CellData>)> = Vec::new();
        {
            let mut ri = row_iteration;
            while let Some(row) = ri.next() {
                let dirty = row.dirty().unwrap_or(false);
                let mut cells = Vec::with_capacity(cols as usize);
                let cell_iteration = cell_iter
                    .update(row)
                    .map_err(|e| format!("CellIterator::update: {e:?}"))?;
                {
                    let mut ci = cell_iteration;
                    while let Some(cell) = ci.next() {
                        let graphemes = cell
                            .graphemes()
                            .map_err(|_| "Cell::graphemes failed".to_string())?;
                        let text: String = graphemes.iter().collect();

                        let style = match cell.style() {
                            Ok(s) => convert_style(&s),
                            Err(_) => CellStyle::default_style(),
                        };

                        // Read the real width from the cell. Treating every
                        // cell as narrow makes a CJK glyph emit the character
                        // *and* a space over the column it already covers.
                        let width = cell
                            .raw_cell()
                            .and_then(|raw| raw.wide())
                            .unwrap_or(CellWide::Narrow);
                        let wide = width == CellWide::Wide;
                        let spacer =
                            width == CellWide::SpacerTail || width == CellWide::SpacerHead;

                        // Canonicalise blank cells. ghostty reports a blank
                        // as either an empty grapheme list or a single space
                        // depending on how it came to be blank; if both forms
                        // reach the diff, a screen that was reconstructed from
                        // a redraw compares unequal to the identical screen
                        // that was built by the emulator, and every cell looks
                        // changed.
                        let text = if spacer || text == " " {
                            String::new()
                        } else {
                            text
                        };

                        cells.push(CellData {
                            text,
                            wide,
                            spacer,
                            style,
                        });
                    }
                }
                // Pad short rows
                while cells.len() < cols as usize {
                    cells.push(CellData::empty());
                }
                row_snapshots.push((dirty, cells));
            }

        }

        // Take only `rows` worth of rows
        for (_, cells) in row_snapshots.into_iter().take(rows as usize) {
            screen_rows.push(cells);
        }

        // Pad if fewer rows than expected
        let empty_row: Vec<CellData> = (0..cols).map(|_| CellData::empty()).collect();
        while screen_rows.len() < rows as usize {
            screen_rows.push(empty_row.clone());
        }

        Ok(ScreenSnapshot {
            rows: screen_rows,
            cols,
            rows_count: rows,
            cursor_x,
            cursor_y,
            cursor_visible,
            title: self.terminal.title().unwrap_or("").to_string(),
            bracketed_paste: self.mode(Mode::BRACKETED_PASTE),
            mouse_reporting: self.mouse_reporting(),
            mouse_encoding: self.mouse_encoding(),
            mouse_focus_event: self.mode(Mode::FOCUS_EVENT),
            mouse_alternate_scroll: self.mode(Mode::ALT_SCROLL),
            fg,
            bg,
            palette,
        })
    }

    fn mode(&self, mode: Mode) -> bool {
        self.terminal.mode(mode).unwrap_or(false)
    }

    /// The most specific mouse-reporting mode the host has turned on.
    fn mouse_reporting(&self) -> MouseReporting {
        // Later modes supersede earlier ones, matching xterm.
        for (mode, value) in [
            (Mode::ANY_MOUSE, MouseReporting::AnyEvent),
            (Mode::BUTTON_MOUSE, MouseReporting::ButtonEvent),
            (Mode::NORMAL_MOUSE, MouseReporting::VT200),
            (Mode::X10_MOUSE, MouseReporting::X10),
        ] {
            if self.mode(mode) {
                return value;
            }
        }
        MouseReporting::None
    }

    fn mouse_encoding(&self) -> MouseEncoding {
        for (mode, value) in [
            (Mode::SGR_MOUSE, MouseEncoding::Sgr),
            (Mode::URXVT_MOUSE, MouseEncoding::Urxvt),
            (Mode::UTF8_MOUSE, MouseEncoding::Utf8),
        ] {
            if self.mode(mode) {
                return value;
            }
        }
        MouseEncoding::Default
    }
}

/// Convert a ghostty-vt Style to our CellStyle.
fn convert_style(s: &libghostty_vt::style::Style) -> CellStyle {
    CellStyle {
        fg: convert_color(&s.fg_color),
        bg: convert_color(&s.bg_color),
        bold: s.bold,
        italic: s.italic,
        faint: s.faint,
        blink: s.blink,
        inverse: s.inverse,
        invisible: s.invisible,
        strikethrough: s.strikethrough,
        overline: s.overline,
        underline: s.underline != libghostty_vt::style::Underline::None,
    }
}

/// Convert a ghostty-vt StyleColor to our StyleColor.
fn convert_color(c: &libghostty_vt::style::StyleColor) -> StyleColor {
    match c {
        libghostty_vt::style::StyleColor::None => StyleColor::Default,
        libghostty_vt::style::StyleColor::Palette(idx) => StyleColor::Palette(idx.0),
        libghostty_vt::style::StyleColor::Rgb(rgb) => StyleColor::Rgb(*rgb),
    }
}
