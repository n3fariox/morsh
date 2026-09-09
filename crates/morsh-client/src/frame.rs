//! Painting the local screen: folding predictions into the server's state and
//! diffing the result against what the user is currently looking at.

use crate::ClientTransport;
use morsh_prediction::PredictionEngine;
use morsh_terminal::{DisplayDiff, ScreenSnapshot};
use std::io::{self, Write};

/// Overlay the prediction engine's speculative cells onto the frame we are
/// about to paint.
///
/// Mosh does the same thing (`OverlayManager::apply`) rather than drawing
/// predictions over the top of the finished frame: a prediction that is folded
/// into the frame participates in the next diff, so it is cleaned up
/// automatically when the server's real output supersedes it.
pub fn apply_overlay(frame: &mut ScreenSnapshot, prediction: &PredictionEngine) {
    if !prediction.should_display() {
        return;
    }

    let flagging = prediction.is_flagging();
    let late_acked = prediction.local_frame_late_acked();
    let (cols, rows) = (frame.cols as usize, frame.rows_count as usize);

    for row in prediction.overlay().rows() {
        if row.row_num >= rows {
            continue;
        }
        for cell in &row.cells {
            if !cell.active || cell.col >= cols {
                continue;
            }
            let pending = late_acked < cell.expiration_frame;
            let Some(target) = frame.cell_mut(cell.col as u16, row.row_num as u16) else {
                continue;
            };
            if cell.unknown {
                // An unknown prediction says "something will be here", not
                // "a blank will be here" — painting a space over it is what
                // made every keystroke erase the rest of the line.
                if flagging && pending && cell.col + 1 != cols {
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

    if let Some((row, col)) = prediction.cursor_pos() {
        if row < rows && col < cols {
            frame.cursor_y = row as u16;
            frame.cursor_x = col as u16;
        }
    }
}

/// Paint the difference between what is on the user's screen and what the
/// server's latest state (plus predictions) says should be there.
pub fn output_new_frame(
    transport: &ClientTransport,
    prediction: &PredictionEngine,
    local_framebuffer: &mut ScreenSnapshot,
    stdout: &mut io::Stdout,
) -> io::Result<()> {
    let mut target = transport.get_latest_remote_state().state.snapshot();
    apply_overlay(&mut target, prediction);

    let diff = DisplayDiff::diff(local_framebuffer, &target);
    if !diff.is_empty() {
        stdout.write_all(diff.as_bytes())?;
        stdout.flush()?;
    }
    *local_framebuffer = target;
    Ok(())
}
