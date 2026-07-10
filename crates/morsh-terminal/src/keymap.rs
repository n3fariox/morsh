use libghostty_vt::key::{Action, Encoder, Event, Key, Mods};

/// Key encoder that translates raw key events to terminal escape sequences.
pub struct KeyMap {
    encoder: Encoder<'static>,
}

impl KeyMap {
    pub fn new() -> Result<Self, String> {
        let encoder = Encoder::new().map_err(|e| format!("Failed to create key encoder: {e:?}"))?;
        Ok(Self { encoder })
    }

    /// Encode a character key press (the common case).
    pub fn encode_char(&mut self, ch: char) -> Vec<u8> {
        let mut event = Event::new().expect("Failed to create key event");
        event.set_key(Key::Unidentified);
        event.set_action(Action::Press);
        event.set_utf8(Some(ch.to_string()));

        let mut buf = Vec::with_capacity(8);
        let _ = self.encoder.encode_to_vec(&event, &mut buf);
        buf
    }

    /// Encode an Enter key.
    pub fn encode_enter(&mut self) -> Vec<u8> {
        let mut event = Event::new().expect("Failed to create key event");
        event.set_key(Key::Enter);
        event.set_action(Action::Press);

        let mut buf = Vec::with_capacity(4);
        let _ = self.encoder.encode_to_vec(&event, &mut buf);
        buf
    }

    /// Encode a Backspace key.
    pub fn encode_backspace(&mut self) -> Vec<u8> {
        let mut event = Event::new().expect("Failed to create key event");
        event.set_key(Key::Backspace);
        event.set_action(Action::Press);

        let mut buf = Vec::with_capacity(4);
        let _ = self.encoder.encode_to_vec(&event, &mut buf);
        buf
    }

    /// Encode an arrow key.
    pub fn encode_arrow(&mut self, direction: Arrow) -> Vec<u8> {
        let key = match direction {
            Arrow::Up => Key::ArrowUp,
            Arrow::Down => Key::ArrowDown,
            Arrow::Left => Key::ArrowLeft,
            Arrow::Right => Key::ArrowRight,
        };
        let mut event = Event::new().expect("Failed to create key event");
        event.set_key(key);
        event.set_action(Action::Press);

        let mut buf = Vec::with_capacity(8);
        let _ = self.encoder.encode_to_vec(&event, &mut buf);
        buf
    }

    /// Encode a key with modifiers.
    pub fn encode_with_mods(&mut self, key: Key, mods: Mods) -> Vec<u8> {
        let mut event = Event::new().expect("Failed to create key event");
        event.set_key(key);
        event.set_action(Action::Press);
        event.set_mods(mods);

        let mut buf = Vec::with_capacity(16);
        let _ = self.encoder.encode_to_vec(&event, &mut buf);
        buf
    }

    /// Access the underlying encoder.
    pub fn inner(&self) -> &Encoder<'static> {
        &self.encoder
    }

    /// Access the underlying encoder mutably.
    pub fn inner_mut(&mut self) -> &mut Encoder<'static> {
        &mut self.encoder
    }
}

/// Arrow key directions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrow {
    Up,
    Down,
    Left,
    Right,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_keymap() {
        let km = KeyMap::new();
        assert!(km.is_ok());
    }

    #[test]
    fn encode_char_key() {
        let mut km = KeyMap::new().unwrap();
        let seq = km.encode_char('a');
        assert!(!seq.is_empty());
    }

    #[test]
    fn encode_enter_key() {
        let mut km = KeyMap::new().unwrap();
        let seq = km.encode_enter();
        assert!(!seq.is_empty());
    }

    #[test]
    fn encode_backspace_key() {
        let mut km = KeyMap::new().unwrap();
        let seq = km.encode_backspace();
        assert!(!seq.is_empty());
    }

    #[test]
    fn encode_arrow_key() {
        let mut km = KeyMap::new().unwrap();
        let seq = km.encode_arrow(Arrow::Up);
        assert!(!seq.is_empty());
    }

    #[test]
    fn encode_with_mods() {
        let mut km = KeyMap::new().unwrap();
        let seq = km.encode_with_mods(Key::C, Mods::CTRL);
        assert!(!seq.is_empty());
    }
}
