use std::collections::HashMap;
use std::time::Instant;

/// Validity of a prediction against the current server state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validity {
    /// Not yet confirmed by server (waiting for ack).
    Pending,
    /// Prediction matches server state — confirmed.
    Correct,
    /// Prediction matches but is trivial (blank matches blank).
    CorrectNoCredit,
    /// Prediction doesn't match or has expired.
    IncorrectOrExpired,
    /// Prediction is inactive.
    Inactive,
}

/// A predicted cell replacement.
#[derive(Debug, Clone)]
pub struct ConditionalOverlayCell {
    /// When this prediction expires (frame number).
    pub expiration_frame: u64,
    /// Column position.
    pub col: usize,
    /// Whether this cell is actively predicting.
    pub active: bool,
    /// Epoch until which this prediction is tentative.
    pub tentative_until_epoch: u64,
    /// When this prediction was created (for glitch detection).
    pub prediction_time: Instant,
    /// The predicted character.
    pub replacement: char,
    /// Whether the prediction is for an unknown character (e.g. beyond last column).
    pub unknown: bool,
    /// Original cell contents before prediction (for credit tracking).
    pub original_contents: Vec<char>,
}

impl ConditionalOverlayCell {
    pub fn new(col: usize, epoch: u64) -> Self {
        Self {
            expiration_frame: 0,
            col,
            active: false,
            tentative_until_epoch: epoch,
            prediction_time: Instant::now(),
            replacement: ' ',
            unknown: false,
            original_contents: Vec::new(),
        }
    }

    /// Whether this prediction is still tentative (not yet confirmed).
    pub fn is_tentative(&self, confirmed_epoch: u64) -> bool {
        self.tentative_until_epoch > confirmed_epoch
    }

    /// Reset to inactive state.
    pub fn reset(&mut self) {
        self.active = false;
        self.expiration_frame = u64::MAX;
        self.tentative_until_epoch = u64::MAX;
        self.unknown = false;
        self.original_contents.clear();
    }

    /// Reset but preserve original contents for credit tracking.
    pub fn reset_with_orig(&mut self) {
        if !self.active || self.unknown {
            self.reset();
            return;
        }
        self.original_contents.push(self.replacement);
        self.active = false;
        self.expiration_frame = u64::MAX;
        self.tentative_until_epoch = u64::MAX;
    }

    /// Mark as active with an expiration.
    pub fn expire(&mut self, frame: u64, time: Instant) {
        self.expiration_frame = frame;
        self.prediction_time = time;
    }

    /// Check validity against the actual cell content.
    pub fn get_validity(&self, actual: char, row_height: usize, col_width: usize, _early_ack: u64, late_ack: u64) -> Validity {
        if !self.active {
            return Validity::Inactive;
        }
        if row_height == 0 || self.col >= col_width {
            return Validity::IncorrectOrExpired;
        }

        // Not yet confirmed by server
        if late_ack < self.expiration_frame {
            return Validity::Pending;
        }

        if self.unknown {
            return Validity::CorrectNoCredit;
        }

        if self.replacement == ' ' && actual == ' ' {
            return Validity::CorrectNoCredit;
        }

        if actual == self.replacement {
            // Check if it matches original content (no credit)
            if self.original_contents.contains(&actual) {
                return Validity::CorrectNoCredit;
            }
            return Validity::Correct;
        }

        Validity::IncorrectOrExpired
    }
}

/// A predicted cursor position.
#[derive(Debug, Clone)]
pub struct ConditionalCursorMove {
    /// When this prediction expires (frame number).
    pub expiration_frame: u64,
    /// Row position.
    pub row: usize,
    /// Column position.
    pub col: usize,
    /// Whether this cursor prediction is active.
    pub active: bool,
    /// Epoch until which this prediction is tentative.
    pub tentative_until_epoch: u64,
}

impl ConditionalCursorMove {
    pub fn new(row: usize, col: usize, epoch: u64) -> Self {
        Self {
            expiration_frame: 0,
            row,
            col,
            active: false,
            tentative_until_epoch: epoch,
        }
    }

    pub fn is_tentative(&self, confirmed_epoch: u64) -> bool {
        self.tentative_until_epoch > confirmed_epoch
    }

    /// Check validity against actual cursor position.
    pub fn get_validity(&self, actual_row: usize, actual_col: usize, row_height: usize, col_width: usize, late_ack: u64) -> Validity {
        if !self.active {
            return Validity::Inactive;
        }
        if self.row >= row_height || self.col >= col_width {
            return Validity::IncorrectOrExpired;
        }
        if late_ack >= self.expiration_frame {
            if actual_row == self.row && actual_col == self.col {
                return Validity::Correct;
            }
            return Validity::IncorrectOrExpired;
        }
        Validity::Pending
    }
}

/// A row of overlay cells.
#[derive(Debug, Clone)]
pub struct OverlayRow {
    pub row_num: usize,
    pub cells: Vec<ConditionalOverlayCell>,
}

impl OverlayRow {
    pub fn new(row_num: usize, num_cols: usize, epoch: u64) -> Self {
        let cells = (0..num_cols)
            .map(|col| ConditionalOverlayCell::new(col, epoch))
            .collect();
        Self { row_num, cells }
    }
}

/// Manages all overlay rows.
#[derive(Debug, Clone)]
pub struct Overlay {
    rows: HashMap<usize, OverlayRow>,
}

impl Default for Overlay {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay {
    pub fn new() -> Self {
        Self {
            rows: HashMap::new(),
        }
    }

    /// Get or create a row with the given number.
    pub fn get_or_make_row(&mut self, row_num: usize, num_cols: usize, epoch: u64) -> &mut OverlayRow {
        self.rows
            .entry(row_num)
            .or_insert_with(|| OverlayRow::new(row_num, num_cols, epoch))
    }

    /// Get a reference to a row.
    pub fn get_row(&self, row_num: usize) -> Option<&OverlayRow> {
        self.rows.get(&row_num)
    }

    /// Get a mutable reference to a row.
    pub fn get_row_mut(&mut self, row_num: usize) -> Option<&mut OverlayRow> {
        self.rows.get_mut(&row_num)
    }

    /// Iterate over all rows in order.
    pub fn rows(&self) -> impl Iterator<Item = &OverlayRow> {
        let mut rows: Vec<_> = self.rows.values().collect();
        rows.sort_by_key(|r| r.row_num);
        rows.into_iter()
    }

    /// Iterate over all rows mutably.
    pub fn rows_mut(&mut self) -> impl Iterator<Item = &mut OverlayRow> {
        let mut rows: Vec<_> = self.rows.values_mut().collect();
        rows.sort_by_key(|r| r.row_num);
        rows.into_iter()
    }

    /// Remove rows outside the terminal bounds.
    pub fn prune(&mut self, max_row: usize) {
        self.rows.retain(|&row_num, _| row_num < max_row);
    }

    /// Clear all predictions in a given epoch.
    pub fn kill_epoch(&mut self, epoch: u64) {
        for row in self.rows.values_mut() {
            for cell in &mut row.cells {
                if cell.active && cell.tentative_until_epoch <= epoch {
                    cell.reset();
                }
            }
        }
    }

    /// Reset all cells.
    pub fn reset_all(&mut self) {
        for row in self.rows.values_mut() {
            for cell in &mut row.cells {
                cell.reset();
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() || self.rows.values().all(|r| r.cells.iter().all(|c| !c.active))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_cell_basic() {
        let mut cell = ConditionalOverlayCell::new(5, 1);
        assert!(!cell.active);
        assert!(cell.is_tentative(0));

        cell.active = true;
        cell.replacement = 'A';
        cell.expire(10, Instant::now());

        assert_eq!(cell.get_validity('A', 24, 80, 0, 10), Validity::Correct);
        assert_eq!(cell.get_validity('B', 24, 80, 0, 10), Validity::IncorrectOrExpired);
    }

    #[test]
    fn overlay_cell_pending_before_ack() {
        let mut cell = ConditionalOverlayCell::new(0, 1);
        cell.active = true;
        cell.replacement = 'X';
        cell.expire(5, Instant::now());

        // late_ack < expiration_frame → Pending
        assert_eq!(cell.get_validity('X', 24, 80, 0, 3), Validity::Pending);
    }

    #[test]
    fn overlay_cell_inactive() {
        let cell = ConditionalOverlayCell::new(0, 1);
        assert_eq!(cell.get_validity('A', 24, 80, 0, 100), Validity::Inactive);
    }

    #[test]
    fn cursor_move_validity() {
        let mut cursor = ConditionalCursorMove::new(5, 10, 1);
        cursor.active = true;
        cursor.expiration_frame = 5;

        // Before ack
        assert_eq!(cursor.get_validity(5, 10, 24, 80, 3), Validity::Pending);

        // After ack, matches
        assert_eq!(cursor.get_validity(5, 10, 24, 80, 10), Validity::Correct);

        // After ack, doesn't match
        assert_eq!(cursor.get_validity(5, 11, 24, 80, 10), Validity::IncorrectOrExpired);
    }

    #[test]
    fn overlay_row_management() {
        let mut overlay = Overlay::new();
        let row = overlay.get_or_make_row(5, 80, 1);
        assert_eq!(row.cells.len(), 80);
        assert_eq!(row.cells[0].col, 0);
        assert_eq!(row.cells[79].col, 79);

        // Same row returns existing
        let row2 = overlay.get_or_make_row(5, 80, 1);
        assert_eq!(row2.row_num, 5);
    }

    #[test]
    fn overlay_prune() {
        let mut overlay = Overlay::new();
        overlay.get_or_make_row(0, 80, 1);
        overlay.get_or_make_row(23, 80, 1);
        overlay.get_or_make_row(25, 80, 1); // out of bounds

        overlay.prune(24);
        assert!(overlay.get_row(25).is_none());
        assert!(overlay.get_row(0).is_some());
        assert!(overlay.get_row(23).is_some());
    }
}
