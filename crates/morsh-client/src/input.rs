//! Keyboard and resize handling: reading the local terminal's raw input bytes
//! and feeding them to the transport and the predictor.
//!
//! Like stock mosh, input is never decoded into keys. Whatever the local
//! terminal sends (modified arrows, function keys, pastes, mouse reports) is
//! forwarded to the remote pty byte for byte; only the escape sequence is
//! intercepted.

use crate::ClientTransport;
use morsh_prediction::{NotificationEngine, PredictionEngine};
use morsh_terminal::ScreenSnapshot;
use tokio::sync::mpsc;

/// Ctrl-^ (Ctrl-Shift-6) — the stock mosh escape key.
const DEFAULT_ESCAPE_KEY: u8 = 0x1E;

/// Bulk input this long is treated as a paste and not predicted.
const PASTE_THRESHOLD: usize = 100;

pub enum TermEvent {
    Input(Vec<u8>),
    Resize(i32, i32),
    /// Show (true) or take down (false) the escape key help.
    EscapeHelp(bool),
    Quit,
}

/// The escape key and how to type it literally, set from `MOSH_ESCAPE_KEY`
/// exactly as stock mosh-client does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscapeConfig {
    key: u8,
    /// Typed after the escape key, sends one literal escape key. Two
    /// spellings so a Ctrl-letter escape accepts the letter in either case.
    pass_key: u8,
    pass_key2: u8,
    /// A printable escape key only counts at the start of a line, so it can
    /// still be typed normally.
    requires_lf: bool,
}

impl Default for EscapeConfig {
    fn default() -> Self {
        Self::for_key(DEFAULT_ESCAPE_KEY)
    }
}

impl EscapeConfig {
    /// Reads `MOSH_ESCAPE_KEY`. `None` means the escape key is disabled.
    pub fn from_env() -> Option<Self> {
        Self::from_setting(std::env::var_os("MOSH_ESCAPE_KEY").map(|v| v.into_encoded_bytes()))
    }

    /// Unset or invalid falls back to Ctrl-^; empty disables the escape key;
    /// a single ASCII character becomes the escape key, unless it is one of
    /// the control keys that would be too easy to hit by accident.
    fn from_setting(value: Option<Vec<u8>>) -> Option<Self> {
        let key = match value.as_deref() {
            Some([]) => return None,
            Some(&[key]) if (1..128).contains(&key) => key,
            _ => DEFAULT_ESCAPE_KEY,
        };
        // Ctrl-C, Ctrl-D, LF, Ctrl-L, CR.
        if matches!(key, 0x03 | 0x04 | 0x0A | 0x0C | 0x0D) {
            return Some(Self::default());
        }
        Some(Self::for_key(key))
    }

    fn for_key(key: u8) -> Self {
        let pass_key = if key < 32 { key + b'@' } else { key };
        Self {
            key,
            pass_key,
            pass_key2: pass_key.to_ascii_lowercase(),
            requires_lf: key >= 32,
        }
    }

    /// "Ctrl-^" for a control key, the quoted character otherwise.
    fn key_name(&self) -> String {
        if self.key < 32 {
            format!("Ctrl-{}", self.pass_key as char)
        } else {
            format!("\"{}\"", self.key as char)
        }
    }

    /// How to quit, for the notification bar: "Ctrl-^ .".
    pub fn quit_keystroke(&self) -> String {
        format!("{} .", self.key_name())
    }

    /// Shown while the escape key is waiting for its second key.
    pub fn help(&self) -> String {
        format!(
            "Commands: Ctrl-Z suspends, \".\" quits, \"{}\" gives literal {}",
            self.pass_key as char,
            self.key_name()
        )
    }
}

/// What the escape filter made of a chunk of input.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Filtered {
    /// Bytes to send to the remote side.
    pub send: Vec<u8>,
    /// Show (true) or take down (false) the escape key help.
    pub help: Option<bool>,
    /// The user typed the quit sequence. Input after it is dropped.
    pub quit: bool,
}

/// Recognizes the two-keystroke escape sequence in the input stream, as
/// stock mosh-client does.
///
/// The escape key followed by `.` quits; by Ctrl-Z suspends; by the pass key
/// (`^` for Ctrl-^) sends one literal escape key; by anything else sends the
/// escape key and that byte through unchanged. The two bytes may arrive in
/// different reads.
pub struct EscapeFilter {
    /// `None` when the escape key is disabled.
    config: Option<EscapeConfig>,
    armed: bool,
    /// The last byte was LF or CR, for a printable escape key.
    lf_entered: bool,
}

impl EscapeFilter {
    pub fn new(config: Option<EscapeConfig>) -> Self {
        Self {
            config,
            armed: false,
            lf_entered: false,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Filtered {
        let mut out = Filtered::default();
        let Some(config) = &self.config else {
            out.send = bytes.to_vec();
            return out;
        };

        let mut was_armed = self.armed;
        for &byte in bytes {
            if self.armed {
                self.armed = false;
                match byte {
                    b'.' => {
                        out.quit = true;
                        return out;
                    }
                    // Ctrl-Z: suspend, not yet implemented. Swallowed rather
                    // than passed through so it does not reach the remote
                    // shell and stop the wrong process.
                    0x1A => {}
                    b if b == config.pass_key || b == config.pass_key2 => out.send.push(config.key),
                    other => out.send.extend_from_slice(&[config.key, other]),
                }
                continue;
            }

            if byte == config.key && (self.lf_entered || !config.requires_lf) {
                self.armed = true;
                was_armed = true;
                self.lf_entered = false;
                continue;
            }
            self.lf_entered = byte == b'\n' || byte == b'\r';
            out.send.push(byte);
        }

        if self.armed {
            out.help = Some(true);
        } else if was_armed {
            out.help = Some(false);
        }
        out
    }
}

/// Pass one chunk of raw input through the escape filter and publish the
/// result. Returns false once the receiver is gone.
fn publish(escape: &mut EscapeFilter, bytes: &[u8], term_tx: &mpsc::Sender<TermEvent>) -> bool {
    let filtered = escape.feed(bytes);
    if !filtered.send.is_empty()
        && term_tx
            .blocking_send(TermEvent::Input(filtered.send))
            .is_err()
    {
        return false;
    }
    if let Some(show) = filtered.help {
        if term_tx.blocking_send(TermEvent::EscapeHelp(show)).is_err() {
            return false;
        }
    }
    // Keep reading after a quit: the main loop may still be waiting for the
    // server to acknowledge the shutdown, and it is the one that decides to
    // exit.
    !(filtered.quit && term_tx.blocking_send(TermEvent::Quit).is_err())
}

/// Read the local terminal and publish [`TermEvent`]s.
///
/// Reads block, so they run on a dedicated OS thread. It is never joined: it
/// sits in a read until the process exits.
#[cfg(unix)]
pub fn spawn_input_reader(term_tx: mpsc::Sender<TermEvent>, escape: Option<EscapeConfig>) {
    use std::io::Read;
    use tokio::signal::unix::{signal, SignalKind};

    let resize_tx = term_tx.clone();
    match signal(SignalKind::window_change()) {
        Ok(mut winch) => {
            tokio::spawn(async move {
                while winch.recv().await.is_some() {
                    if let Ok((w, h)) = crossterm::terminal::size() {
                        if resize_tx
                            .send(TermEvent::Resize(w as i32, h as i32))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            });
        }
        Err(e) => tracing::warn!("Cannot watch for SIGWINCH: {e}"),
    }

    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut escape = EscapeFilter::new(escape);
        let mut buf = [0u8; 16384];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if !publish(&mut escape, &buf[..n], &term_tx) {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    tracing::warn!("stdin read failed: {e}");
                    break;
                }
            }
        }
    });
}

/// Read the local console and publish [`TermEvent`]s.
///
/// With `ENABLE_VIRTUAL_TERMINAL_INPUT` set (see [`enable_vt_input`]) the
/// console delivers the same VT byte stream a Unix terminal would, as the
/// characters of key-down records; resizes arrive as buffer-size records in
/// the same queue.
///
/// Reads block, so they run on a dedicated OS thread. It is never joined: it
/// sits in a read until the process exits.
#[cfg(windows)]
pub fn spawn_input_reader(term_tx: mpsc::Sender<TermEvent>, escape: Option<EscapeConfig>) {
    use windows_sys::Win32::System::Console::{
        GetStdHandle, ReadConsoleInputW, INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE,
        WINDOW_BUFFER_SIZE_EVENT,
    };

    std::thread::spawn(move || {
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut escape = EscapeFilter::new(escape);
        let mut records: [INPUT_RECORD; 128] = unsafe { std::mem::zeroed() };
        let mut utf16 = Utf16ToUtf8::default();
        loop {
            let mut count = 0u32;
            let ok = unsafe {
                ReadConsoleInputW(
                    handle,
                    records.as_mut_ptr(),
                    records.len() as u32,
                    &mut count,
                )
            };
            if ok == 0 {
                tracing::warn!(
                    "ReadConsoleInputW failed: {}",
                    std::io::Error::last_os_error()
                );
                break;
            }

            let mut resized = false;
            for record in &records[..count as usize] {
                match record.EventType as u32 {
                    KEY_EVENT => {
                        let key = unsafe { record.Event.KeyEvent };
                        let unit = unsafe { key.uChar.UnicodeChar };
                        // Key-up records and bare modifier presses carry no
                        // character; the VT sequences are all in key-downs.
                        if key.bKeyDown != 0 && unit != 0 {
                            for _ in 0..key.wRepeatCount.max(1) {
                                utf16.push(unit);
                            }
                        }
                    }
                    WINDOW_BUFFER_SIZE_EVENT => resized = true,
                    _ => {}
                }
            }

            let bytes = utf16.take();
            if !bytes.is_empty() && !publish(&mut escape, &bytes, &term_tx) {
                break;
            }
            // The record holds the buffer size, not the window size; ask for
            // the latter.
            if resized {
                if let Ok((w, h)) = crossterm::terminal::size() {
                    if term_tx
                        .blocking_send(TermEvent::Resize(w as i32, h as i32))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
}

/// Turns the console's UTF-16 code units into UTF-8, holding back a high
/// surrogate until its pair arrives (possibly in the next read).
#[cfg(any(windows, test))]
#[derive(Default)]
struct Utf16ToUtf8 {
    pending_high: Option<u16>,
    out: Vec<u8>,
}

#[cfg(any(windows, test))]
impl Utf16ToUtf8 {
    fn push(&mut self, unit: u16) {
        let units: &[u16] = match (self.pending_high.take(), unit) {
            (None, 0xD800..=0xDBFF) => {
                self.pending_high = Some(unit);
                return;
            }
            (Some(high), 0xDC00..=0xDFFF) => &[high, unit],
            // A lone high surrogate: emit it (as U+FFFD) and carry on with this unit.
            (Some(high), _) => {
                self.encode(&[high]);
                return self.push(unit);
            }
            (None, _) => &[unit],
        };
        self.encode(units);
    }

    fn encode(&mut self, units: &[u16]) {
        for ch in char::decode_utf16(units.iter().copied()) {
            let ch = ch.unwrap_or(char::REPLACEMENT_CHARACTER);
            let mut buf = [0u8; 4];
            self.out
                .extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
    }

    fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }
}

/// Windows console: have the console hand us raw VT input instead of key
/// records, and report resizes. Call after `enable_raw_mode`. Returns the
/// original mode to restore on exit.
#[cfg(windows)]
pub fn enable_vt_input() -> Option<u32> {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_MOUSE_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_WINDOW_INPUT, STD_INPUT_HANDLE,
    };
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return None;
        }
        // Mouse input would arrive as console records rather than VT bytes;
        // with it off, mouse reports come through the VT stream when the
        // remote application asks for them.
        let new_mode =
            (mode | ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_WINDOW_INPUT) & !ENABLE_MOUSE_INPUT;
        if SetConsoleMode(handle, new_mode) == 0 {
            tracing::warn!("Failed to set console input mode {new_mode:#x}");
            return None;
        }
        Some(mode)
    }
}

#[cfg(windows)]
pub fn restore_input_mode(mode: Option<u32>) {
    use windows_sys::Win32::System::Console::{GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE};
    if let Some(mode) = mode {
        unsafe {
            SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), mode);
        }
    }
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
        TermEvent::Input(bytes) => {
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
        TermEvent::EscapeHelp(show) => {
            if show {
                notifications.show_escape_help();
            } else {
                notifications.hide_escape_help();
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

    const ESC: u8 = DEFAULT_ESCAPE_KEY;

    fn filter() -> EscapeFilter {
        EscapeFilter::new(Some(EscapeConfig::default()))
    }

    fn setting(s: &str) -> Option<EscapeConfig> {
        EscapeConfig::from_setting(Some(s.as_bytes().to_vec()))
    }

    fn send(bytes: &[u8]) -> Filtered {
        Filtered {
            send: bytes.to_vec(),
            ..Default::default()
        }
    }

    /// The second key of an escape sequence: sends `bytes` and takes down the help.
    fn after_escape(bytes: &[u8]) -> Filtered {
        Filtered {
            send: bytes.to_vec(),
            help: Some(false),
            quit: false,
        }
    }

    #[test]
    fn ordinary_input_passes_straight_through() {
        let mut escape = filter();
        assert_eq!(escape.feed(b"a"), send(b"a"));
        // Modified keys are forwarded exactly as the terminal sent them.
        assert_eq!(escape.feed(b"\x1b[1;2D"), send(b"\x1b[1;2D"));
        assert_eq!(escape.feed("é".as_bytes()), send("é".as_bytes()));
    }

    #[test]
    fn escape_key_shows_help_until_the_next_key() {
        let mut escape = filter();
        assert_eq!(
            escape.feed(&[ESC]),
            Filtered {
                help: Some(true),
                ..Default::default()
            }
        );
        assert_eq!(escape.feed(b"x"), after_escape(&[ESC, b'x']));
    }

    #[test]
    fn escape_then_dot_quits() {
        let mut escape = filter();
        escape.feed(&[ESC]);
        assert_eq!(
            escape.feed(b"."),
            Filtered {
                quit: true,
                ..Default::default()
            }
        );
    }

    #[test]
    fn quit_sequence_inside_one_read() {
        let mut escape = filter();
        assert_eq!(
            escape.feed(&[b'l', b's', ESC, b'.', b'x']),
            Filtered {
                send: b"ls".to_vec(),
                help: None,
                quit: true,
            }
        );
    }

    #[test]
    fn escape_then_pass_key_sends_one_literal_escape() {
        let mut escape = filter();
        escape.feed(&[ESC]);
        assert_eq!(escape.feed(b"^"), after_escape(&[ESC]));
        // And the filter is disarmed again, so the next '.' is just a period.
        assert_eq!(escape.feed(b"."), send(b"."));
    }

    #[test]
    fn escape_twice_sends_both() {
        let mut escape = filter();
        assert_eq!(
            escape.feed(&[ESC, ESC, b'.']),
            after_escape(&[ESC, ESC, b'.'])
        );
    }

    #[test]
    fn escape_then_other_input_sends_both() {
        let mut escape = filter();
        escape.feed(&[ESC]);
        assert_eq!(
            escape.feed(b"\x1b[A"),
            after_escape(&[ESC, 0x1b, b'[', b'A'])
        );
    }

    #[test]
    fn escape_then_ctrl_z_is_swallowed() {
        let mut escape = filter();
        assert_eq!(escape.feed(&[ESC, 0x1A, b'.']), after_escape(b"."));
    }

    #[test]
    fn disabled_escape_key_passes_everything() {
        let mut escape = EscapeFilter::new(None);
        assert_eq!(escape.feed(&[ESC, b'.']), send(&[ESC, b'.']));
    }

    #[test]
    fn escape_key_setting_follows_mosh() {
        assert_eq!(
            EscapeConfig::from_setting(None),
            Some(EscapeConfig::default())
        );
        assert_eq!(setting(""), None);
        // Too long, non-ASCII, or a forbidden control key: the default.
        for s in ["ab", "é", "\x03", "\x04", "\n", "\x0c", "\r"] {
            assert_eq!(setting(s), Some(EscapeConfig::default()), "{s:?}");
        }

        let ctrl_a = setting("\x01").unwrap();
        assert_eq!((ctrl_a.pass_key, ctrl_a.pass_key2), (b'A', b'a'));
        assert!(!ctrl_a.requires_lf);
        assert_eq!(ctrl_a.quit_keystroke(), "Ctrl-A .");
        assert_eq!(
            ctrl_a.help(),
            "Commands: Ctrl-Z suspends, \".\" quits, \"A\" gives literal Ctrl-A"
        );

        let tilde = setting("~").unwrap();
        assert_eq!((tilde.pass_key, tilde.pass_key2), (b'~', b'~'));
        assert!(tilde.requires_lf);
        assert_eq!(tilde.quit_keystroke(), "\"~\" .");

        let default = EscapeConfig::default();
        assert_eq!(default.quit_keystroke(), "Ctrl-^ .");
        assert_eq!(
            default.help(),
            "Commands: Ctrl-Z suspends, \".\" quits, \"^\" gives literal Ctrl-^"
        );
    }

    #[test]
    fn ctrl_letter_escape_takes_either_case_as_pass_key() {
        let mut escape = EscapeFilter::new(setting("\x01"));
        assert_eq!(escape.feed(&[0x01, b'a']), after_escape(&[0x01]));
        assert_eq!(escape.feed(&[0x01, b'A']), after_escape(&[0x01]));
    }

    #[test]
    fn printable_escape_key_only_counts_at_line_start() {
        let mut escape = EscapeFilter::new(setting("~"));
        // At the very start of the session, and mid-line, it is just a tilde.
        assert_eq!(escape.feed(b"~."), send(b"~."));
        assert_eq!(escape.feed(b"cd ~."), send(b"cd ~."));
        assert_eq!(
            escape.feed(b"\r~."),
            Filtered {
                send: b"\r".to_vec(),
                help: None,
                quit: true,
            }
        );
        // Typed twice after a newline, the second tilde is literal.
        let mut escape = EscapeFilter::new(setting("~"));
        assert_eq!(escape.feed(b"\n~~~"), after_escape(b"\n~~"));
    }

    #[test]
    fn utf16_input_becomes_utf8() {
        let mut conv = Utf16ToUtf8::default();
        for unit in "a\u{e9}\x1b[1;2D".encode_utf16() {
            conv.push(unit);
        }
        assert_eq!(conv.take(), "a\u{e9}\x1b[1;2D".as_bytes());

        // A surrogate pair split across two reads.
        let units: Vec<u16> = "\u{1F600}".encode_utf16().collect();
        conv.push(units[0]);
        assert!(conv.take().is_empty());
        conv.push(units[1]);
        assert_eq!(conv.take(), "\u{1F600}".as_bytes());

        // A lone high surrogate does not swallow the next character.
        conv.push(0xD800);
        conv.push(b'z' as u16);
        assert_eq!(conv.take(), "\u{FFFD}z".as_bytes());
    }
}
