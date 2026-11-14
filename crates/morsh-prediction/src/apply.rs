//! Folding predictions into the frame about to be painted.

use crate::PredictionEngine;
use morsh_terminal::ScreenSnapshot;

impl PredictionEngine {
    /// Overlay the speculative cells and cursor onto `frame`.
    ///
    /// Mosh does the same thing (`PredictionEngine::apply`) rather than
    /// drawing predictions over the top of the finished frame: a prediction
    /// that is folded into the frame participates in the next diff, so it is
    /// cleaned up automatically when the server's real output supersedes it.
    pub fn apply(&self, frame: &mut ScreenSnapshot) {
        if !self.should_display() {
            return;
        }

        let flagging = self.is_flagging();
        let late_acked = self.local_frame_late_acked();
        // Tentative cells (typed after Enter, Up, etc.) aren't shown until the
        // server confirms a prediction from that epoch.
        let confirmed_epoch = self.confirmed_epoch();
        let (cols, rows) = (frame.cols as usize, frame.rows_count as usize);

        for row in self.overlay().rows() {
            if row.row_num >= rows {
                continue;
            }
            for cell in &row.cells {
                // Like stock mosh, leave the last column to the server: wrap
                // behavior there varies by application.
                if !cell.active || cell.is_tentative(confirmed_epoch) || cell.col + 1 >= cols {
                    continue;
                }
                let pending = late_acked < cell.expiration_frame;
                if !cell.unknown {
                    split_wide_glyph(frame, cell.col as u16, row.row_num as u16);
                }
                let Some(target) = frame.cell_mut(cell.col as u16, row.row_num as u16) else {
                    continue;
                };
                if cell.unknown {
                    // An unknown prediction says "something will be here", not
                    // "a blank will be here" — painting a space over it is what
                    // made every keystroke erase the rest of the line.
                    if flagging && pending {
                        target.style.underline = true;
                    }
                    continue;
                }
                target.text = cell.replacement.to_string();
                target.wide = false;
                target.spacer = false;
                if flagging && pending {
                    target.style.underline = true;
                }
            }
        }

        // The predicted cursor is hidden while tentative, like its cells.
        if let Some((row, col)) = self.display_cursor() {
            if row < rows && col < cols {
                frame.cursor_y = row as u16;
                frame.cursor_x = col as u16;
            }
        }
    }
}

/// Blank the other half of the wide glyph at (`col`, `row`), if there is one,
/// because something narrow is about to be painted over this half.
///
/// A terminal destroys the whole glyph when either half is overwritten, and the
/// frame has to say so. Left half-standing, the diff would skip the column as
/// belonging to the glyph, and never draw what was predicted there.
fn split_wide_glyph(frame: &mut ScreenSnapshot, col: u16, row: u16) {
    let Some(cell) = frame.cell(col, row) else {
        return;
    };
    let partner = match (cell.wide, cell.spacer) {
        (true, _) => col + 1,
        (_, true) if col > 0 && frame.cell(col - 1, row).is_some_and(|c| c.wide) => col - 1,
        _ => return,
    };
    if let Some(other) = frame.cell_mut(partner, row) {
        other.text.clear();
        other.wide = false;
        other.spacer = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DisplayPreference;
    use morsh_terminal::{CellData, DisplayDiff, MoshTerminal};
    use serial_test::serial;

    const COLS: u16 = 20;
    const ROWS: u16 = 3;

    /// Below the underline threshold: predicted cells look like real ones, so
    /// the diff can't tell them apart from what the server sends afterwards.
    const UNFLAGGED_SEND_INTERVAL_MS: u64 = 35;

    /// An engine with a confirmed epoch, so its predictions are displayed.
    fn confirmed_engine() -> PredictionEngine {
        let mut engine = PredictionEngine::new();
        engine.set_display_preference(DisplayPreference::Adaptive);
        engine.set_send_interval(UNFLAGGED_SEND_INTERVAL_MS);
        let mut screen = ScreenSnapshot::new(COLS, ROWS);
        screen.cursor_y = 2;
        engine.new_user_byte('Q', &screen);
        engine.set_local_frame_sent(1);
        engine.set_local_frame_late_acked(1);
        screen.rows[2][0].text = "Q".to_string();
        screen.cursor_x = 1;
        engine.validate_predictions(&screen);
        engine
    }

    /// Type `key` on `line`, then let the server catch up with `server_edit`,
    /// driving a second terminal the way the client drives the user's. Returns
    /// the rows the server has and the rows the user ends up looking at.
    fn after_catch_up(
        line: &str,
        cursor: &str,
        key: char,
        server_edit: &str,
    ) -> (ScreenSnapshot, ScreenSnapshot) {
        let mut server = MoshTerminal::new(COLS, ROWS, 0).unwrap();
        server.write(line.as_bytes());
        server.write(cursor.as_bytes());
        let before = server.snapshot().unwrap();

        let mut user = MoshTerminal::new(COLS, ROWS, 0).unwrap();
        user.write(DisplayDiff::diff(&user.snapshot().unwrap(), &before).as_bytes());

        let mut engine = confirmed_engine();
        engine.new_user_byte(key, &before);
        let mut predicted = before.clone();
        engine.apply(&mut predicted);
        user.write(DisplayDiff::diff(&before, &predicted).as_bytes());

        server.write(server_edit.as_bytes());
        let after = server.snapshot().unwrap();
        user.write(DisplayDiff::diff(&predicted, &after).as_bytes());
        (after, user.snapshot().unwrap())
    }

    const INSERT_X: &str = "\x1b[4hX\x1b[4l";

    #[test]
    #[serial]
    fn insert_before_wide_text_leaves_the_screen_matching_the_server() {
        let (server, user) = after_catch_up("ab日本語cd", "\x1b[1;2H", 'X', INSERT_X);
        assert_eq!(user.rows, server.rows);
    }

    #[test]
    #[serial]
    fn backspace_before_wide_text_leaves_the_screen_matching_the_server() {
        let (server, user) = after_catch_up("abc日本語de", "\x1b[1;3H", '\x7f', "\x1b[D\x1b[P");
        assert_eq!(user.rows, server.rows);
    }

    #[test]
    #[serial]
    fn insert_before_combining_text_leaves_the_screen_matching_the_server() {
        let (server, user) = after_catch_up("abe\u{301}cd", "\x1b[1;2H", 'X', INSERT_X);
        assert_eq!(user.rows, server.rows);
    }

    #[test]
    #[serial]
    fn plain_text_is_still_predicted() {
        let mut server = MoshTerminal::new(COLS, ROWS, 0).unwrap();
        server.write("日本 abcd\x1b[1;7H".as_bytes());
        let before = server.snapshot().unwrap();
        let mut engine = confirmed_engine();
        engine.new_user_byte('X', &before);
        let mut predicted = before.clone();
        engine.apply(&mut predicted);
        assert_eq!(predicted.cell(6, 0).unwrap().text, "X");
        assert_eq!(predicted.cell(7, 0).unwrap().text, "b");
    }

    #[test]
    #[serial]
    fn a_keystroke_that_would_shift_a_cluster_shows_nothing() {
        for line in ["ab日本語", "abe\u{301}cd"] {
            let mut server = MoshTerminal::new(COLS, ROWS, 0).unwrap();
            server.write(line.as_bytes());
            server.write(b"\x1b[1;2H");
            let before = server.snapshot().unwrap();
            let mut engine = confirmed_engine();
            engine.new_user_byte('X', &before);
            let mut predicted = before.clone();
            engine.apply(&mut predicted);
            assert_eq!(predicted, before, "{line}");
        }
    }

    #[test]
    #[serial]
    fn prediction_over_the_right_half_of_a_wide_glyph_is_drawn() {
        let mut terminal = MoshTerminal::new(COLS, ROWS, 0).unwrap();
        terminal.write("日本".as_bytes());
        let before = terminal.snapshot().unwrap();

        // The server can put a wide glyph under a prediction made earlier.
        let mut frame = before.clone();
        split_wide_glyph(&mut frame, 1, 0);
        frame.cell_mut(1, 0).unwrap().text = "X".to_string();
        frame.cell_mut(1, 0).unwrap().spacer = false;

        let mut user = MoshTerminal::new(COLS, ROWS, 0).unwrap();
        user.write(DisplayDiff::diff(&user.snapshot().unwrap(), &before).as_bytes());
        user.write(DisplayDiff::diff(&before, &frame).as_bytes());
        assert_eq!(user.snapshot().unwrap().rows[0][..4], frame.rows[0][..4]);
    }

    #[test]
    #[serial]
    fn splitting_a_wide_glyph_clears_its_other_half() {
        let mut terminal = MoshTerminal::new(COLS, ROWS, 0).unwrap();
        terminal.write("日".as_bytes());
        let mut frame = terminal.snapshot().unwrap();
        split_wide_glyph(&mut frame, 0, 0);
        assert!(!frame.cell(1, 0).unwrap().spacer);

        let mut frame = terminal.snapshot().unwrap();
        split_wide_glyph(&mut frame, 1, 0);
        assert_eq!(*frame.cell(0, 0).unwrap(), CellData::empty());
    }
}
