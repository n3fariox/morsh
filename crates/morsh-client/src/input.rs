//! Keyboard and resize handling: turning crossterm events into the bytes the
//! remote pty should see, and feeding them to the transport and the predictor.

use crate::ClientTransport;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use morsh_prediction::{NotificationEngine, PredictionEngine};
use morsh_terminal::ScreenSnapshot;
use tokio::sync::mpsc;

/// Ctrl-^ (Ctrl-Shift-6) — the stock mosh escape key.
pub const ESCAPE_KEY: u8 = 0x1E;

/// Bulk input this long is treated as a paste and not predicted.
const PASTE_THRESHOLD: usize = 100;

pub enum TermEvent {
    Key(Vec<u8>),
    Resize(i32, i32),
    Quit,
}

/// What the escape filter decided to do with a chunk of input.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Swallowed — either arming the escape or an escape command with no bytes.
    None,
    Send(Vec<u8>),
    Quit,
}

/// Recognizes the two-keystroke escape sequence in the input stream.
///
/// `ESCAPE_KEY` followed by `.` quits; followed by `ESCAPE_KEY` again it sends
/// one literal `0x1E`; followed by anything else it sends the escape and that
/// input through unchanged.
#[derive(Default)]
pub struct EscapeFilter {
    armed: bool,
}

impl EscapeFilter {
    pub fn feed(&mut self, bytes: Vec<u8>) -> Action {
        if self.armed {
            self.armed = false;
            if bytes.len() == 1 {
                return match bytes[0] {
                    b'.' => Action::Quit,
                    // Ctrl-Z: suspend, not yet implemented. Swallowed rather
                    // than passed through so it does not reach the remote
                    // shell and stop the wrong process.
                    0x1A => Action::None,
                    ESCAPE_KEY => Action::Send(vec![ESCAPE_KEY]),
                    other => Action::Send(vec![ESCAPE_KEY, other]),
                };
            }
            let mut both = vec![ESCAPE_KEY];
            both.extend(bytes);
            return Action::Send(both);
        }

        if bytes.len() == 1 && bytes[0] == ESCAPE_KEY {
            self.armed = true;
            return Action::None;
        }
        Action::Send(bytes)
    }
}

/// The bytes a terminal sends for one key press.
///
/// Returns an empty vector for keys morsh does not forward.
pub fn encode_key(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
    let mut bytes = Vec::new();
    match code {
        KeyCode::Char(ch) => {
            if modifiers.contains(KeyModifiers::CONTROL) && ch.is_ascii_lowercase() {
                bytes.push((ch as u8) - b'a' + 1);
            } else if modifiers.contains(KeyModifiers::ALT) {
                bytes.push(0x1b);
                bytes.push(ch as u8);
            } else {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
        }
        KeyCode::Enter => bytes.push(b'\r'),
        KeyCode::Backspace => bytes.push(0x7f),
        KeyCode::Tab => bytes.push(b'\t'),
        KeyCode::Esc => bytes.push(0x1b),
        KeyCode::Up => bytes.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => bytes.extend_from_slice(b"\x1b[B"),
        KeyCode::Right => bytes.extend_from_slice(b"\x1b[C"),
        KeyCode::Left => bytes.extend_from_slice(b"\x1b[D"),
        KeyCode::Home => bytes.extend_from_slice(b"\x1b[H"),
        KeyCode::End => bytes.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => bytes.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => bytes.extend_from_slice(b"\x1b[6~"),
        KeyCode::Insert => bytes.extend_from_slice(b"\x1b[2~"),
        KeyCode::Delete => bytes.extend_from_slice(b"\x1b[3~"),
        KeyCode::F(n) => match n {
            1 => bytes.extend_from_slice(b"\x1bOP"),
            2 => bytes.extend_from_slice(b"\x1bOQ"),
            3 => bytes.extend_from_slice(b"\x1bOR"),
            4 => bytes.extend_from_slice(b"\x1bOS"),
            5 => bytes.extend_from_slice(b"\x1b[15~"),
            6 => bytes.extend_from_slice(b"\x1b[17~"),
            7 => bytes.extend_from_slice(b"\x1b[18~"),
            8 => bytes.extend_from_slice(b"\x1b[19~"),
            9 => bytes.extend_from_slice(b"\x1b[20~"),
            10 => bytes.extend_from_slice(b"\x1b[21~"),
            11 => bytes.extend_from_slice(b"\x1b[23~"),
            12 => bytes.extend_from_slice(b"\x1b[24~"),
            _ => {}
        },
        _ => {}
    }
    bytes
}

/// Read the local terminal on its own task and publish [`TermEvent`]s.
pub fn spawn_input_reader(term_tx: mpsc::Sender<TermEvent>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut events = EventStream::new();
        let mut escape = EscapeFilter::default();
        while let Some(Ok(event)) = events.next().await {
            match event {
                Event::Key(KeyEvent {
                    code,
                    modifiers,
                    kind: KeyEventKind::Press,
                    ..
                }) => {
                    let bytes = encode_key(code, modifiers);
                    if bytes.is_empty() {
                        continue;
                    }
                    match escape.feed(bytes) {
                        Action::None => {}
                        Action::Send(bytes) => {
                            let _ = term_tx.try_send(TermEvent::Key(bytes));
                        }
                        Action::Quit => {
                            // Keep reading afterwards: the main loop may still
                            // be waiting for the server to acknowledge the
                            // shutdown, and it is the one that decides to exit.
                            let _ = term_tx.try_send(TermEvent::Quit);
                        }
                    }
                }
                Event::Resize(w, h) => {
                    let _ = term_tx.send(TermEvent::Resize(w as i32, h as i32)).await;
                }
                _ => {}
            }
        }
    })
}

/// Apply one [`TermEvent`] to the transport and the predictor.
///
/// Returns true if the client should exit immediately.
pub fn handle_term_event(
    event: TermEvent,
    transport: &mut ClientTransport,
    prediction: &mut PredictionEngine,
    local_framebuffer: &ScreenSnapshot,
    notifications: &mut NotificationEngine,
) -> bool {
    if transport.shutdown_in_progress() {
        return false;
    }

    match event {
        TermEvent::Key(bytes) => {
            // Don't predict for bulk data (a paste).
            let paste = bytes.len() > PASTE_THRESHOLD;
            if paste {
                prediction.reset();
            }
            prediction.set_local_frame_sent(transport.sender.sent_state_last());

            let (cols, rows) = (
                local_framebuffer.cols as usize,
                local_framebuffer.rows_count as usize,
            );

            for byte in bytes {
                if !paste {
                    let (cur_row, cur_col) = prediction.cursor_pos().unwrap_or((
                        local_framebuffer.cursor_y as usize,
                        local_framebuffer.cursor_x as usize,
                    ));
                    prediction.new_user_byte(byte as char, cur_row, cur_col, cols, rows, |r, c| {
                        local_framebuffer
                            .cell(c as u16, r as u16)
                            .and_then(|d| d.text.chars().next())
                            .unwrap_or(' ')
                    });
                }
                transport.sender.current_state_mut().push_key(byte);
            }
            false
        }
        TermEvent::Resize(w, h) => {
            transport.sender.current_state_mut().push_resize(w, h);
            prediction.reset();
            tracing::info!("Resize: {w}x{h}");
            false
        }
        TermEvent::Quit => {
            if transport.has_remote_addr() {
                notifications.set_notification("Exiting on user request...".to_string(), true);
                transport.start_shutdown();
                false
            } else {
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_keys_pass_straight_through() {
        let mut escape = EscapeFilter::default();
        assert_eq!(escape.feed(b"a".to_vec()), Action::Send(b"a".to_vec()));
        assert_eq!(
            escape.feed(b"\x1b[A".to_vec()),
            Action::Send(b"\x1b[A".to_vec())
        );
    }

    #[test]
    fn escape_then_dot_quits() {
        let mut escape = EscapeFilter::default();
        assert_eq!(escape.feed(vec![ESCAPE_KEY]), Action::None);
        assert_eq!(escape.feed(b".".to_vec()), Action::Quit);
    }

    #[test]
    fn escape_twice_sends_one_literal_escape() {
        let mut escape = EscapeFilter::default();
        assert_eq!(escape.feed(vec![ESCAPE_KEY]), Action::None);
        assert_eq!(
            escape.feed(vec![ESCAPE_KEY]),
            Action::Send(vec![ESCAPE_KEY])
        );
        // And the filter is disarmed again, so the next '.' is just a period.
        assert_eq!(escape.feed(b".".to_vec()), Action::Send(b".".to_vec()));
    }

    #[test]
    fn escape_then_another_key_sends_both() {
        let mut escape = EscapeFilter::default();
        escape.feed(vec![ESCAPE_KEY]);
        assert_eq!(
            escape.feed(b"x".to_vec()),
            Action::Send(vec![ESCAPE_KEY, b'x'])
        );

        escape.feed(vec![ESCAPE_KEY]);
        assert_eq!(
            escape.feed(b"\x1b[A".to_vec()),
            Action::Send(vec![ESCAPE_KEY, 0x1b, b'[', b'A'])
        );
    }

    #[test]
    fn escape_then_ctrl_z_is_swallowed() {
        let mut escape = EscapeFilter::default();
        escape.feed(vec![ESCAPE_KEY]);
        assert_eq!(escape.feed(vec![0x1A]), Action::None);
        assert_eq!(escape.feed(b".".to_vec()), Action::Send(b".".to_vec()));
    }

    #[test]
    fn control_and_alt_chars_are_encoded_like_a_terminal() {
        assert_eq!(
            encode_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            vec![0x03]
        );
        assert_eq!(
            encode_key(KeyCode::Char('x'), KeyModifiers::ALT),
            vec![0x1b, b'x']
        );
        assert_eq!(
            encode_key(KeyCode::Char('é'), KeyModifiers::NONE),
            "é".as_bytes().to_vec()
        );
    }

    #[test]
    fn named_keys_are_encoded() {
        assert_eq!(encode_key(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(encode_key(KeyCode::Backspace, KeyModifiers::NONE), [0x7f]);
        assert_eq!(encode_key(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(encode_key(KeyCode::F(5), KeyModifiers::NONE), b"\x1b[15~");
    }

    #[test]
    fn unforwarded_keys_encode_to_nothing() {
        assert!(encode_key(KeyCode::F(20), KeyModifiers::NONE).is_empty());
        assert!(encode_key(KeyCode::CapsLock, KeyModifiers::NONE).is_empty());
    }
}
