//! Synthetic tmux-style VT output: independently addressed side-by-side panes.
//! No dependency on a running tmux server or the developer's terminal settings.
use std::fmt::Write;

#[derive(Clone, Copy, Debug)]
pub enum Side {
    Left,
    Right,
}

pub fn pane_bounds(cols: u16, side: Side) -> (u16, u16) {
    let split = cols / 2;
    match side {
        Side::Left => (0, split - 1),
        Side::Right => (split, cols - split),
    }
}

fn row_text(width: u16, row: u16, frame: u16, unicode: bool) -> String {
    let prefix = format!("row {row:03} frame {frame:03} ");
    let tail = if unicode { "e\u{301} 界" } else { " END" }; // four display columns
    let padding = width as usize - prefix.len() - 4;
    format!(
        "{prefix}{}{tail}",
        char::from(b'a' + (frame % 26) as u8)
            .to_string()
            .repeat(padding)
    )
}

pub fn initial_screen(cols: u16, rows: u16) -> Vec<u8> {
    let mut vt = String::from("\x1b[0m\x1b[2J\x1b[H");
    for row in 0..rows - 1 {
        let (left, width) = pane_bounds(cols, Side::Left);
        let _ = write!(
            vt,
            "\x1b[{};{}H\x1b[32m{}",
            row + 1,
            left + 1,
            row_text(width, row, 0, true)
        );
        let _ = write!(vt, "\x1b[0m\x1b[{};{}H│", row + 1, cols / 2);
        let (right, width) = pane_bounds(cols, Side::Right);
        let _ = write!(
            vt,
            "\x1b[{};{}H\x1b[34m{}",
            row + 1,
            right + 1,
            row_text(width, row, 20, true)
        );
    }
    let status = format!("[tmux] {cols}x{rows} ");
    let _ = write!(
        vt,
        "\x1b[{};1H\x1b[0;7m{status}{}\x1b[0m\x1b[1;1H",
        rows,
        " ".repeat(cols as usize - status.len())
    );
    vt.into_bytes()
}

/// A pane redraw represented by CUP + row text, as a
/// multiplexer must do when the adjacent pane cannot scroll with it.
/// Varying frame shifts row numbers and alternates ASCII/wide-glyph endings.
pub fn update_pane(cols: u16, rows: u16, side: Side, frame: u16) -> Vec<u8> {
    let (start, width) = pane_bounds(cols, side);
    let mut vt = String::new();
    for row in 0..rows - 1 {
        let _ = write!(
            vt,
            "\x1b[{};{}H\x1b[33m{}",
            row + 1,
            start + 1,
            row_text(width, row + frame, frame, frame % 2 == 1)
        );
    }
    let _ = write!(vt, "\x1b[0m\x1b[2;{}H", start + 2);
    vt.into_bytes()
}

/// Scroll one pane upward by one row while the adjacent pane stays fixed.
pub fn scroll_pane(cols: u16, rows: u16, side: Side) -> Vec<u8> {
    let (start, width) = pane_bounds(cols, side);
    let (color, frame) = match side {
        Side::Left => (32, 0),
        Side::Right => (34, 20),
    };
    let mut vt = String::new();
    for row in 0..rows - 1 {
        let _ = write!(
            vt,
            "\x1b[{};{}H\x1b[{}m{}",
            row + 1,
            start + 1,
            color,
            row_text(width, row + 1, frame, true)
        );
    }
    vt.push_str("\x1b[0m\x1b[1;1H");
    vt.into_bytes()
}
