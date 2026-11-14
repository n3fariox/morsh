use crate::overlay::{ConditionalCursorMove, Overlay, Validity};
use morsh_terminal::{CellData, ScreenSnapshot};
use std::time::Instant;

/// Display preference for predictions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayPreference {
    Always,
    Never,
    Adaptive,
}

/// Where we are in parsing an escape sequence typed by the user
/// (e.g. arrow keys arrive as `ESC [ D` or `ESC O D`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputState {
    Ground,
    /// Saw ESC.
    Escape,
    /// Saw `ESC [`; parameter/intermediate bytes until a final byte.
    Csi,
    /// Saw `ESC O` (application cursor/keypad mode); next byte is final.
    Ss3,
}

/// The prediction engine tracks speculative local echo for low-latency feel.
///
/// When the user types a character, the prediction engine immediately shows
/// it locally before the server confirms. When the server's state arrives,
/// predictions are confirmed (if matching) or culled (if wrong).
pub struct PredictionEngine {
    /// Overlay cells for predicted characters.
    overlay: Overlay,
    /// Cursor position predictions.
    cursors: Vec<ConditionalCursorMove>,

    /// Frame number when we last sent a packet.
    local_frame_sent: u64,
    /// Frame number of last acked packet.
    local_frame_acked: u64,
    /// Frame number of last late-acked packet.
    local_frame_late_acked: u64,

    /// Current prediction epoch (incremented on each keystroke group).
    prediction_epoch: u64,
    /// Highest confirmed epoch.
    confirmed_epoch: u64,

    /// Whether predictions are shown with underline (flagging).
    flagging: bool,
    /// Whether SRTT trigger is active (show predictions due to slow RTT).
    srtt_trigger: bool,
    /// Glitch trigger counter (show predictions temporarily after long-pending).
    glitch_trigger: u32,
    /// Last time a prediction was quickly confirmed.
    last_quick_confirmation: Instant,

    /// Current send interval from RTT.
    send_interval_ms: u64,

    /// Display preference.
    display_preference: DisplayPreference,
    /// Whether to predict in overwrite mode.
    predict_overwrite: bool,

    /// Last known terminal height.
    last_height: usize,
    /// Last known terminal width.
    last_width: usize,

    /// Escape-sequence parser state for user input.
    input_state: InputState,
}

// Thresholds (from mosh C++ source)
const SRTT_TRIGGER_LOW: u64 = 20;
const SRTT_TRIGGER_HIGH: u64 = 30;
const FLAG_TRIGGER_LOW: u64 = 50;
const FLAG_TRIGGER_HIGH: u64 = 80;
const GLITCH_THRESHOLD_MS: u64 = 250;
const GLITCH_REPAIR_COUNT: u32 = 10;
const GLITCH_REPAIR_MININTERVAL_MS: u64 = 150;
const GLITCH_FLAG_THRESHOLD_MS: u64 = 5000;

impl PredictionEngine {
    pub fn new() -> Self {
        Self {
            overlay: Overlay::new(),
            cursors: Vec::new(),
            local_frame_sent: 0,
            local_frame_acked: 0,
            local_frame_late_acked: 0,
            prediction_epoch: 1,
            confirmed_epoch: 0,
            flagging: false,
            srtt_trigger: false,
            glitch_trigger: 0,
            last_quick_confirmation: Instant::now(),
            send_interval_ms: 250,
            display_preference: DisplayPreference::Adaptive,
            predict_overwrite: false,
            last_height: 0,
            last_width: 0,
            input_state: InputState::Ground,
        }
    }

    /// Set display preference.
    pub fn set_display_preference(&mut self, pref: DisplayPreference) {
        self.display_preference = pref;
    }

    /// Set whether to use overwrite mode for predictions.
    pub fn set_predict_overwrite(&mut self, overwrite: bool) {
        self.predict_overwrite = overwrite;
    }

    /// Update the send interval from RTT measurement.
    pub fn set_send_interval(&mut self, ms: u64) {
        self.send_interval_ms = ms;
    }

    /// Record that we sent a packet.
    pub fn set_local_frame_sent(&mut self, frame: u64) {
        self.local_frame_sent = frame;
    }

    /// Record that a frame was acked.
    pub fn set_local_frame_acked(&mut self, frame: u64) {
        self.local_frame_acked = frame;
    }

    /// Record that a frame was late-acked.
    pub fn set_local_frame_late_acked(&mut self, frame: u64) {
        self.local_frame_late_acked = frame;
    }

    /// Whether predictions are currently active (showing something).
    fn is_active(&self) -> bool {
        self.overlay
            .rows()
            .any(|r| r.cells.iter().any(|c| c.active))
    }

    /// Whether timing-based triggers are still relevant.
    fn timing_tests_necessary(&self) -> bool {
        !(self.glitch_trigger > 0 && self.flagging)
    }

    /// How long to wait before next timing check (ms).
    pub fn wait_time_ms(&self) -> u64 {
        if self.timing_tests_necessary() && self.is_active() {
            50
        } else {
            u64::MAX
        }
    }

    /// Whether predictions should be displayed.
    pub fn should_display(&self) -> bool {
        match self.display_preference {
            DisplayPreference::Never => false,
            DisplayPreference::Always => true,
            DisplayPreference::Adaptive => self.srtt_trigger || self.glitch_trigger > 0,
        }
    }

    /// Become tentative (reset epochs to prevent showing predictions until confirmed).
    fn become_tentative(&mut self) {
        self.prediction_epoch += 1;
    }

    /// Input whose effect we can't predict (Enter, Ctrl-C, Up/Down, Alt+key...).
    /// The server may move the cursor anywhere, so drop the predicted cursor;
    /// the next keystroke then starts from the server's real cursor position.
    fn unknown_input(&mut self) {
        self.cursors.clear();
        self.become_tentative();
    }

    /// Highest epoch the server has confirmed. Overlay cells from later
    /// epochs are tentative and must not be displayed.
    pub fn confirmed_epoch(&self) -> u64 {
        self.confirmed_epoch
    }

    /// Initialize cursor prediction if none exists.
    fn init_cursor(&mut self, cursor_row: usize, cursor_col: usize) {
        let dominated = self
            .cursors
            .last()
            .is_some_and(|c| c.tentative_until_epoch == self.prediction_epoch);

        if self.cursors.is_empty() || !dominated {
            self.cursors.push(ConditionalCursorMove::new(
                cursor_row,
                cursor_col,
                self.prediction_epoch,
            ));
            if let Some(last) = self.cursors.last_mut() {
                last.active = true;
                last.expiration_frame = self.local_frame_sent + 1;
            }
        }
    }

    /// Get the current cursor position from the cursor predictions.
    pub fn cursor_pos(&self) -> Option<(usize, usize)> {
        self.cursors
            .last()
            .filter(|c| c.active)
            .map(|c| (c.row, c.col))
    }

    /// Predicted cursor position to show on screen, if predictions are being
    /// displayed and the prediction isn't tentative.
    pub fn display_cursor(&self) -> Option<(usize, usize)> {
        if !self.should_display() {
            return None;
        }
        self.cursors
            .last()
            .filter(|c| c.active && !c.is_tentative(self.confirmed_epoch))
            .map(|c| (c.row, c.col))
    }

    /// Reset all predictions.
    pub fn reset(&mut self) {
        self.cursors.clear();
        self.overlay = Overlay::new();
        self.become_tentative();
    }

    /// Process a user keystroke and create predictions.
    ///
    /// `ch` is the character typed and `screen` is what the user is looking
    /// at, as mosh's `new_user_byte` takes the local framebuffer. Predicting
    /// an insert means shifting the real contents of the line rightward, so
    /// the engine has to be able to see them. Overlay cells are one column
    /// wide and hold one `char`, so a keystroke that would move or land on a
    /// cell one narrow `char` can't describe (a wide glyph or a
    /// multi-codepoint cluster) is treated as unknown input rather than
    /// predicted wrongly.
    pub fn new_user_byte(&mut self, ch: char, screen: &ScreenSnapshot) {
        let (num_cols, num_rows) = (screen.cols as usize, screen.rows_count as usize);
        if self.display_preference == DisplayPreference::Never || num_cols == 0 || num_rows == 0 {
            return;
        }

        // Check for terminal resize
        if self.last_height != num_rows || self.last_width != num_cols {
            self.last_height = num_rows;
            self.last_width = num_cols;
            self.reset();
        }

        // First cull existing predictions
        self.cull(0, 0, num_rows, num_cols);

        // Only now pick the cursor: a resize above drops predictions made
        // for the old size, whose cursor may lie beyond the new one.
        let (cursor_row, cursor_col) = self
            .cursor_pos()
            .unwrap_or((screen.cursor_y as usize, screen.cursor_x as usize));
        if cursor_row >= num_rows || cursor_col >= num_cols {
            // Nowhere sensible to predict from.
            self.unknown_input();
            self.input_state = InputState::Ground;
            return;
        }
        let cell_at = |r: usize, c: usize| screen_char(screen, r, c);

        self.init_cursor(cursor_row, cursor_col);

        // Escape sequences (arrow keys, function keys, Alt+key) must not be
        // predicted as printable text.
        match self.input_state {
            InputState::Ground => {}
            InputState::Escape => {
                self.input_state = match ch {
                    '[' => InputState::Csi,
                    'O' => InputState::Ss3,
                    _ => {
                        // Alt+key or other ESC dispatch: unknown effect
                        self.unknown_input();
                        InputState::Ground
                    }
                };
                return;
            }
            InputState::Csi => {
                // Final bytes are 0x40..=0x7E; anything else is a parameter/intermediate
                if ('\x40'..='\x7e').contains(&ch) {
                    self.input_state = InputState::Ground;
                    self.predict_cursor_key(ch, num_cols, &cell_at);
                }
                return;
            }
            InputState::Ss3 => {
                self.input_state = InputState::Ground;
                self.predict_cursor_key(ch, num_cols, &cell_at);
                return;
            }
        }

        if ch == '\x1b' {
            self.input_state = InputState::Escape;
        } else if ch == '\x7f' {
            self.predict_backspace(cursor_row, cursor_col, num_cols, &cell_at);
        } else if ch.is_control() || (!ch.is_ascii_graphic() && ch != ' ') {
            // Unknown control character (including Enter) — the server may
            // move the cursor anywhere, so it's no longer predictable
            self.unknown_input();
        } else {
            self.predict_insert(ch, cursor_row, cursor_col, num_cols, num_rows, &cell_at);
        }
    }

    /// Handle the final byte of a CSI/SS3 key sequence. Left/right arrows move
    /// the predicted cursor (as stock mosh does); anything else is unknown, as
    /// is stepping over a wide glyph, which the host may do in one keypress.
    fn predict_cursor_key<F>(&mut self, final_byte: char, num_cols: usize, cell_at: &F)
    where
        F: Fn(usize, usize) -> Option<char>,
    {
        let frame = self.local_frame_sent + 1;
        match (final_byte, self.cursors.last_mut()) {
            ('C', Some(cursor)) if cell_at(cursor.row, cursor.col).is_some() => {
                if cursor.col + 1 < num_cols {
                    cursor.col += 1;
                    cursor.expiration_frame = frame;
                }
            }
            ('D', Some(cursor))
                if cursor.col == 0 || cell_at(cursor.row, cursor.col - 1).is_some() =>
            {
                if cursor.col > 0 {
                    cursor.col -= 1;
                    cursor.expiration_frame = frame;
                }
            }
            _ => self.unknown_input(),
        }
    }

    /// Move the predicted cursor to the start of the next row.
    ///
    /// Mosh's `PredictionEngine::newline_carriage_return`. On the last row it
    /// does not try to predict a scroll; it blanks the row instead.
    fn newline_carriage_return(&mut self, cursor_row: usize, num_cols: usize, num_rows: usize) {
        let frame = self.local_frame_sent + 1;
        let now = Instant::now();
        let epoch = self.prediction_epoch;

        if let Some(cursor) = self.cursors.last_mut() {
            cursor.col = 0;
            if cursor.row + 1 < num_rows {
                cursor.row += 1;
            }
            cursor.expiration_frame = frame;
        }

        if cursor_row + 1 >= num_rows {
            // Blank prediction for the last row.
            let row = self.overlay.get_or_make_row(cursor_row, num_cols, epoch);
            for cell in row.cells.iter_mut() {
                cell.active = true;
                cell.tentative_until_epoch = epoch;
                cell.expire(frame, now);
                cell.unknown = false;
                cell.replacement = ' ';
            }
        }
    }

    /// Predict a backspace: move the cursor left and shift the rest of the
    /// line leftward over the deleted cell.
    fn predict_backspace<F>(
        &mut self,
        cursor_row: usize,
        cursor_col: usize,
        num_cols: usize,
        cell_at: &F,
    ) where
        F: Fn(usize, usize) -> Option<char>,
    {
        if cursor_col == 0 {
            return;
        }

        let new_col = cursor_col - 1;
        let rightmost = if self.predict_overwrite {
            new_col
        } else {
            num_cols - 1
        };
        let Some(line) = (new_col..=rightmost)
            .map(|col| cell_at(cursor_row, col))
            .collect::<Option<Vec<char>>>()
        else {
            self.unknown_input();
            return;
        };

        let frame = self.local_frame_sent + 1;
        let now = Instant::now();
        let epoch = self.prediction_epoch;

        // Move cursor left
        if let Some(last_cursor) = self.cursors.last_mut() {
            last_cursor.col = new_col;
            last_cursor.expiration_frame = frame;
        }

        if self.predict_overwrite {
            // Just clear the cell to the left
            let row = self.overlay.get_or_make_row(cursor_row, num_cols, epoch);
            let cell = &mut row.cells[new_col];
            cell.reset_with_orig();
            cell.active = true;
            cell.tentative_until_epoch = epoch;
            cell.expire(frame, now);
            cell.unknown = false;
            cell.replacement = ' ';
            return;
        }

        // Read the right-hand neighbours before mutating, so each cell sees
        // the pre-shift state — the same values mosh reads, since it walks
        // left to right and only ever looks one column ahead.
        let right_states: Vec<(bool, bool, char)> = {
            let row = self.overlay.get_or_make_row(cursor_row, num_cols, epoch);
            (0..num_cols)
                .map(|i| {
                    if i + 1 < num_cols {
                        let next = &row.cells[i + 1];
                        (next.active, next.unknown, next.replacement)
                    } else {
                        (false, false, ' ')
                    }
                })
                .collect()
        };

        let row = self.overlay.get_or_make_row(cursor_row, num_cols, epoch);
        for (i, right) in right_states.iter().enumerate().take(num_cols).skip(new_col) {
            let cell = &mut row.cells[i];
            cell.reset_with_orig();
            cell.active = true;
            cell.tentative_until_epoch = epoch;
            cell.expire(frame, now);

            if i + 2 < num_cols {
                let (next_active, next_unknown, next_replacement) = *right;
                if next_active {
                    cell.unknown = next_unknown;
                    if !next_unknown {
                        cell.replacement = next_replacement;
                    }
                } else {
                    // Nothing predicted there: shift what the server actually
                    // has one column left.
                    cell.unknown = false;
                    cell.replacement = line[i + 1 - new_col];
                }
            } else {
                cell.unknown = true;
            }
        }
    }

    /// Predict a character insertion at the cursor position.
    fn predict_insert<F>(
        &mut self,
        ch: char,
        cursor_row: usize,
        cursor_col: usize,
        num_cols: usize,
        num_rows: usize,
        cell_at: &F,
    ) where
        F: Fn(usize, usize) -> Option<char>,
    {
        // If at the last column, things are tricky (wrap behavior varies by app)
        if cursor_col + 1 >= num_cols {
            self.become_tentative();
        }

        let rightmost = if self.predict_overwrite {
            cursor_col
        } else {
            num_cols - 1
        };

        let Some(line) = (cursor_col..=rightmost)
            .map(|col| cell_at(cursor_row, col))
            .collect::<Option<Vec<char>>>()
        else {
            self.unknown_input();
            return;
        };

        // Build the shifted cells in a temporary buffer to avoid borrow issues
        let epoch = self.prediction_epoch;
        let frame = self.local_frame_sent + 1;
        let now = Instant::now();

        // First, read the left neighbors
        let mut left_states: Vec<(bool, bool, char)> = Vec::with_capacity(rightmost + 1);
        {
            let row = self.overlay.get_or_make_row(cursor_row, num_cols, epoch);
            for i in 0..=rightmost {
                if i == 0 {
                    left_states.push((false, false, ' '));
                } else {
                    let prev = &row.cells[i - 1];
                    left_states.push((prev.active, prev.unknown, prev.replacement));
                }
            }
        }

        // Now mutate the cells
        {
            let row = self.overlay.get_or_make_row(cursor_row, num_cols, epoch);
            // Shift cells right from rightmost to cursor position
            for i in (cursor_col + 1..=rightmost).rev() {
                let cell = &mut row.cells[i];
                cell.reset_with_orig();
                cell.active = true;
                cell.tentative_until_epoch = epoch;
                cell.expire(frame, now);

                let (left_active, left_unknown, left_replacement) = left_states[i];
                if i == num_cols - 1 {
                    cell.unknown = true;
                } else if left_active {
                    if left_unknown {
                        cell.unknown = true;
                    } else {
                        cell.unknown = false;
                        cell.replacement = left_replacement;
                    }
                } else {
                    // Nothing predicted to the left: shift the cell the server
                    // actually has rightward. Substituting a blank here is what
                    // made every keystroke visually erase the rest of the line.
                    cell.unknown = false;
                    cell.replacement = line[i - 1 - cursor_col];
                }
            }

            // Place the character at cursor position
            let cell = &mut row.cells[cursor_col];
            cell.reset_with_orig();
            cell.active = true;
            cell.tentative_until_epoch = epoch;
            cell.expire(frame, now);
            cell.unknown = false;
            cell.replacement = ch;
        }

        // Move the cursor right, or wrap.
        if cursor_col + 1 < num_cols {
            if let Some(last_cursor) = self.cursors.last_mut() {
                last_cursor.col = cursor_col + 1;
                last_cursor.expiration_frame = frame;
            }
        } else {
            self.become_tentative();
            self.newline_carriage_return(cursor_row, num_cols, num_rows);
        }
    }

    /// Cull (validate) predictions against current server state.
    ///
    /// `actual_cells` maps (row, col) → actual character from the server's terminal.
    pub fn cull(
        &mut self,
        _cursor_row: usize,
        _cursor_col: usize,
        num_rows: usize,
        _num_cols: usize,
    ) {
        if self.display_preference == DisplayPreference::Never {
            return;
        }

        // Control srtt_trigger with hysteresis
        if self.send_interval_ms > SRTT_TRIGGER_HIGH {
            self.srtt_trigger = true;
        } else if self.srtt_trigger
            && self.send_interval_ms <= SRTT_TRIGGER_LOW
            && !self.is_active()
        {
            self.srtt_trigger = false;
        }

        // Control flagging with hysteresis
        if self.send_interval_ms > FLAG_TRIGGER_HIGH {
            self.flagging = true;
        } else if self.send_interval_ms <= FLAG_TRIGGER_LOW {
            self.flagging = false;
        }

        if self.glitch_trigger > GLITCH_REPAIR_COUNT {
            self.flagging = true;
        }

        // Prune out-of-bounds rows
        self.overlay.prune(num_rows);
    }

    /// Validate predictions against the server's latest screen, confirming
    /// or culling them.
    ///
    /// Call this when a new server state arrives.
    pub fn validate_predictions(&mut self, screen: &ScreenSnapshot) {
        self.validate_predictions_with(
            |r, c| {
                screen
                    .cell(c as u16, r as u16)
                    .and_then(|d| d.text.chars().next())
                    .unwrap_or(' ')
            },
            screen.cursor_y as usize,
            screen.cursor_x as usize,
        );
    }

    fn validate_predictions_with<F>(&mut self, get_cell: F, cursor_row: usize, cursor_col: usize)
    where
        F: Fn(usize, usize) -> char,
    {
        let num_rows = self.last_height;
        let num_cols = self.last_width;
        let now = Instant::now();

        // Validate overlay cells
        let mut should_reset = false;
        let mut max_confirmed = self.confirmed_epoch;

        for row in self.overlay.rows_mut() {
            let rn = row.row_num;
            for cell in &mut row.cells {
                let actual = get_cell(rn, cell.col);
                let validity = cell.get_validity(
                    actual,
                    num_rows,
                    num_cols,
                    self.local_frame_acked,
                    self.local_frame_late_acked,
                );

                match validity {
                    Validity::IncorrectOrExpired => {
                        should_reset = true;
                    }
                    Validity::Correct => {
                        if cell.tentative_until_epoch > max_confirmed {
                            max_confirmed = cell.tentative_until_epoch;
                        }
                        // Decrement glitch trigger for quick confirmations
                        let elapsed = now.duration_since(cell.prediction_time).as_millis() as u64;
                        if elapsed < GLITCH_THRESHOLD_MS
                            && self.glitch_trigger > 0
                            && now.duration_since(self.last_quick_confirmation).as_millis() as u64
                                >= GLITCH_REPAIR_MININTERVAL_MS
                        {
                            self.glitch_trigger = self.glitch_trigger.saturating_sub(1);
                            self.last_quick_confirmation = now;
                        }
                        cell.reset();
                    }
                    Validity::CorrectNoCredit => {
                        cell.reset();
                    }
                    Validity::Pending => {
                        let elapsed = now.duration_since(cell.prediction_time).as_millis() as u64;
                        if elapsed >= GLITCH_FLAG_THRESHOLD_MS {
                            self.glitch_trigger = GLITCH_REPAIR_COUNT * 2;
                        } else if elapsed >= GLITCH_THRESHOLD_MS
                            && self.glitch_trigger < GLITCH_REPAIR_COUNT
                        {
                            self.glitch_trigger = GLITCH_REPAIR_COUNT;
                        }
                    }
                    Validity::Inactive => {}
                }
            }
        }

        self.confirmed_epoch = max_confirmed;

        // Validate cursor predictions
        if let Some(last_cursor) = self.cursors.last() {
            if last_cursor.active {
                let validity = last_cursor.get_validity(
                    cursor_row,
                    cursor_col,
                    num_rows,
                    num_cols,
                    self.local_frame_late_acked,
                );
                if validity == Validity::IncorrectOrExpired {
                    should_reset = true;
                }
            }
        }

        // Remove non-pending cursors
        self.cursors.retain(|c| {
            c.get_validity(
                cursor_row,
                cursor_col,
                num_rows,
                num_cols,
                self.local_frame_late_acked,
            ) == Validity::Pending
        });

        if should_reset {
            self.reset();
        }
    }

    /// Get immutable access to the overlay.
    pub fn overlay(&self) -> &Overlay {
        &self.overlay
    }

    /// Get the current late-acked frame number.
    pub fn local_frame_late_acked(&self) -> u64 {
        self.local_frame_late_acked
    }

    /// Whether flagging is active (RTT high enough to underline predictions).
    pub fn is_flagging(&self) -> bool {
        self.flagging
    }
}

/// The character at (`row`, `col`) of `screen`, or `None` for a cell one
/// narrow `char` can't describe. Off-screen reads as blank.
fn screen_char(screen: &ScreenSnapshot, row: usize, col: usize) -> Option<char> {
    screen
        .cell(col as u16, row as u16)
        .map_or(Some(' '), CellData::single_char)
}

impl Default for PredictionEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `cols` x `rows` screen showing `line` on row `row`, with the cursor
    /// at (`row`, `col`).
    fn screen(cols: u16, rows: u16, row: usize, col: usize, line: &str) -> ScreenSnapshot {
        let mut screen = ScreenSnapshot::new(cols, rows);
        for (cell, ch) in screen
            .rows
            .get_mut(row)
            .into_iter()
            .flatten()
            .zip(line.chars())
        {
            cell.text = ch.to_string();
        }
        screen.cursor_y = row as u16;
        screen.cursor_x = col as u16;
        screen
    }

    /// A blank 80x24 screen with the cursor at (`row`, `col`).
    fn blank(row: usize, col: usize) -> ScreenSnapshot {
        screen(80, 24, row, col, "")
    }

    /// A 10x5 line of 'a's with a wide glyph in columns 5 and 6.
    fn with_wide_glyph(col: usize) -> ScreenSnapshot {
        let mut screen = screen(10, 5, 0, col, "aaaaaaaaaa");
        screen.rows[0][5].text = "日".to_string();
        screen.rows[0][5].wide = true;
        screen.rows[0][6].text.clear();
        screen.rows[0][6].spacer = true;
        screen
    }

    fn engine() -> PredictionEngine {
        let mut engine = PredictionEngine::new();
        engine.set_display_preference(DisplayPreference::Always);
        engine
    }

    fn type_str(engine: &mut PredictionEngine, keys: &str, screen: &ScreenSnapshot) {
        for ch in keys.chars() {
            engine.new_user_byte(ch, screen);
        }
    }

    #[test]
    fn prediction_engine_creation() {
        let engine = PredictionEngine::new();
        assert_eq!(engine.prediction_epoch, 1);
        assert_eq!(engine.confirmed_epoch, 0);
        assert!(!engine.is_active());
        assert!(!engine.should_display());
    }

    #[test]
    fn prediction_engine_never_display() {
        let mut engine = PredictionEngine::new();
        engine.set_display_preference(DisplayPreference::Never);

        engine.new_user_byte('A', &blank(0, 0));
        assert!(!engine.is_active());
    }

    #[test]
    fn prediction_engine_predicts_char() {
        let mut engine = engine();
        engine.new_user_byte('A', &blank(5, 10));
        assert!(engine.is_active());
        assert_eq!(engine.cursor_pos(), Some((5, 11))); // moved right
    }

    #[test]
    fn arrow_keys_are_not_predicted_as_text() {
        for seq in ["\x1b[D", "\x1bOD", "\x1b[1;5D"] {
            let mut engine = engine();
            type_str(&mut engine, seq, &blank(5, 10));
            assert!(!engine.is_active(), "{seq:?} produced overlay cells");
            assert_eq!(engine.cursor_pos(), Some((5, 9)), "{seq:?}");
        }
    }

    #[test]
    fn enter_drops_predicted_cursor_and_makes_next_key_tentative() {
        let mut engine = engine();
        type_str(&mut engine, "l\r", &blank(5, 10));
        // Server decides where the cursor goes after Enter
        assert_eq!(engine.cursor_pos(), None);

        // Next key starts from the server's cursor (a new line), not the old one
        engine.new_user_byte('h', &blank(7, 2));
        assert_eq!(engine.cursor_pos(), Some((7, 3)));
        let cell = engine
            .overlay()
            .rows()
            .flat_map(|r| r.cells.iter().map(move |c| (r.row_num, c)))
            .find(|(_, c)| c.active && c.replacement == 'h')
            .map(|(row, c)| (row, c.col, c.is_tentative(engine.confirmed_epoch())));
        assert_eq!(cell, Some((7, 2, true)));
    }

    fn predicted_row(engine: &PredictionEngine, row: usize) -> String {
        engine
            .overlay()
            .get_row(row)
            .map(|r| {
                r.cells
                    .iter()
                    .map(|c| match (c.active, c.unknown) {
                        (false, _) => '.',
                        (true, true) => '?',
                        (true, false) => c.replacement,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn insert_mid_line_shifts_actual_text() {
        let mut engine = engine();
        // Cursor on 'b'; typing 'X' should predict "$ aXbc" with the tail shifted
        engine.new_user_byte('X', &screen(10, 5, 0, 3, "$ abc"));
        assert_eq!(predicted_row(&engine, 0), "...Xbc   ?");
    }

    #[test]
    fn backspace_mid_line_shifts_actual_text() {
        let mut engine = engine();
        // Cursor on 'c'; backspace removes 'b'
        engine.new_user_byte('\x7f', &screen(10, 5, 0, 4, "$ abc"));
        assert_eq!(predicted_row(&engine, 0), "...c    ??");
        assert_eq!(engine.cursor_pos(), Some((0, 3)));
    }

    #[test]
    fn insert_is_not_predicted_when_it_would_shift_a_wide_cell() {
        let mut engine = engine();
        engine.new_user_byte('X', &with_wide_glyph(1));
        assert_eq!(engine.cursor_pos(), None);
        assert_eq!(predicted_row(&engine, 0), "");
    }

    #[test]
    fn insert_right_of_a_wide_cell_is_still_predicted() {
        let mut engine = engine();
        engine.new_user_byte('X', &with_wide_glyph(7));
        assert_eq!(engine.cursor_pos(), Some((0, 8)));
    }

    #[test]
    fn backspace_is_not_predicted_over_a_wide_cell() {
        let mut engine = engine();
        engine.new_user_byte('\x7f', &with_wide_glyph(7));
        assert_eq!(engine.cursor_pos(), None);
    }

    #[test]
    fn arrows_are_not_predicted_across_a_wide_cell() {
        for (start, key) in [(5, "\x1b[C"), (7, "\x1b[D")] {
            let mut engine = engine();
            type_str(&mut engine, key, &with_wide_glyph(start));
            assert_eq!(engine.cursor_pos(), None, "{key:?}");
        }
    }

    #[test]
    fn typing_resumes_after_escape_sequence() {
        let mut engine = engine();
        type_str(&mut engine, "\x1b[C", &blank(5, 10));
        assert_eq!(engine.cursor_pos(), Some((5, 11)));
        engine.new_user_byte('x', &blank(5, 11));
        assert!(engine.is_active());
        assert_eq!(engine.cursor_pos(), Some((5, 12)));
    }

    #[test]
    fn prediction_engine_backspace() {
        let mut engine = engine();
        type_str(&mut engine, "A\x7f", &blank(5, 10));
        assert_eq!(engine.cursor_pos(), Some((5, 10))); // moved back left
    }

    #[test]
    fn prediction_engine_reset() {
        let mut engine = engine();
        engine.new_user_byte('A', &blank(0, 0));
        assert!(engine.is_active());

        engine.reset();
        assert!(!engine.is_active());
    }

    #[test]
    fn prediction_engine_epoch_management() {
        let mut engine = engine();
        let initial_epoch = engine.prediction_epoch;
        engine.new_user_byte('A', &blank(0, 0));

        // Epoch should increase after becoming tentative
        assert!(engine.prediction_epoch >= initial_epoch);
    }

    #[test]
    fn prediction_engine_wait_time() {
        let mut engine = engine();

        // No active predictions — wait time is max
        assert_eq!(engine.wait_time_ms(), u64::MAX);

        // With active predictions and timing tests necessary
        engine.new_user_byte('A', &blank(0, 0));
        assert_eq!(engine.wait_time_ms(), 50);
    }

    #[test]
    fn prediction_engine_send_interval_triggers() {
        let mut engine = PredictionEngine::new();
        engine.set_display_preference(DisplayPreference::Adaptive);

        // Low RTT — no trigger
        engine.set_send_interval(10);
        engine.cull(0, 0, 24, 80);
        assert!(!engine.srtt_trigger);

        // High RTT — trigger
        engine.set_send_interval(50);
        engine.cull(0, 0, 24, 80);
        assert!(engine.srtt_trigger);
    }

    /// The server applied our resize while a prediction near the old right
    /// margin was still pending: the next keystroke must start from the new
    /// screen's cursor, not index the narrower row at the old column.
    #[test]
    fn keystroke_after_the_screen_narrows_does_not_panic() {
        let mut engine = engine();
        engine.new_user_byte('a', &blank(5, 70));
        assert_eq!(engine.cursor_pos(), Some((5, 71)));

        let narrow = screen(40, 24, 5, 10, "");
        for ch in "bc\x7f\x1b[C\x1b[D".chars() {
            engine.new_user_byte(ch, &narrow);
        }
        engine.validate_predictions(&narrow);
        let mut frame = narrow.clone();
        engine.apply(&mut frame);
    }

    /// A zero-sized screen (a terminal reporting 0x0 while it starts up)
    /// must not underflow the column arithmetic.
    #[test]
    fn keystroke_on_an_empty_screen_does_nothing() {
        let mut engine = engine();
        let empty = screen(0, 0, 0, 0, "");
        for ch in "a\x7f\r\x1b[C".chars() {
            engine.new_user_byte(ch, &empty);
        }
        assert!(!engine.is_active());
    }

    /// The server's cursor can sit past the last row or column in a stale
    /// or odd frame; predicting from there must not index out of bounds.
    #[test]
    fn keystroke_with_the_cursor_off_screen_does_not_panic() {
        for (row, col) in [(3, 2), (0, 10), (9, 99)] {
            let mut engine = engine();
            let screen = screen(10, 3, 0, 0, "abc");
            let mut screen = screen.clone();
            screen.cursor_y = row;
            screen.cursor_x = col;
            for ch in "x\x7f".chars() {
                engine.new_user_byte(ch, &screen);
            }
        }
    }

    /// Regression: predicting an insert used to mark every cell from the
    /// cursor to the right margin `unknown`, which the renderer painted as
    /// spaces — so each keystroke visually erased the rest of the line.
    #[test]
    fn insert_shifts_the_real_line_contents_rightward() {
        let mut engine = PredictionEngine::new();
        engine.set_local_frame_sent(1);

        // Type 'X' at column 2, in front of "hello".
        engine.new_user_byte('X', &screen(20, 5, 0, 2, "  hello"));

        let row = engine.overlay().get_row(0).expect("overlay row");
        assert_eq!(row.cells[2].replacement, 'X');
        assert!(!row.cells[2].unknown);

        // "hello" shifted one column right, rather than being blanked.
        let shifted: String = (3..8).map(|i| row.cells[i].replacement).collect();
        assert_eq!(
            shifted, "hello",
            "prior contents were not shifted rightward"
        );
        for i in 3..8 {
            assert!(
                !row.cells[i].unknown,
                "cell {i} was marked unknown and would render as a blank"
            );
        }
    }

    #[test]
    fn backspace_shifts_the_real_line_contents_leftward() {
        let mut engine = PredictionEngine::new();
        engine.set_local_frame_sent(1);

        // Backspace with the cursor just after 'c'.
        engine.new_user_byte('\x7f', &screen(20, 5, 0, 3, "abcdef"));

        let row = engine.overlay().get_row(0).expect("overlay row");
        let shifted: String = (2..5).map(|i| row.cells[i].replacement).collect();
        assert_eq!(shifted, "def", "prior contents were not shifted leftward");
    }
}
