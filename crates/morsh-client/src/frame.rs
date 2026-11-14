//! Painting the local screen: folding predictions into the server's state and
//! diffing the result against what the user is currently looking at.

use crate::ClientTransport;
use morsh_prediction::{NotificationEngine, PredictionEngine};
use morsh_terminal::{DisplayDiff, ScreenSnapshot};
use std::io::{self, Write};

/// What the user should be looking at: the server's screen with
/// predictions and the notification bar folded in. Mosh's
/// `OverlayManager::apply`. Built in `target`, whose storage is reused.
fn compose_frame(
    remote: &ScreenSnapshot,
    prediction: &mut PredictionEngine,
    notifications: &mut NotificationEngine,
    target: &mut ScreenSnapshot,
) {
    target.clone_from(remote);
    prediction.cull(remote);
    prediction.apply(target);
    notifications.adjust_message();
    notifications.apply(target);
}

/// Paint the difference between what is on the user's screen and what the
/// server's latest state (plus predictions and the notification bar) says
/// should be there. `scratch` is where the frame is built; keeping it from
/// frame to frame saves allocating a whole screen each time.
pub fn output_new_frame(
    transport: &ClientTransport,
    prediction: &mut PredictionEngine,
    notifications: &mut NotificationEngine,
    local_framebuffer: &mut ScreenSnapshot,
    scratch: &mut ScreenSnapshot,
    stdout: &mut io::Stdout,
) -> io::Result<()> {
    compose_frame(
        transport.get_latest_remote_state().state.screen(),
        prediction,
        notifications,
        scratch,
    );
    paint(local_framebuffer, scratch, stdout)
}

/// Bring the user's screen from `shown` to `target`. Afterwards `shown` is
/// the new frame and `target` holds the old one, for its storage.
fn paint(
    shown: &mut ScreenSnapshot,
    target: &mut ScreenSnapshot,
    out: &mut impl Write,
) -> io::Result<()> {
    // Most wakeups (timers, acks, the countup) change nothing on screen,
    // and comparing frames costs a tenth of diffing them.
    if shown == target {
        return Ok(());
    }
    let diff = DisplayDiff::diff(shown, target);
    if !diff.is_empty() {
        if shown.rows != target.rows {
            // Synchronized update (ignored where unsupported) with the cursor
            // hidden, so the terminal never shows a half-drawn frame or the
            // cursor hopping between changed cells.
            let mut buf = String::with_capacity(diff.len() + 32);
            buf.push_str("\x1b[?2026h\x1b[?25l");
            buf.push_str(&diff);
            if target.cursor_visible {
                buf.push_str("\x1b[?25h");
            }
            buf.push_str("\x1b[?2026l");
            out.write_all(buf.as_bytes())?;
        } else {
            out.write_all(diff.as_bytes())?;
        }
        out.flush()?;
    }
    std::mem::swap(shown, target);
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
        let mut shown = ScreenSnapshot::new(20, 3);
        remote.cursor_y = 1;

        // An earlier keystroke the server echoed promptly: predictions in
        // this epoch are no longer tentative.
        compose_frame(&remote, &mut prediction, &mut notifications, &mut shown);
        prediction.new_user_byte('a', &shown);
        prediction.set_local_frame_sent(1);
        prediction.set_local_frame_late_acked(1);
        remote.rows[1][0].text = "a".to_string();
        remote.cursor_x = 1;

        compose_frame(&remote, &mut prediction, &mut notifications, &mut shown);
        prediction.new_user_byte('x', &shown);
        compose_frame(&remote, &mut prediction, &mut notifications, &mut shown);
        assert_eq!(
            shown.cell(1, 1).unwrap().text,
            "",
            "shown before the glitch threshold"
        );

        std::thread::sleep(Duration::from_millis(260));
        compose_frame(&remote, &mut prediction, &mut notifications, &mut shown);
        assert_eq!(shown.cell(1, 1).unwrap().text, "x");
    }

    #[test]
    fn an_unchanged_frame_writes_nothing() {
        let mut shown = ScreenSnapshot::new(20, 3);
        let mut out = Vec::new();
        paint(&mut shown, &mut ScreenSnapshot::new(20, 3), &mut out).unwrap();
        assert!(out.is_empty());

        let mut target = ScreenSnapshot::new(20, 3);
        target.rows[1][2].text = "x".to_string();
        paint(&mut shown, &mut target.clone(), &mut out).unwrap();
        assert!(!out.is_empty());
        assert_eq!(shown, target);
    }
}
