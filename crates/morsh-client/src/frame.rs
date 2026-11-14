//! Painting the local screen: folding predictions into the server's state and
//! diffing the result against what the user is currently looking at.

use crate::ClientTransport;
use morsh_prediction::{NotificationEngine, PredictionEngine};
use morsh_terminal::{DisplayDiff, ScreenSnapshot};
use std::io::{self, Write};

/// What the user should be looking at: the server's screen with
/// predictions and the notification bar folded in. Mosh's
/// `OverlayManager::apply`.
fn compose_frame(
    remote: &ScreenSnapshot,
    prediction: &mut PredictionEngine,
    notifications: &mut NotificationEngine,
) -> ScreenSnapshot {
    let mut target = remote.clone();
    prediction.cull(remote);
    prediction.apply(&mut target);
    notifications.adjust_message();
    notifications.apply(&mut target);
    target
}

/// Paint the difference between what is on the user's screen and what the
/// server's latest state (plus predictions and the notification bar) says
/// should be there.
pub fn output_new_frame(
    transport: &ClientTransport,
    prediction: &mut PredictionEngine,
    notifications: &mut NotificationEngine,
    local_framebuffer: &mut ScreenSnapshot,
    stdout: &mut io::Stdout,
) -> io::Result<()> {
    let target = compose_frame(
        transport.get_latest_remote_state().state.screen(),
        prediction,
        notifications,
    );

    let diff = DisplayDiff::diff(local_framebuffer, &target);
    if !diff.is_empty() {
        if local_framebuffer.rows != target.rows {
            // Synchronized update (ignored where unsupported) with the cursor
            // hidden, so the terminal never shows a half-drawn frame or the
            // cursor hopping between changed cells.
            let mut out = String::with_capacity(diff.len() + 32);
            out.push_str("\x1b[?2026h\x1b[?25l");
            out.push_str(&diff);
            if target.cursor_visible {
                out.push_str("\x1b[?25h");
            }
            out.push_str("\x1b[?2026l");
            stdout.write_all(out.as_bytes())?;
        } else {
            stdout.write_all(diff.as_bytes())?;
        }
        stdout.flush()?;
    }
    *local_framebuffer = target;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use morsh_prediction::DisplayPreference;
    use std::time::Duration;

    /// When the network stalls, a keystroke the server hasn't echoed for a
    /// quarter of a second is shown anyway (mosh's glitch trigger), even
    /// though the round trip is normally too short to bother predicting.
    /// Nothing arrives from the server meanwhile, so it has to happen as
    /// frames are drawn, not when packets come in.
    #[test]
    fn a_stalled_keystroke_is_shown_without_word_from_the_server() {
        let mut prediction = PredictionEngine::new();
        prediction.set_display_preference(DisplayPreference::Adaptive);
        prediction.set_send_interval(20);
        let mut notifications = NotificationEngine::new();
        let mut remote = ScreenSnapshot::new(20, 3);
        remote.cursor_y = 1;

        // An earlier keystroke the server echoed promptly: predictions in
        // this epoch are no longer tentative.
        let shown = compose_frame(&remote, &mut prediction, &mut notifications);
        prediction.new_user_byte('a', &shown);
        prediction.set_local_frame_sent(1);
        prediction.set_local_frame_late_acked(1);
        remote.rows[1][0].text = "a".to_string();
        remote.cursor_x = 1;

        let shown = compose_frame(&remote, &mut prediction, &mut notifications);
        prediction.new_user_byte('x', &shown);
        let shown = compose_frame(&remote, &mut prediction, &mut notifications);
        assert_eq!(
            shown.cell(1, 1).unwrap().text,
            "",
            "shown before the glitch threshold"
        );

        std::thread::sleep(Duration::from_millis(260));
        let shown = compose_frame(&remote, &mut prediction, &mut notifications);
        assert_eq!(shown.cell(1, 1).unwrap().text, "x");
    }
}
