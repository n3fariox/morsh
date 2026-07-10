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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellData {
    pub text: String,
    pub wide: bool,
    pub style: CellStyle,
}

impl CellData {
    /// Empty cell (space with default style).
    pub fn empty() -> Self {
        Self {
            text: String::new(),
            wide: false,
            style: CellStyle::default_style(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// A complete snapshot of the terminal screen at a point in time.
#[derive(Clone, Debug)]
pub struct ScreenSnapshot {
    pub rows: Vec<Vec<CellData>>,
    pub cols: u16,
    pub rows_count: u16,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub cursor_visible: bool,
    pub fg: RgbColor,
    pub bg: RgbColor,
    pub palette: [RgbColor; 256],
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
