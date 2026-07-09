use std::time::Instant;

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
    /// When the message expires.
    message_expiration: Option<Instant>,
    /// Whether to show the quit keystroke in the message.
    show_quit_keystroke: bool,
    /// The escape key description (e.g. "Ctrl-c .").
    escape_key_string: String,
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

    /// Show a notification message.
    pub fn set_notification(&mut self, msg: String, permanent: bool) {
        self.message = msg;
        self.message_is_network_error = false;
        if permanent {
            self.message_expiration = None;
        } else {
            self.message_expiration = Some(Instant::now() + std::time::Duration::from_secs(1));
        }
    }

    /// Show a network error message.
    pub fn set_network_error(&mut self, msg: String) {
        self.message = msg;
        self.message_is_network_error = true;
        self.message_expiration = Some(Instant::now() + std::time::Duration::from_millis(3100)); // ACK_INTERVAL + 100
    }

    /// Clear the network error if one is displayed.
    pub fn clear_network_error(&mut self) {
        if self.message_is_network_error {
            self.message_expiration = Some(
                self.message_expiration
                    .unwrap_or_else(Instant::now)
                    .min(Instant::now() + std::time::Duration::from_secs(1)),
            );
        }
    }

    /// Whether the server is late (no contact in >6.5s).
    pub fn server_late(&self) -> bool {
        self.last_word_from_server.elapsed().as_millis() > 6500
    }

    /// Whether our replies are late (no ack in >10s).
    pub fn reply_late(&self) -> bool {
        self.last_acked_state.elapsed().as_millis() > 10000
    }

    /// Whether we need to show a "waiting" countup.
    pub fn need_countup(&self) -> bool {
        self.server_late() || self.reply_late()
    }

    /// Get the elapsed seconds since last server contact.
    pub fn seconds_since_heard(&self) -> u64 {
        self.last_word_from_server.elapsed().as_secs()
    }

    /// Get the elapsed seconds since last ack.
    pub fn seconds_since_acked(&self) -> u64 {
        self.last_acked_state.elapsed().as_secs()
    }

    /// Whether there's a message to display.
    /// Only shows for explicit messages or sustained connection problems.
    pub fn has_message(&self) -> bool {
        if !self.message.is_empty() {
            return true;
        }
        // Only show countup banner if server has been late for a while (>10s)
        // to avoid flickering between packets
        self.server_late() && self.seconds_since_heard() > 10
    }

    /// Get the notification text to render.
    pub fn get_text(&self) -> String {
        let time_expired = self.need_countup();

        if self.message.is_empty() && !time_expired {
            return String::new();
        }

        let keystroke_suffix = if self.show_quit_keystroke && !self.escape_key_string.is_empty() {
            format!(" [To quit: {}]", self.escape_key_string)
        } else {
            String::new()
        };

        if self.message.is_empty() && time_expired {
            let (elapsed, explanation) = if self.reply_late() && !self.server_late() {
                (self.seconds_since_acked(), "reply")
            } else {
                (self.seconds_since_heard(), "contact")
            };
            format!(
                "mosh: Last {} {} ago.{}",
                explanation, elapsed, keystroke_suffix
            )
        } else if !self.message.is_empty() && !time_expired {
            format!("mosh: {}{}", self.message, keystroke_suffix)
        } else {
            let (elapsed, explanation) = if self.reply_late() && !self.server_late() {
                (self.seconds_since_acked(), "reply")
            } else {
                (self.seconds_since_heard(), "contact")
            };
            format!(
                "mosh: {} ({}s without {}.){}",
                self.message, elapsed, explanation, keystroke_suffix
            )
        }
    }

    /// Render the notification bar as VT escape sequences.
    ///
    /// Draws a blue bar across the top row of the terminal with white text.
    /// Returns the VT string to write to the terminal.
    pub fn render(&self) -> Option<String> {
        if !self.has_message() {
            return None;
        }

        let text = self.get_text();
        let width = 80; // TODO: pass actual terminal width

        // Save cursor, move to 1;1, set reverse video, fill row, write text, restore cursor
        let mut vt = String::new();
        vt.push_str("\x1b[s"); // Save cursor
        vt.push_str("\x1b[1;1H"); // Move to row 1, col 1
        vt.push_str("\x1b[7m"); // Reverse video (white on blue)

        // Fill the row with spaces
        for _ in 0..width {
            vt.push(' ');
        }

        // Go back and write the text
        vt.push_str(&format!("\x1b[1;1H"));
        // Truncate text to fit width
        let display_text: String = text.chars().take(width - 1).collect();
        vt.push_str(&display_text);

        vt.push_str("\x1b[0m"); // Reset attributes
        vt.push_str("\x1b[u"); // Restore cursor

        Some(vt)
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
    use std::time::Duration;

    #[test]
    fn notification_basic() {
        let mut engine = NotificationEngine::new();
        assert!(!engine.has_message());

        engine.set_notification("Connected".to_string(), false);
        assert!(engine.has_message());
        assert!(!engine.need_countup());
    }

    #[test]
    fn notification_server_late() {
        let mut engine = NotificationEngine::new();

        // 3 seconds: not late yet
        engine.last_word_from_server = Instant::now() - Duration::from_secs(3);
        assert!(!engine.server_late());
        assert!(!engine.has_message());

        // 8 seconds: late but <10s threshold → no banner
        engine.last_word_from_server = Instant::now() - Duration::from_secs(8);
        assert!(engine.server_late());
        assert!(!engine.has_message()); // suppressed to avoid flicker

        // 12 seconds: late AND >10s → banner shows
        engine.last_word_from_server = Instant::now() - Duration::from_secs(12);
        assert!(engine.has_message());
    }

    #[test]
    fn notification_reply_late() {
        let mut engine = NotificationEngine::new();
        engine.last_acked_state = Instant::now() - Duration::from_secs(15);

        assert!(engine.reply_late());
        assert!(engine.need_countup());
    }

    #[test]
    fn notification_text_generation() {
        let mut engine = NotificationEngine::new();
        engine.last_word_from_server = Instant::now() - Duration::from_secs(30);

        let text = engine.get_text();
        assert!(text.contains("contact"));
        assert!(text.contains("30"));
    }

    #[test]
    fn notification_render() {
        let mut engine = NotificationEngine::new();
        assert!(engine.render().is_none());

        engine.set_notification("Test".to_string(), true);
        let vt = engine.render().unwrap();
        assert!(vt.contains("\x1b[7m")); // Reverse video
        assert!(vt.contains("Test"));
    }

    #[test]
    fn notification_permanent() {
        let mut engine = NotificationEngine::new();
        engine.set_notification("Perm".to_string(), true);

        // Permanent messages don't have an expiration
        assert!(engine.message_expiration.is_none());
        assert!(engine.has_message());
    }
}
