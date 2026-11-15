use libghostty_vt::style::RgbColor;

/// A color value: either RGB or a 256-color palette index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StyleColor {
    Default,
    Rgb(RgbColor),
    Palette(u8),
}

impl StyleColor {
    pub fn is_default(self) -> bool {
        matches!(self, StyleColor::Default)
    }
}

/// Style attributes for a terminal cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CellStyle {
    pub fg: StyleColor,
    pub bg: StyleColor,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: bool,
}

impl CellStyle {
    pub fn default_style() -> Self {
        Self {
            fg: StyleColor::Default,
            bg: StyleColor::Default,
            bold: false,
            italic: false,
            faint: false,
            blink: false,
            inverse: false,
            invisible: false,
            strikethrough: false,
            overline: false,
            underline: false,
        }
    }

    pub fn is_default(&self) -> bool {
        *self == Self::default_style()
    }
}

/// A single terminal cell with content and style.
#[derive(Debug, PartialEq, Eq)]
pub struct CellData {
    pub text: String,
    /// This cell holds a double-width glyph and owns the column to its right.
    pub wide: bool,
    /// This cell is the right half of a double-width glyph. Nothing may be
    /// drawn here: writing the wide glyph already covered it.
    pub spacer: bool,
    pub style: CellStyle,
}

impl Clone for CellData {
    fn clone(&self) -> Self {
        Self {
            text: self.text.clone(),
            wide: self.wide,
            spacer: self.spacer,
            style: self.style,
        }
    }

    /// Reuse the text buffer when its capacity is sufficient.
    fn clone_from(&mut self, source: &Self) {
        let Self {
            text,
            wide,
            spacer,
            style,
        } = source;
        self.text.clone_from(text);
        self.wide = *wide;
        self.spacer = *spacer;
        self.style = *style;
    }
}

impl CellData {
    /// Empty cell (space with default style).
    pub fn empty() -> Self {
        Self {
            text: String::new(),
            wide: false,
            spacer: false,
            style: CellStyle::default_style(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The `char` this cell shows, if one narrow `char` describes it fully.
    /// `None` for either half of a wide glyph and for multi-codepoint clusters.
    pub fn single_char(&self) -> Option<char> {
        if self.wide || self.spacer {
            return None;
        }
        let mut chars = self.text.chars();
        match (chars.next(), chars.next()) {
            (None, _) => Some(' '),
            (Some(c), None) => Some(c),
            _ => None,
        }
    }
}

/// Which mouse-reporting mode the host has requested.
///
/// Mirrors mosh's `Terminal::DrawState::mouse_reporting_mode`; the numeric
/// values are the DEC private mode numbers so they can be written straight
/// into an escape sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MouseReporting {
    #[default]
    None = 0,
    X10 = 9,
    VT200 = 1000,
    ButtonEvent = 1002,
    AnyEvent = 1003,
}

/// How mouse coordinates are encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MouseEncoding {
    #[default]
    Default = 0,
    Utf8 = 1005,
    Sgr = 1006,
    Urxvt = 1015,
}

/// A complete snapshot of the terminal screen at a point in time.
///
/// This is morsh's equivalent of mosh's `Terminal::Framebuffer`: everything
/// the protocol synchronizes about a terminal, and everything
/// [`crate::DisplayDiff`] needs to reproduce it. It carries more than the
/// grid — the window title and the input modes are part of the terminal's
/// observable state, and a client that never learns about them can never show
/// a title or run a mouse-driven program.
#[derive(Debug, PartialEq)]
pub struct ScreenSnapshot {
    pub rows: Vec<Vec<CellData>>,
    pub cols: u16,
    pub rows_count: u16,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub cursor_visible: bool,
    /// Window title set by the host (OSC 0/2).
    pub title: String,
    /// DEC 2004: bracketed paste.
    pub bracketed_paste: bool,
    /// DEC 9 / 1000 / 1002 / 1003.
    pub mouse_reporting: MouseReporting,
    /// DEC 1005 / 1006 / 1015.
    pub mouse_encoding: MouseEncoding,
    /// DEC 1004: focus in/out reporting.
    pub mouse_focus_event: bool,
    /// DEC 1007: alternate scroll.
    pub mouse_alternate_scroll: bool,
    /// DEC 2027: grapheme clustering. Unlike the modes above this one is kept
    /// between emulators and never reaches the user's terminal, which may have
    /// its own default that we couldn't restore on exit.
    pub grapheme_cluster: bool,
    pub fg: RgbColor,
    pub bg: RgbColor,
    pub palette: [RgbColor; 256],
}

impl Clone for ScreenSnapshot {
    fn clone(&self) -> Self {
        Self {
            rows: self.rows.clone(),
            cols: self.cols,
            rows_count: self.rows_count,
            cursor_x: self.cursor_x,
            cursor_y: self.cursor_y,
            cursor_visible: self.cursor_visible,
            title: self.title.clone(),
            bracketed_paste: self.bracketed_paste,
            mouse_reporting: self.mouse_reporting,
            mouse_encoding: self.mouse_encoding,
            mouse_focus_event: self.mouse_focus_event,
            mouse_alternate_scroll: self.mouse_alternate_scroll,
            grapheme_cluster: self.grapheme_cluster,
            fg: self.fg,
            bg: self.bg,
            palette: self.palette,
        }
    }

    /// Reuse row and cell buffers when their capacity is sufficient.
    fn clone_from(&mut self, source: &Self) {
        let Self {
            rows,
            cols,
            rows_count,
            cursor_x,
            cursor_y,
            cursor_visible,
            title,
            bracketed_paste,
            mouse_reporting,
            mouse_encoding,
            mouse_focus_event,
            mouse_alternate_scroll,
            grapheme_cluster,
            fg,
            bg,
            palette,
        } = source;
        self.rows.clone_from(rows);
        self.cols = *cols;
        self.rows_count = *rows_count;
        self.cursor_x = *cursor_x;
        self.cursor_y = *cursor_y;
        self.cursor_visible = *cursor_visible;
        self.title.clone_from(title);
        self.bracketed_paste = *bracketed_paste;
        self.mouse_reporting = *mouse_reporting;
        self.mouse_encoding = *mouse_encoding;
        self.mouse_focus_event = *mouse_focus_event;
        self.mouse_alternate_scroll = *mouse_alternate_scroll;
        self.grapheme_cluster = *grapheme_cluster;
        self.fg = *fg;
        self.bg = *bg;
        self.palette = *palette;
    }
}

impl ScreenSnapshot {
    /// Create an empty snapshot with the given dimensions.
    pub fn new(cols: u16, rows: u16) -> Self {
        let empty_row = (0..cols).map(|_| CellData::empty()).collect();
        Self {
            rows: vec![empty_row; rows as usize],
            cols,
            rows_count: rows,
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: true,
            title: String::new(),
            bracketed_paste: false,
            mouse_reporting: MouseReporting::None,
            mouse_encoding: MouseEncoding::Default,
            mouse_focus_event: false,
            mouse_alternate_scroll: false,
            grapheme_cluster: false,
            fg: RgbColor { r: 255, g: 255, b: 255 },
            bg: RgbColor { r: 0, g: 0, b: 0 },
            palette: default_palette(),
        }
    }

    /// Get a cell at (x, y). Returns None if out of bounds.
    pub fn cell(&self, x: u16, y: u16) -> Option<&CellData> {
        self.rows
            .get(y as usize)?
            .get(x as usize)
    }

    /// Get a mutable cell at (x, y). Returns None if out of bounds.
    pub fn cell_mut(&mut self, x: u16, y: u16) -> Option<&mut CellData> {
        self.rows
            .get_mut(y as usize)?
            .get_mut(x as usize)
    }
}

fn default_palette() -> [RgbColor; 256] {
    let mut palette = [RgbColor { r: 0, g: 0, b: 0 }; 256];
    // Standard 16 colors (xterm default)
    let standard: [(u8, u8, u8); 16] = [
        (0, 0, 0),       // Black
        (205, 49, 49),   // Red
        (13, 188, 121),  // Green
        (229, 229, 16),  // Yellow
        (36, 114, 200),  // Blue
        (188, 63, 188),  // Magenta
        (17, 168, 205),  // Cyan
        (229, 229, 229), // White
        (102, 102, 102), // Bright Black
        (241, 76, 76),   // Bright Red
        (35, 209, 139),  // Bright Green
        (245, 245, 67),  // Bright Yellow
        (59, 142, 234),  // Bright Blue
        (214, 112, 214), // Bright Magenta
        (41, 184, 219),  // Bright Cyan
        (255, 255, 255), // Bright White
    ];
    for (i, &(r, g, b)) in standard.iter().enumerate() {
        palette[i] = RgbColor { r, g, b };
    }
    // 216-color cube (indices 16-231)
    for i in 0u16..216 {
        let idx = (i + 16) as usize;
        let r = ((i / 36) * 51) as u8;
        let g = (((i / 6) % 6) * 51) as u8;
        let b = ((i % 6) * 51) as u8;
        palette[idx] = RgbColor { r, g, b };
    }
    // Grayscale ramp (indices 232-255)
    for i in 0u16..24 {
        let v = (8 + i * 10) as u8;
        palette[(232 + i) as usize] = RgbColor { r: v, g: v, b: v };
    }
    palette
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_from_copies_all_state_and_keeps_storage_independent() {
        let mut source = ScreenSnapshot::new(12, 3);
        source.cursor_x = 2;
        source.cursor_y = 1;
        source.cursor_visible = false;
        source.title = "a new title".into();
        source.bracketed_paste = true;
        source.mouse_reporting = MouseReporting::AnyEvent;
        source.mouse_encoding = MouseEncoding::Sgr;
        source.mouse_focus_event = true;
        source.mouse_alternate_scroll = true;
        source.grapheme_cluster = true;
        source.fg = RgbColor { r: 1, g: 2, b: 3 };
        source.bg = RgbColor { r: 4, g: 5, b: 6 };
        source.palette[42] = source.fg;
        source.rows[0][0].text = "👍🏽".into();
        source.rows[0][0].wide = true;
        source.rows[0][0].style.bold = true;
        source.rows[0][1].spacer = true;
        source.rows[1][2].text = "e\u{301}".into();

        let mut reused = ScreenSnapshot::new(4, 1);
        reused.clone_from(&source);
        assert_eq!(reused, source);
        assert_eq!(source.clone(), source);

        // Shrink, grow a cluster, clear a title, and replace all mode state.
        let mut changed = ScreenSnapshot::new(3, 1);
        changed.rows[0][0].text = "👨\u{200d}👩\u{200d}👧".into();
        source.clone_from(&changed);
        assert_eq!(source, changed);
        assert_eq!(reused.rows[0][0].text, "👍🏽");
        assert_eq!(reused.title, "a new title");
        reused.clone_from(&source);
        assert_eq!(reused, changed);
        reused.rows[0][0].text.clear();
        assert!(!source.rows[0][0].text.is_empty());
    }
}
