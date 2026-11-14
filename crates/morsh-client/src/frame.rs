//! Painting the local screen: folding predictions into the server's state and
//! diffing the result against what the user is currently looking at.

use crate::ClientTransport;
use morsh_prediction::{NotificationEngine, PredictionEngine};
use morsh_terminal::{DisplayDiff, ScreenSnapshot};
use std::io::{self, Write};

/// Paint the difference between what is on the user's screen and what the
/// server's latest state (plus predictions and the notification bar) says
/// should be there.
pub fn output_new_frame(
    transport: &ClientTransport,
    prediction: &PredictionEngine,
    notifications: &mut NotificationEngine,
    local_framebuffer: &mut ScreenSnapshot,
    stdout: &mut io::Stdout,
) -> io::Result<()> {
    let mut target = transport.get_latest_remote_state().state.snapshot();
    prediction.apply(&mut target);
    notifications.adjust_message();
    notifications.apply(&mut target);

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
