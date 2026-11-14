//! Query recognition follows mosh 1.4's parserstate.cc and terminalfunctions.cc.
//! Ghostty's effects support more queries and different device attributes, so
//! enabling its PTY-write callback would advertise capabilities mosh lacks.

#[derive(Clone, Copy)]
pub(crate) enum Query {
    PrimaryAttributes,
    SecondaryAttributes,
    Status,
    Cursor,
}

#[derive(Clone, Copy, Default)]
enum State {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    String {
        bell_terminated: bool,
    },
}

#[derive(Default)]
pub(crate) struct QueryParser {
    state: State,
    // Only plain CSI and CSI > can name one of mosh's queries.
    prefix: Option<u8>,
    first_param: u32,
    param_done: bool,
    param_bytes: u8,
    utf8: [u8; 4],
    utf8_len: usize,
    pending_c2: bool,
}

impl QueryParser {
    /// Ghostty does not interpret UTF-8 C1 controls. Translate them to their
    /// 7-bit equivalents, retaining a trailing UTF-8 lead byte across writes.
    pub fn normalize<'a>(&mut self, input: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        use std::borrow::Cow;
        if !self.pending_c2 && !input.contains(&0xc2) {
            return Cow::Borrowed(input);
        }
        let mut out = Vec::with_capacity(input.len() + 1);
        for &byte in input {
            if self.pending_c2 {
                self.pending_c2 = false;
                if (0x80..=0x9f).contains(&byte) {
                    let alias = match byte {
                        0x84 => Some(b'D'),
                        0x85 => Some(b'E'),
                        0x88 => Some(b'H'),
                        0x8d => Some(b'M'),
                        0x90 => Some(b'P'),
                        0x98 => Some(b'X'),
                        0x9b => Some(b'['),
                        0x9c => Some(b'\\'),
                        0x9d => Some(b']'),
                        0x9e => Some(b'^'),
                        0x9f => Some(b'_'),
                        _ => None,
                    };
                    if let Some(alias) = alias {
                        out.extend_from_slice(&[0x1b, alias]);
                    } else {
                        // Other C1 controls cancel the sequence but mosh has
                        // no registered action for them. CAN does the same.
                        out.push(0x18);
                    }
                    continue;
                }
                out.push(0xc2);
            }
            if byte == 0xc2 {
                self.pending_c2 = true;
            } else {
                out.push(byte);
            }
        }
        Cow::Owned(out)
    }

    pub fn next_query(&mut self, input: &[u8]) -> Option<(usize, Query)> {
        let mut index = 0;
        while index < input.len() {
            if matches!(self.state, State::Ground) && self.utf8_len == 0 {
                // Plain text cannot query the terminal. Skip whole runs while
                // preserving ESC and the UTF-8 lead byte for C1 controls.
                index += input[index..]
                    .iter()
                    .position(|&b| b == 0x1b || b == 0xc2)?;
            }
            if let Some(query) = self.advance(input[index]) {
                return Some((index + 1, query));
            }
            index += 1;
        }
        None
    }

    pub fn advance(&mut self, byte: u8) -> Option<Query> {
        if self.utf8_len == 0 && byte < 0x80 {
            return self.character(byte as u32);
        }
        self.utf8[self.utf8_len] = byte;
        self.utf8_len += 1;
        match std::str::from_utf8(&self.utf8[..self.utf8_len]) {
            Ok(text) => {
                let ch = text.chars().next().unwrap() as u32;
                self.utf8_len = 0;
                self.character(ch)
            }
            Err(error) if error.error_len().is_none() => None,
            Err(_) => {
                let retry = self.utf8_len > 1;
                self.utf8_len = 0;
                self.character(0xfffd);
                // Like mosh's UTF8Parser, retry the last byte after an invalid
                // multibyte sequence (it may start an escape sequence).
                if retry {
                    self.advance(byte)
                } else {
                    None
                }
            }
        }
    }

    fn start_csi(&mut self) {
        self.state = State::CsiEntry;
        self.prefix = None;
        self.first_param = 0;
        self.param_done = false;
        self.param_bytes = 0;
    }

    fn parameter(&mut self, ch: u8) {
        // Mosh keeps at most 100 parameter bytes and treats values >65535 as
        // absent. Only the first parameter affects a device status request.
        if self.param_bytes == 100 {
            return;
        }
        self.param_bytes += 1;
        if ch == b';' {
            self.param_done = true;
        } else if !self.param_done {
            self.first_param = (self.first_param * 10 + (ch - b'0') as u32).min(65536);
        }
    }

    fn dispatch(&mut self, ch: u8) -> Option<Query> {
        self.state = State::Ground;
        match (self.prefix, ch) {
            (None, b'c') => Some(Query::PrimaryAttributes),
            (Some(b'>'), b'c') => Some(Query::SecondaryAttributes),
            (None, b'n') if self.first_param == 5 => Some(Query::Status),
            (None, b'n') if self.first_param == 6 => Some(Query::Cursor),
            _ => None,
        }
    }

    fn character(&mut self, ch: u32) -> Option<Query> {
        // These transitions apply in every parser state, including strings.
        match ch {
            0x18 | 0x1a | 0x80..=0x8f | 0x91..=0x97 | 0x99 | 0x9a | 0x9c => {
                self.state = State::Ground;
                return None;
            }
            0x1b => {
                self.state = State::Escape;
                return None;
            }
            0x90 | 0x98 | 0x9e | 0x9f => {
                self.state = State::String {
                    bell_terminated: false,
                };
                return None;
            }
            0x9d => {
                self.state = State::String {
                    bell_terminated: true,
                };
                return None;
            }
            0x9b => {
                self.start_csi();
                return None;
            }
            _ => {}
        }
        // Mosh classifies high Unicode codepoints as 'A' for transitions.
        // None can be a query final byte or a parameter.
        let ch = if ch >= 0xa0 { b'A' } else { ch as u8 };
        match self.state {
            State::Ground => {}
            State::Escape => match ch {
                b'[' => self.start_csi(),
                b']' => {
                    self.state = State::String {
                        bell_terminated: true,
                    }
                }
                b'P' | b'X' | b'^' | b'_' => {
                    self.state = State::String {
                        bell_terminated: false,
                    }
                }
                0x20..=0x2f => self.state = State::EscapeIntermediate,
                0x30..=0x7e => self.state = State::Ground,
                _ => {}
            },
            State::EscapeIntermediate => {
                if (0x30..=0x7e).contains(&ch) {
                    self.state = State::Ground;
                }
            }
            State::CsiEntry | State::CsiParam => match ch {
                b'0'..=b'9' | b';' => {
                    self.parameter(ch);
                    self.state = State::CsiParam;
                }
                0x3c..=0x3f if matches!(self.state, State::CsiEntry) => {
                    self.prefix = Some(ch);
                    self.state = State::CsiParam;
                }
                b':' | 0x3c..=0x3f => self.state = State::CsiIgnore,
                0x20..=0x2f => self.state = State::CsiIntermediate,
                0x40..=0x7e => return self.dispatch(ch),
                _ => {}
            },
            State::CsiIntermediate => match ch {
                0x30..=0x3f => self.state = State::CsiIgnore,
                0x40..=0x7e => self.state = State::Ground,
                _ => {}
            },
            State::CsiIgnore => {
                if (0x40..=0x7e).contains(&ch) {
                    self.state = State::Ground;
                }
            }
            State::String { bell_terminated } => {
                if bell_terminated && ch == 7 {
                    self.state = State::Ground;
                }
            }
        }
        None
    }
}
