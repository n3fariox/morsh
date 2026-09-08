use crate::overlay::{ConditionalCursorMove, Overlay, Validity};
use std::time::Instant;

/// Display preference for predictions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayPreference {
    Always,
    Never,
    Adaptive,
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
        self.overlay.rows().any(|r| r.cells.iter().any(|c| c.active))
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
            DisplayPreference::Adaptive => {
                self.srtt_trigger || self.glitch_trigger > 0
            }
        }
    }

    /// Become tentative (reset epochs to prevent showing predictions until confirmed).
    fn become_tentative(&mut self) {
        self.prediction_epoch += 1;
    }

    /// Initialize cursor prediction if none exists.
    fn init_cursor(&mut self, cursor_row: usize, cursor_col: usize) {
        let dominated = self.cursors.last().is_some_and(|c| {
            c.tentative_until_epoch == self.prediction_epoch
        });

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
        self.cursors.last().filter(|c| c.active).map(|c| (c.row, c.col))
    }

    /// Reset all predictions.
    pub fn reset(&mut self) {
        self.cursors.clear();
        self.overlay = Overlay::new();
        self.become_tentative();
    }

    /// Process a user keystroke and create predictions.
    ///
    /// `ch` is the character typed and `cursor_row`/`cursor_col` is the current
    /// cursor position. `cell_at(row, col)` reads the character the *server*
    /// last put at a position: predicting an insert means shifting the real
    /// contents of the line rightward, so the engine has to be able to see them.
    pub fn new_user_byte<F>(
        &mut self,
        ch: char,
        cursor_row: usize,
        cursor_col: usize,
        num_cols: usize,
        num_rows: usize,
        cell_at: F,
    ) where
        F: Fn(usize, usize) -> char,
    {
        if self.display_preference == DisplayPreference::Never {
            return;
        }

        // Check for terminal resize
        if self.last_height != num_rows || self.last_width != num_cols {
            self.last_height = num_rows;
            self.last_width = num_cols;
            self.reset();
        }

        // First cull existing predictions
        self.cull(cursor_row, cursor_col, num_rows, num_cols);

        self.init_cursor(cursor_row, cursor_col);

        if ch == '\x7f' {
            self.predict_backspace(cursor_row, cursor_col, num_cols, &cell_at);
        } else if ch == '\r' || ch == '\n' {
            // A newline moves the cursor somewhere we cannot predict the
            // contents of, so the whole epoch becomes tentative.
            self.become_tentative();
            self.newline_carriage_return(cursor_row, num_cols, num_rows);
        } else if ch.is_control() || (!ch.is_ascii_graphic() && ch != ' ') {
            // Unknown control character — become tentative
            self.become_tentative();
        } else {
            self.predict_insert(ch, cursor_row, cursor_col, num_cols, num_rows, &cell_at);
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
        F: Fn(usize, usize) -> char,
    {
        if cursor_col == 0 {
            return;
        }

        let new_col = cursor_col - 1;
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
                    cell.replacement = cell_at(cursor_row, i + 1);
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
        F: Fn(usize, usize) -> char,
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
                    cell.replacement = cell_at(cursor_row, i - 1);
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

    /// Validate predictions against actual server state and update accordingly.
    ///
    /// Call this when a new server state arrives. For each prediction,
    /// compare against `get_cell(row, col)` to confirm or cull.
    pub fn validate_predictions<F>(&mut self, get_cell: F, cursor_row: usize, cursor_col: usize)
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
        self.cursors
            .retain(|c| c.get_validity(cursor_row, cursor_col, num_rows, num_cols, self.local_frame_late_acked) == Validity::Pending);

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

impl Default for PredictionEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        engine.new_user_byte('A', 0, 0, 80, 24, |_, _| ' ');
        assert!(!engine.is_active());
    }

    #[test]
    fn prediction_engine_predicts_char() {
        let mut engine = PredictionEngine::new();
        engine.set_display_preference(DisplayPreference::Always);

        engine.new_user_byte('A', 5, 10, 80, 24, |_, _| ' ');
        assert!(engine.is_active());

        let cursor = engine.cursor_pos();
        assert_eq!(cursor, Some((5, 11))); // moved right
    }

    #[test]
    fn prediction_engine_backspace() {
        let mut engine = PredictionEngine::new();
        engine.set_display_preference(DisplayPreference::Always);

        // Type a character first
        engine.new_user_byte('A', 5, 10, 80, 24, |_, _| ' ');

        // Then backspace
        engine.new_user_byte('\x7f', 5, 11, 80, 24, |_, _| ' ');

        let cursor = engine.cursor_pos();
        assert_eq!(cursor, Some((5, 10))); // moved back left
    }

    #[test]
    fn prediction_engine_reset() {
        let mut engine = PredictionEngine::new();
        engine.set_display_preference(DisplayPreference::Always);

        engine.new_user_byte('A', 0, 0, 80, 24, |_, _| ' ');
        assert!(engine.is_active());

        engine.reset();
        assert!(!engine.is_active());
    }

    #[test]
    fn prediction_engine_epoch_management() {
        let mut engine = PredictionEngine::new();
        engine.set_display_preference(DisplayPreference::Always);

        let initial_epoch = engine.prediction_epoch;
        engine.new_user_byte('A', 0, 0, 80, 24, |_, _| ' ');

        // Epoch should increase after becoming tentative
        assert!(engine.prediction_epoch >= initial_epoch);
    }

    #[test]
    fn prediction_engine_wait_time() {
        let mut engine = PredictionEngine::new();
        engine.set_display_preference(DisplayPreference::Always);

        // No active predictions — wait time is max
        assert_eq!(engine.wait_time_ms(), u64::MAX);

        // With active predictions and timing tests necessary
        engine.new_user_byte('A', 0, 0, 80, 24, |_, _| ' ');
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

    /// Regression: predicting an insert used to mark every cell from the
    /// cursor to the right margin `unknown`, which the renderer painted as
    /// spaces — so each keystroke visually erased the rest of the line.
    #[test]
    fn insert_shifts_the_real_line_contents_rightward() {
        let mut engine = PredictionEngine::new();
        engine.set_local_frame_sent(1);

        // The server's line reads "hello" starting at column 2.
        let line: Vec<char> = "  hello".chars().collect();
        let cell_at = move |_row: usize, col: usize| *line.get(col).unwrap_or(&' ');

        // Type 'X' at column 2, in front of "hello".
        engine.new_user_byte('X', 0, 2, 20, 5, cell_at);

        let row = engine.overlay().get_row(0).expect("overlay row");
        assert_eq!(row.cells[2].replacement, 'X');
        assert!(!row.cells[2].unknown);

        // "hello" shifted one column right, rather than being blanked.
        let shifted: String = (3..8).map(|i| row.cells[i].replacement).collect();
        assert_eq!(shifted, "hello", "prior contents were not shifted rightward");
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

        let line: Vec<char> = "abcdef".chars().collect();
        let cell_at = move |_row: usize, col: usize| *line.get(col).unwrap_or(&' ');

        // Backspace with the cursor just after 'c'.
        engine.new_user_byte('\x7f', 0, 3, 20, 5, cell_at);

        let row = engine.overlay().get_row(0).expect("overlay row");
        let shifted: String = (2..5).map(|i| row.cells[i].replacement).collect();
        assert_eq!(shifted, "def", "prior contents were not shifted leftward");
    }
}
