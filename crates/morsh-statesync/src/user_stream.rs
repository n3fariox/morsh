use morsh_proto::client::{UserMessage, Instruction as ClientInstruction, Keystroke, ResizeMessage};
use prost::Message;

/// A user input event: keystroke or resize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserEvent {
    /// A single keystroke byte.
    UserByte(u8),
    /// Terminal resize.
    Resize { width: i32, height: i32 },
}

/// Queue of user input events (client side).
///
/// Provides diff_from() / apply_string() for the state synchronization protocol.
#[derive(Debug, Clone)]
pub struct UserStream {
    events: Vec<UserEvent>,
}

impl Default for UserStream {
    fn default() -> Self {
        Self::new()
    }
}

impl UserStream {
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    /// Push a keystroke event.
    pub fn push_key(&mut self, byte: u8) {
        self.events.push(UserEvent::UserByte(byte));
    }

    /// Push a resize event.
    pub fn push_resize(&mut self, width: i32, height: i32) {
        self.events.push(UserEvent::Resize { width, height });
    }

    /// Compute the diff from `existing` to `self`.
    /// Returns serialized ClientBuffers::UserMessage.
    pub fn diff_from(&self, existing: &UserStream) -> Vec<u8> {
        let mut msg = UserMessage { instruction: Vec::new() };

        // Find where existing ends and our new events begin
        let common_prefix_len = self.common_prefix_len(existing);
        let new_events = &self.events[common_prefix_len..];

        // Group consecutive keystrokes into a single instruction
        let mut i = 0;
        while i < new_events.len() {
            match &new_events[i] {
                UserEvent::UserByte(byte) => {
                    let mut keys = vec![*byte];
                    i += 1;
                    // Group consecutive keystrokes
                    while i < new_events.len() {
                        if let UserEvent::UserByte(b) = &new_events[i] {
                            keys.push(*b);
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    msg.instruction.push(ClientInstruction {
                        keystroke: Some(Keystroke { keys: Some(keys) }),
                        resize: None,
                    });
                }
                UserEvent::Resize { width, height } => {
                    msg.instruction.push(ClientInstruction {
                        keystroke: None,
                        resize: Some(ResizeMessage {
                            width: Some(*width),
                            height: Some(*height),
                        }),
                    });
                    i += 1;
                }
            }
        }

        let mut buf = Vec::with_capacity(msg.encoded_len());
        msg.encode(&mut buf).unwrap();
        buf
    }

    /// Apply a serialized diff to this stream.
    pub fn apply_string(&mut self, diff: &[u8]) {
        let msg = UserMessage::decode(diff)
            .expect("Failed to decode UserMessage");

        for inst in &msg.instruction {
            if let Some(ref keystroke) = inst.keystroke {
                if let Some(ref keys) = keystroke.keys {
                    for &byte in keys {
                        self.events.push(UserEvent::UserByte(byte));
                    }
                }
            }
            if let Some(ref resize) = inst.resize {
                let w = resize.width.unwrap_or(80);
                let h = resize.height.unwrap_or(24);
                self.events.push(UserEvent::Resize { width: w, height: h });
            }
        }
    }

    /// Remove the common prefix with `prefix`.
    pub fn subtract(&mut self, prefix: &UserStream) {
        let common = self.common_prefix_len(prefix);
        self.events.drain(..common);
    }

    /// Number of events in the stream.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the stream is empty.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Get an event by index.
    pub fn get(&self, idx: usize) -> Option<&UserEvent> {
        self.events.get(idx)
    }

    fn common_prefix_len(&self, other: &UserStream) -> usize {
        let mut i = 0;
        while i < self.events.len() && i < other.events.len() {
            if self.events[i] != other.events[i] {
                break;
            }
            i += 1;
        }
        i
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_length() {
        let mut s = UserStream::new();
        assert!(s.is_empty());
        s.push_key(b'a');
        s.push_key(b'b');
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn diff_from_empty() {
        let empty = UserStream::new();
        let mut s = UserStream::new();
        s.push_key(b'x');

        // s has more events than empty, so s.diff_from(&empty) gives the new events
        let diff = s.diff_from(&empty);
        assert!(!diff.is_empty());

        // Apply diff to empty stream
        let mut applied = UserStream::new();
        applied.apply_string(&diff);
        assert_eq!(applied.len(), 1);
        assert_eq!(applied.get(0), Some(&UserEvent::UserByte(b'x')));
    }

    #[test]
    fn diff_from_partial() {
        let mut a = UserStream::new();
        a.push_key(b'a');
        a.push_key(b'b');

        let mut b = UserStream::new();
        b.push_key(b'a');
        b.push_key(b'b');
        b.push_key(b'c');

        // b has more events, so b.diff_from(&a) gives the new event
        let diff = b.diff_from(&a);
        let mut result = a.clone();
        result.apply_string(&diff);
        assert_eq!(result.len(), 3);
        assert_eq!(result.get(2), Some(&UserEvent::UserByte(b'c')));
    }

    #[test]
    fn subtract_prefix() {
        let mut s = UserStream::new();
        s.push_key(b'a');
        s.push_key(b'b');
        s.push_key(b'c');

        let prefix = UserStream { events: vec![UserEvent::UserByte(b'a'), UserEvent::UserByte(b'b')] };
        s.subtract(&prefix);
        assert_eq!(s.len(), 1);
        assert_eq!(s.get(0), Some(&UserEvent::UserByte(b'c')));
    }

    #[test]
    fn keystrokes_grouped_in_diff() {
        let empty = UserStream::new();
        let mut s = UserStream::new();
        s.push_key(b'h');
        s.push_key(b'i');

        // s has new events compared to empty
        let diff = s.diff_from(&empty);
        let msg = UserMessage::decode(diff.as_slice()).unwrap();
        // Consecutive keystrokes should be in a single instruction
        assert_eq!(msg.instruction.len(), 1);
        let keys = msg.instruction[0].keystroke.as_ref().unwrap().keys.as_ref().unwrap();
        assert_eq!(keys, &vec![b'h', b'i']);
    }

    #[test]
    fn resize_event() {
        let empty = UserStream::new();
        let mut s = UserStream::new();
        s.push_resize(120, 40);

        // s has new events compared to empty
        let diff = s.diff_from(&empty);
        let mut result = UserStream::new();
        result.apply_string(&diff);
        assert_eq!(result.get(0), Some(&UserEvent::Resize { width: 120, height: 40 }));
    }
}
