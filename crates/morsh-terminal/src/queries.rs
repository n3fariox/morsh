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
    /// Unimplemented C1 controls and CAN/SUB cancel through ST without printing.
    /// A true prefix flag requests a separate 0xC2 write before the returned data.
    pub fn normalize<'a>(&mut self, input: &'a [u8]) -> (bool, std::borrow::Cow<'a, [u8]>) {
        use std::borrow::Cow;
        if input.is_empty() {
            return (false, Cow::Borrowed(input));
        }
        // Flush a withheld plain-text lead byte separately instead of copying
        // the next chunk just to prepend it. Only actual controls need storage.
        let plain_prefix = self.pending_c2 && !(0x80..=0x9f).contains(&input[0]);
        if plain_prefix {
            self.pending_c2 = false;
        }
        if !self.pending_c2 && memchr::memchr3(0xc2, 0x18, 0x1a, input).is_none() {
            return (plain_prefix, Cow::Borrowed(input));
        }
        if !self.pending_c2
            && input.last() != Some(&0xc2)
            && memchr::memchr2(0x18, 0x1a, input).is_none()
            && !contains_c1(input)
        {
            return (plain_prefix, Cow::Borrowed(input));
        }
        let mut out = None;
        let mut start = 0;
        if self.pending_c2 {
            self.pending_c2 = false;
            Self::c1_alias(out.insert(Vec::with_capacity(input.len() + 1)), input[0]);
            start = 1;
        }
        // Retain only the final lead byte, borrowing the preceding text when
        // there is nothing else to rewrite. Ordinary Latin-1 stays borrowed.
        let end = if input.last() == Some(&0xc2) && start < input.len() {
            self.pending_c2 = true;
            input.len() - 1
        } else {
            input.len()
        };
        let scan_start = start;
        for pos in
            memchr::memchr3_iter(0xc2, 0x18, 0x1a, &input[scan_start..end]).map(|i| scan_start + i)
        {
            if input[pos] == 0x18 || input[pos] == 0x1a {
                let buffer = out.get_or_insert_with(|| Vec::with_capacity(input.len() + 1));
                buffer.extend_from_slice(&input[start..pos]);
                // Ghostty prints CAN/SUB. ST cancels the sequence without
                // leaving a control-code cell or moving the cursor.
                buffer.extend_from_slice(b"\x1b\\");
                start = pos + 1;
            } else if pos + 1 < end && (0x80..=0x9f).contains(&input[pos + 1]) {
                let buffer = out.get_or_insert_with(|| Vec::with_capacity(input.len() + 1));
                buffer.extend_from_slice(&input[start..pos]);
                Self::c1_alias(buffer, input[pos + 1]);
                start = pos + 2;
            }
        }
        let data = match out {
            Some(mut buffer) => {
                buffer.extend_from_slice(&input[start..end]);
                Cow::Owned(buffer)
            }
            None => Cow::Borrowed(&input[..end]),
        };
        (plain_prefix, data)
    }

    fn c1_alias(out: &mut Vec<u8>, byte: u8) {
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
            out.extend_from_slice(b"\x1b\\");
        }
    }

    pub fn next_query(&mut self, input: &[u8]) -> Option<(usize, Query)> {
        let mut index = 0;
        if memchr::memchr2(b'c', b'n', input).is_none() {
            // No stock query can finish in this chunk. ESC resets the parser
            // from every state, so only the suffix after its last occurrence
            // can determine the state carried to the next chunk.
            if let Some(last_escape) = memchr::memrchr(0x1b, input) {
                self.state = State::Escape;
                self.utf8_len = 0;
                index = last_escape + 1;
            }
        }
        while index < input.len() {
            if self.utf8_len == 0 {
                match self.state {
                    State::Ground => {
                        // C1 controls were normalized above, so ordinary UTF-8
                        // text needs no decoding here, including Latin-1.
                        index += memchr::memchr(0x1b, &input[index..])?;
                        if input[index..].starts_with(b"\x1b[") {
                            self.start_csi();
                            index += 2;
                            continue;
                        }
                    }
                    State::CsiEntry | State::CsiParam => {
                        // DA ignores parameters; DSR only uses the first. Once
                        // it cannot become 5/6, do not parse the remaining digits.
                        if matches!(self.state, State::CsiParam)
                            && (self.prefix.is_some()
                                || self.param_done
                                || !matches!(self.first_param, 0 | 5 | 6))
                        {
                            index += input[index..]
                                .iter()
                                .position(|&b| !b.is_ascii_digit() && b != b';')?;
                        }
                        let byte = input[index];
                        if byte.is_ascii_digit() || byte == b';' {
                            self.parameter(byte);
                            self.state = State::CsiParam;
                            index += 1;
                            continue;
                        }
                        if (0x40..=0x7e).contains(&byte) {
                            index += 1;
                            if let Some(query) = self.dispatch(byte) {
                                return Some((index, query));
                            }
                            continue;
                        }
                    }
                    State::CsiIntermediate | State::CsiIgnore => {
                        index += input[index..].iter().position(|&b| {
                            (0x40..=0x7e).contains(&b)
                                || b == 0x1b
                                || b == 0x18
                                || b == 0x1a
                                || b >= 0x80
                        })?;
                    }
                    State::String { bell_terminated } => {
                        index += if bell_terminated {
                            memchr::memchr2(0x1b, 7, &input[index..])?
                        } else {
                            memchr::memchr(0x1b, &input[index..])?
                        };
                    }
                    State::Escape | State::EscapeIntermediate => {}
                }
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

// Check eight bytes at once for the C1 continuation range 0x80..=0x9f.
// This avoids one memchr call per Latin-1 character when none are controls.
fn contains_c1(input: &[u8]) -> bool {
    let (blocks, remainder) = input.as_chunks::<8>();
    for block in blocks {
        let word = u64::from_ne_bytes(*block);
        let candidate = (word & 0xe0e0_e0e0_e0e0_e0e0) ^ 0x8080_8080_8080_8080;
        // Standard word-at-a-time zero-byte test, independent of endianness.
        if candidate.wrapping_sub(0x0101_0101_0101_0101) & !candidate & 0x8080_8080_8080_8080 != 0 {
            return true;
        }
    }
    remainder.iter().any(|b| (0x80..=0x9f).contains(b))
}

#[cfg(test)]
mod tests {
    use super::contains_c1;

    #[test]
    fn c1_range_scan_matches_individual_bytes() {
        for value in 0..=255 {
            for position in 0..24 {
                let mut bytes = [0xc2; 24];
                bytes[position] = value;
                for len in position + 1..=24 {
                    assert_eq!(contains_c1(&bytes[..len]), (0x80..=0x9f).contains(&value));
                }
            }
        }
    }
}
