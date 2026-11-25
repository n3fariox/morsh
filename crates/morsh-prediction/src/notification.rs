//! The notification bar across the top of the screen: connection status,
//! errors and the escape key help. A port of stock mosh's
//! `NotificationEngine` (src/frontend/terminaloverlay.cc).

use morsh_terminal::{CellData, CellStyle, ScreenSnapshot, StyleColor};
use std::time::{Duration, Instant};

/// Mosh's `Network::ACK_INTERVAL`: the server sends at least this often.
const ACK_INTERVAL: Duration = Duration::from_millis(3000);
/// No word from the server for this long (two missed heartbeats) and we
/// start counting up.
const SERVER_LATE: Duration = Duration::from_millis(6500);
/// Our state unacknowledged for this long and we start counting up.
const REPLY_LATE: Duration = Duration::from_millis(10000);
/// How long a non-permanent notification stays up.
const MESSAGE_LIFETIME: Duration = Duration::from_millis(1000);
/// After this long without contact, refresh the countup less often.
const SLOW_COUNTUP_AFTER: Duration = Duration::from_secs(60);

/// Mosh's colors for the bar: white (7) on blue (4).
const BAR_FG: StyleColor = StyleColor::Palette(7);
const BAR_BG: StyleColor = StyleColor::Palette(4);

/// Status bar notification engine (shows connection status, errors).
#[derive(Debug)]
pub struct NotificationEngine {
    /// Last time we heard from the server.
    last_word_from_server: Instant,
    /// Last time our state was acked.
    last_acked_state: Instant,
    /// Current notification message.
    message: String,
    /// Whether the message is a network error.
    message_is_network_error: bool,
    /// When the message expires; `None` for a permanent message.
    message_expiration: Option<Instant>,
    /// Whether to show the quit keystroke in the message.
    show_quit_keystroke: bool,
    /// The escape key description (e.g. "Ctrl-^ .").
    escape_key_string: String,
    /// Shown while the escape key waits for its second key.
    escape_key_help: String,
}

/// Mosh's `human_readable_duration`: "12 seconds", "1:05", "1:00:05".
fn human_readable_duration(secs: u64, seconds_abbr: &str) -> String {
    if secs < 60 {
        format!("{secs} {seconds_abbr}")
    } else if secs < 3600 {
        format!("{}:{:02}", secs / 60, secs % 60)
    } else {
        format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
    }
}

impl NotificationEngine {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            last_word_from_server: now,
            last_acked_state: now,
            message: String::new(),
            message_is_network_error: false,
            message_expiration: None,
            show_quit_keystroke: true,
            escape_key_string: String::new(),
            escape_key_help: String::new(),
        }
    }

    /// Record that we heard from the server.
    pub fn server_heard(&mut self, time: Instant) {
        self.last_word_from_server = time;
    }

    /// Record that our state was acked.
    pub fn server_acked(&mut self, time: Instant) {
        self.last_acked_state = time;
    }

    /// Set the escape key description.
    pub fn set_escape_key_string(&mut self, s: String) {
        self.escape_key_string = s;
    }

    /// Set the help shown while the escape key waits for its second key.
    pub fn set_escape_key_help(&mut self, s: String) {
        self.escape_key_help = s;
    }

    /// Show the escape key help until [`Self::hide_escape_help`]. The quit
    /// keystroke is left off, since the help already says how to quit.
    pub fn show_escape_help(&mut self) {
        self.set_notification(self.escape_key_help.clone(), true);
        self.show_quit_keystroke = false;
    }

    /// Take down the escape key help, unless something else has replaced it.
    pub fn hide_escape_help(&mut self) {
        if !self.message_is_network_error && self.message == self.escape_key_help {
            self.message.clear();
            self.show_quit_keystroke = true;
        }
    }

    /// Show a notification message: for a second, or until replaced if
    /// `permanent`. An empty message takes the current one down.
    pub fn set_notification(&mut self, msg: String, permanent: bool) {
        self.message = msg;
        self.message_is_network_error = false;
        self.show_quit_keystroke = true;
        self.message_expiration = if permanent {
            None
        } else {
            Some(Instant::now() + MESSAGE_LIFETIME)
        };
    }

    /// The message currently set, if any.
    pub fn notification(&self) -> &str {
        &self.message
    }

    /// Show a network error message.
    pub fn set_network_error(&mut self, msg: String) {
        self.message = msg;
        self.message_is_network_error = true;
        self.message_expiration = Some(Instant::now() + ACK_INTERVAL + Duration::from_millis(100));
    }

    /// Let a network error, if one is displayed, expire within a second.
    pub fn clear_network_error(&mut self) {
        if self.message_is_network_error {
            let soon = Instant::now() + MESSAGE_LIFETIME;
            self.message_expiration = Some(self.message_expiration.map_or(soon, |e| e.min(soon)));
        }
    }

    /// Drop the message once it has expired.
    pub fn adjust_message(&mut self) {
        self.adjust_message_at(Instant::now());
    }

    fn adjust_message_at(&mut self, now: Instant) {
        if self.message_expiration.is_some_and(|e| now >= e) {
            self.message.clear();
            self.message_is_network_error = false;
            self.message_expiration = None;
        }
    }

    fn server_late_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_word_from_server) > SERVER_LATE
    }

    fn reply_late_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_acked_state) > REPLY_LATE
    }

    fn need_countup_at(&self, now: Instant) -> bool {
        self.server_late_at(now) || self.reply_late_at(now)
    }

    /// Whether the server is late (no contact in >6.5s).
    pub fn server_late(&self) -> bool {
        self.server_late_at(Instant::now())
    }

    /// Whether our replies are late (no ack in >10s).
    pub fn reply_late(&self) -> bool {
        self.reply_late_at(Instant::now())
    }

    /// Whether we need to show a "Last contact" countup.
    pub fn need_countup(&self) -> bool {
        self.need_countup_at(Instant::now())
    }

    /// Whether there's anything to display.
    pub fn has_message(&self) -> bool {
        !self.message.is_empty() || self.need_countup()
    }

    /// How long until the bar next needs redrawing, in milliseconds: when the
    /// message expires, or the next tick of the countup.
    pub fn wait_time_ms(&self) -> u64 {
        self.wait_time_at(Instant::now()).as_millis() as u64
    }

    fn wait_time_at(&self, now: Instant) -> Duration {
        let mut next = Duration::MAX;
        if let Some(expiration) = self.message_expiration {
            next = next.min(expiration.saturating_duration_since(now));
        }
        if self.need_countup_at(now) {
            // Long disconnected: save power by updating less often (mosh #243).
            let interval =
                if now.saturating_duration_since(self.last_word_from_server) > SLOW_COUNTUP_AFTER {
                    ACK_INTERVAL
                } else {
                    Duration::from_secs(1)
                };
            next = next.min(interval);
        }
        next
    }

    /// Get the notification text to render.
    pub fn get_text(&self) -> String {
        self.text_at(Instant::now())
    }

    fn text_at(&self, now: Instant) -> String {
        let time_expired = self.need_countup_at(now);

        if self.message.is_empty() && !time_expired {
            return String::new();
        }

        let keystroke_suffix = if self.show_quit_keystroke && !self.escape_key_string.is_empty() {
            format!(" [To quit: {}]", self.escape_key_string)
        } else {
            String::new()
        };

        // Prefer "contact" when both are late: no reply is expected without it.
        let (since, explanation) = if self.reply_late_at(now) && !self.server_late_at(now) {
            (self.last_acked_state, "reply")
        } else {
            (self.last_word_from_server, "contact")
        };
        let elapsed = now.saturating_duration_since(since).as_secs();

        if self.message.is_empty() {
            format!(
                "morsh: Last {explanation} {} ago.{keystroke_suffix}",
                human_readable_duration(elapsed, "seconds")
            )
        } else if !time_expired {
            format!("morsh: {}{keystroke_suffix}", self.message)
        } else {
            format!(
                "morsh: {} ({} without {explanation}.){keystroke_suffix}",
                self.message,
                human_readable_duration(elapsed, "s")
            )
        }
    }

    /// Draw the notification bar over the top row of `frame`, as mosh's
    /// `NotificationEngine::apply` does: bold white on blue, across the full
    /// width. Folding it into the frame (rather than writing it to the
    /// terminal separately) lets the display diff erase it once it is gone.
    pub fn apply(&self, frame: &mut ScreenSnapshot) {
        self.apply_at(frame, Instant::now());
    }

    fn apply_at(&self, frame: &mut ScreenSnapshot, now: Instant) {
        let text = self.text_at(now);
        if text.is_empty() || frame.cols == 0 || frame.rows_count == 0 {
            return;
        }

        if frame.cursor_y == 0 {
            frame.cursor_visible = false;
        }

        let bar = CellData {
            text: String::new(),
            wide: false,
            spacer: false,
            style: CellStyle {
                fg: BAR_FG,
                bg: BAR_BG,
                ..CellStyle::default_style()
            },
        };
        let Some(row) = frame.rows.first_mut() else {
            return;
        };
        for cell in row.iter_mut() {
            *cell = bar.clone();
        }
        // Every character is taken as one column wide; control characters
        // would corrupt the display and are dropped.
        for (cell, ch) in row.iter_mut().zip(text.chars().filter(|c| !c.is_control())) {
            cell.text = ch.to_string();
            cell.style.bold = true;
        }
    }
}

impl Default for NotificationEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_with_quit_key() -> NotificationEngine {
        let mut engine = NotificationEngine::new();
        engine.set_escape_key_string("Ctrl-^ .".to_string());
        engine
    }

    fn row_text(frame: &ScreenSnapshot, row: usize) -> String {
        frame.rows[row]
            .iter()
            .map(|c| {
                if c.text.is_empty() {
                    " "
                } else {
                    c.text.as_str()
                }
            })
            .collect()
    }

    #[test]
    fn notification_basic() {
        let mut engine = NotificationEngine::new();
        assert!(!engine.has_message());

        engine.set_notification("Connected".to_string(), false);
        assert!(engine.has_message());
        assert!(!engine.need_countup());
    }

    #[test]
    fn escape_help_hides_quit_keystroke_and_only_clears_itself() {
        let mut engine = engine_with_quit_key();
        engine.set_escape_key_help("Commands: ...".to_string());

        engine.show_escape_help();
        assert_eq!(engine.get_text(), "morsh: Commands: ...");
        engine.hide_escape_help();
        assert!(!engine.has_message());

        // Something else replaced the help: leave it up, with the quit keystroke.
        engine.show_escape_help();
        engine.set_notification("Exiting on user request...".to_string(), true);
        engine.hide_escape_help();
        assert_eq!(
            engine.get_text(),
            "morsh: Exiting on user request... [To quit: Ctrl-^ .]"
        );
    }

    #[test]
    fn server_late_after_six_and_a_half_seconds() {
        let mut engine = NotificationEngine::new();
        let now = Instant::now();

        engine.last_word_from_server = now - Duration::from_millis(6400);
        assert!(!engine.server_late_at(now));
        assert_eq!(engine.text_at(now), "");

        engine.last_word_from_server = now - Duration::from_millis(6600);
        assert!(engine.server_late_at(now));
        assert!(engine.need_countup_at(now));
        assert_eq!(engine.text_at(now), "morsh: Last contact 6 seconds ago.");
    }

    #[test]
    fn last_contact_wording_matches_mosh() {
        let mut engine = engine_with_quit_key();
        let now = Instant::now();
        engine.last_word_from_server = now - Duration::from_millis(12_300);
        engine.last_acked_state = now - Duration::from_millis(12_300);
        assert_eq!(
            engine.text_at(now),
            "morsh: Last contact 12 seconds ago. [To quit: Ctrl-^ .]"
        );

        engine.last_word_from_server = now - Duration::from_secs(65);
        assert_eq!(
            engine.text_at(now),
            "morsh: Last contact 1:05 ago. [To quit: Ctrl-^ .]"
        );

        engine.last_word_from_server = now - Duration::from_secs(3600 + 2 * 60 + 3);
        assert_eq!(
            engine.text_at(now),
            "morsh: Last contact 1:02:03 ago. [To quit: Ctrl-^ .]"
        );
    }

    #[test]
    fn last_reply_when_only_acks_are_late() {
        let mut engine = engine_with_quit_key();
        let now = Instant::now();
        engine.last_word_from_server = now - Duration::from_secs(1);
        engine.last_acked_state = now - Duration::from_secs(15);
        assert!(engine.reply_late_at(now));
        assert_eq!(
            engine.text_at(now),
            "morsh: Last reply 15 seconds ago. [To quit: Ctrl-^ .]"
        );

        // Both late: "contact" wins.
        engine.last_word_from_server = now - Duration::from_secs(11);
        assert_eq!(
            engine.text_at(now),
            "morsh: Last contact 11 seconds ago. [To quit: Ctrl-^ .]"
        );
    }

    #[test]
    fn message_combined_with_countup() {
        let mut engine = engine_with_quit_key();
        engine.set_notification("Exiting on user request...".to_string(), true);
        let now = Instant::now();
        engine.last_word_from_server = now - Duration::from_secs(8);
        assert_eq!(
            engine.text_at(now),
            "morsh: Exiting on user request... (8 s without contact.) [To quit: Ctrl-^ .]"
        );
    }

    #[test]
    fn contact_resuming_takes_the_bar_down() {
        let mut engine = NotificationEngine::new();
        let mut frame = ScreenSnapshot::new(80, 24);
        let now = Instant::now();
        engine.last_word_from_server = now - Duration::from_secs(10);
        engine.apply_at(&mut frame, now);
        assert!(row_text(&frame, 0).starts_with("morsh: Last contact 10 seconds ago."));

        engine.server_heard(now);
        engine.server_acked(now);
        let mut fresh = ScreenSnapshot::new(80, 24);
        engine.apply_at(&mut fresh, now);
        assert_eq!(fresh, ScreenSnapshot::new(80, 24));
    }

    #[test]
    fn apply_draws_bar_across_top_row() {
        let mut engine = NotificationEngine::new();
        engine.set_notification("Hi".to_string(), true);
        let mut frame = ScreenSnapshot::new(20, 3);
        frame.rows[1][0].text = "x".to_string();
        frame.cursor_y = 0;
        engine.apply(&mut frame);

        assert_eq!(row_text(&frame, 0), format!("{:<20}", "morsh: Hi"));
        let text_cell = &frame.rows[0][0].style;
        assert!(text_cell.bold);
        assert_eq!((text_cell.fg, text_cell.bg), (BAR_FG, BAR_BG));
        let blank = &frame.rows[0][19].style;
        assert!(!blank.bold);
        assert_eq!((blank.fg, blank.bg), (BAR_FG, BAR_BG));
        // The rest of the screen is untouched, and the bar hides the cursor.
        assert_eq!(frame.rows[1][0].text, "x");
        assert!(!frame.cursor_visible);
    }

    #[test]
    fn apply_truncates_to_width() {
        let mut engine = NotificationEngine::new();
        engine.set_notification("a long message".to_string(), true);
        let mut frame = ScreenSnapshot::new(10, 2);
        frame.cursor_y = 1;
        engine.apply(&mut frame);
        assert_eq!(row_text(&frame, 0), "morsh: a l");
        assert!(frame.cursor_visible);
    }

    #[test]
    fn messages_expire() {
        let mut engine = NotificationEngine::new();
        engine.set_notification("Brief".to_string(), false);
        let later = Instant::now() + Duration::from_millis(1100);
        engine.adjust_message_at(later);
        assert_eq!(engine.notification(), "");

        engine.set_notification("Perm".to_string(), true);
        assert!(engine.message_expiration.is_none());
        engine.adjust_message_at(later);
        assert_eq!(engine.notification(), "Perm");
    }

    #[test]
    fn network_error_clears_within_a_second() {
        let mut engine = NotificationEngine::new();
        engine.set_network_error("Network is unreachable".to_string());
        let now = Instant::now();
        engine.adjust_message_at(now + Duration::from_millis(1100));
        assert_eq!(engine.notification(), "Network is unreachable");

        engine.clear_network_error();
        engine.adjust_message_at(Instant::now() + Duration::from_millis(1100));
        assert_eq!(engine.notification(), "");
    }

    #[test]
    fn network_error_keeps_quit_keystroke_setting() {
        // Stock mosh's set_network_error leaves show_quit_keystroke alone.
        let mut engine = engine_with_quit_key();
        engine.show_escape_help();
        engine.set_network_error("Network is unreachable".to_string());
        assert_eq!(engine.get_text(), "morsh: Network is unreachable");

        engine.set_notification(String::new(), true);
        engine.set_network_error("Network is unreachable".to_string());
        assert_eq!(
            engine.get_text(),
            "morsh: Network is unreachable [To quit: Ctrl-^ .]"
        );
    }

    #[test]
    fn wait_time_follows_countup() {
        let mut engine = NotificationEngine::new();
        let now = Instant::now();
        assert_eq!(engine.wait_time_at(now), Duration::MAX);

        engine.last_word_from_server = now - Duration::from_secs(10);
        assert_eq!(engine.wait_time_at(now), Duration::from_secs(1));

        engine.last_word_from_server = now - Duration::from_secs(120);
        assert_eq!(engine.wait_time_at(now), ACK_INTERVAL);
    }
}
