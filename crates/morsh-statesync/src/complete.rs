use morsh_network::State;
use morsh_proto::host::{
    EchoAck, HostBytes, HostMessage, Instruction as HostInstruction, ResizeMessage,
};
use morsh_terminal::{DisplayDiff, MoshTerminal, ScreenSnapshot};
use prost::Message;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long an input frame must sit unechoed before it counts as "late acked".
/// Mosh's `Complete::ECHO_TIMEOUT`.
const ECHO_TIMEOUT: Duration = Duration::from_millis(50);

/// The complete terminal state, as the protocol sees it.
///
/// A port of mosh's `Terminal::Complete` (`src/statesync/completeterminal.cc`).
/// The wire form of a diff is a serialized `HostBuffers::HostMessage`, not raw
/// VT bytes: stock `mosh-client` parses it with `fatal_assert(
/// input.ParseFromString(diff) )` and aborts on anything else.
///
/// The screen contents are the canonical state; the emulator is created on
/// demand. That makes a state cheap to clone — the protocol keeps a history of
/// them — while still allowing VT bytes to be applied to any state in that
/// history.
pub struct Complete {
    /// Live emulator. `None` on a state that has only ever been diffed
    /// against; it is rebuilt from `screen` the first time bytes are applied.
    terminal: Option<MoshTerminal>,
    screen: ScreenSnapshot,
    echo_ack: u64,
    /// `(input frame number, when it arrived)`, oldest first.
    input_history: VecDeque<(u64, Instant)>,
}

impl Clone for Complete {
    /// Clones the state, not the emulator: the screen is what the protocol
    /// synchronizes, and rebuilding an emulator from it is only needed if the
    /// clone is later written to.
    fn clone(&self) -> Self {
        Self {
            terminal: None,
            screen: self.screen.clone(),
            echo_ack: self.echo_ack,
            input_history: self.input_history.clone(),
        }
    }
}

impl Complete {
    pub fn new(cols: u16, rows: u16) -> Result<Self, String> {
        let terminal = MoshTerminal::new(cols, rows, 0)?;
        let screen = terminal.snapshot()?;
        Ok(Self {
            terminal: Some(terminal),
            screen,
            echo_ack: 0,
            input_history: VecDeque::new(),
        })
    }

    /// Ensure an emulator exists, rebuilding it from the screen if needed.
    ///
    /// A rebuilt emulator is seeded with a full redraw of the screen, which
    /// restores the cells, the cursor, the window title, and the input modes.
    /// What it cannot restore is emulator state the snapshot does not carry —
    /// scrollback and the scrolling region, chiefly — so the fast paths keep a
    /// live emulator wherever they can rather than relying on this.
    fn terminal_mut(&mut self) -> Result<&mut MoshTerminal, String> {
        if self.terminal.is_none() {
            let (cols, rows) = (self.screen.cols, self.screen.rows_count);
            let mut term = MoshTerminal::new(cols, rows, 0)?;
            term.write(DisplayDiff::full_redraw(&self.screen).as_bytes());
            self.terminal = Some(term);
        }
        Ok(self.terminal.as_mut().unwrap())
    }

    fn refresh(&mut self) {
        if let Some(term) = &self.terminal {
            if let Ok(snap) = term.snapshot() {
                self.screen = snap;
            }
        }
    }

    /// Feed raw host (PTY) output into the terminal.
    ///
    /// Mosh's `Complete::act`, which also returns what the terminal wants to
    /// write back to the host. libghostty-vt does not surface terminal replies
    /// through this API, so nothing is returned; queries such as DA/DSR go
    /// unanswered, exactly as before this change.
    pub fn act(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.terminal_mut()?.write(bytes);
        self.refresh();
        Ok(())
    }

    /// Resize the terminal, preserving its contents.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        self.terminal_mut()?.resize(cols, rows)?;
        self.refresh();
        Ok(())
    }

    /// Get the echo acknowledgment number.
    pub fn echo_ack(&self) -> u64 {
        self.echo_ack
    }

    /// Set the echo acknowledgment number directly.
    pub fn set_echo_ack_num(&mut self, ack: u64) {
        self.echo_ack = ack;
    }

    /// Note that input frame `n` arrived at `now`.
    ///
    /// Mosh's `Complete::register_input_frame`.
    pub fn register_input_frame(&mut self, n: u64, now: Instant) {
        self.input_history.push_back((n, now));
    }

    /// Advance the echo ack to the newest input frame that has been sitting in
    /// the terminal for at least `ECHO_TIMEOUT`.
    ///
    /// Returns whether it changed, in which case the caller should publish the
    /// new state. Mosh's `Complete::set_echo_ack`. This is what drives the
    /// client's prediction underlining: it measures when input was actually
    /// *echoed*, which is later than when the packet carrying it was acked.
    pub fn set_echo_ack(&mut self, now: Instant) -> bool {
        let mut newest_echo_ack = 0u64;
        for &(num, when) in &self.input_history {
            if now.saturating_duration_since(when) >= ECHO_TIMEOUT {
                newest_echo_ack = num;
            }
        }

        self.input_history.retain(|&(num, _)| num >= newest_echo_ack);

        let changed = self.echo_ack != newest_echo_ack;
        self.echo_ack = newest_echo_ack;
        changed
    }

    /// Milliseconds until [`Complete::set_echo_ack`] could next change, or
    /// `None` if nothing is pending.
    pub fn wait_time_ms(&self, now: Instant) -> Option<u64> {
        let (_, when) = *self.input_history.get(1)?;
        let deadline = when + ECHO_TIMEOUT;
        Some(deadline.saturating_duration_since(now).as_millis() as u64)
    }

    /// The current screen contents.
    pub fn screen(&self) -> &ScreenSnapshot {
        &self.screen
    }

    /// The current screen contents, cloned.
    pub fn snapshot(&self) -> ScreenSnapshot {
        self.screen.clone()
    }

    /// Current terminal dimensions.
    pub fn dimensions(&self) -> (u16, u16) {
        (self.screen.cols, self.screen.rows_count)
    }

    /// The diff that reconstructs this state from a blank terminal of the same
    /// size. Mosh's `Complete::init_diff`.
    pub fn init_diff(&self) -> Result<Vec<u8>, String> {
        let blank = Complete::new(self.screen.cols, self.screen.rows_count)?;
        Ok(self.diff_from(&blank))
    }
}

impl State for Complete {
    /// Build a `HostBuffers::HostMessage`, in mosh's order: echo ack when it
    /// changed, resize when the dimensions changed, then the display update.
    fn diff_from(&self, existing: &Self) -> Vec<u8> {
        let mut output = HostMessage {
            instruction: Vec::new(),
        };

        if existing.echo_ack != self.echo_ack {
            output.instruction.push(HostInstruction {
                hostbytes: None,
                resize: None,
                echoack: Some(EchoAck {
                    echo_ack_num: Some(self.echo_ack),
                }),
            });
        }

        if existing.screen != self.screen {
            if existing.screen.cols != self.screen.cols
                || existing.screen.rows_count != self.screen.rows_count
            {
                output.instruction.push(HostInstruction {
                    hostbytes: None,
                    resize: Some(ResizeMessage {
                        width: Some(self.screen.cols as i32),
                        height: Some(self.screen.rows_count as i32),
                    }),
                    echoack: None,
                });
            }

            let update = DisplayDiff::diff(&existing.screen, &self.screen);
            if !update.is_empty() {
                output.instruction.push(HostInstruction {
                    hostbytes: Some(HostBytes {
                        hoststring: Some(update.into_bytes()),
                    }),
                    resize: None,
                    echoack: None,
                });
            }
        }

        let mut buf = Vec::with_capacity(output.encoded_len());
        output.encode(&mut buf).expect("HostMessage encode");
        buf
    }

    fn apply_string(&mut self, diff: &[u8]) -> Result<(), String> {
        let input = HostMessage::decode(diff)
            .map_err(|e| format!("Malformed HostMessage from peer: {e}"))?;

        for inst in &input.instruction {
            if let Some(hb) = &inst.hostbytes {
                if let Some(bytes) = &hb.hoststring {
                    self.act(bytes)?;
                }
            } else if let Some(resize) = &inst.resize {
                let w = resize.width.unwrap_or(80).clamp(1, u16::MAX as i32) as u16;
                let h = resize.height.unwrap_or(24).clamp(1, u16::MAX as i32) as u16;
                self.resize(w, h)?;
            } else if let Some(ea) = &inst.echoack {
                if let Some(num) = ea.echo_ack_num {
                    // Mosh asserts this never goes backwards; a peer that
                    // sends a stale value gets it ignored rather than
                    // crashing us.
                    self.echo_ack = self.echo_ack.max(num);
                }
            }
        }

        Ok(())
    }

    fn state_equal(&self, other: &Self) -> bool {
        self.echo_ack == other.echo_ack && self.screen == other.screen
    }

    /// Move the live emulator across rather than rebuilding one.
    ///
    /// The protocol calls this on a copy that is about to have a diff applied
    /// to it. `source` keeps its screen and rebuilds lazily on the rare
    /// occasion it is written to again, so the common in-order case never
    /// constructs an emulator or replays a full redraw.
    fn adopt_from(&mut self, source: &mut Self) {
        if self.terminal.is_none() && source.screen == self.screen {
            self.terminal = source.terminal.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn create_complete() {
        let c = Complete::new(80, 24);
        assert!(c.is_ok());
        assert_eq!(c.unwrap().dimensions(), (80, 24));
    }

    #[test]
    #[serial]
    fn apply_text_to_snapshot() {
        let mut c = Complete::new(80, 1).unwrap();
        c.act(b"Hello").unwrap();
        let snap = c.screen();
        assert_eq!(snap.rows[0][0].text, "H");
        assert_eq!(snap.rows[0][1].text, "e");
        assert_eq!(snap.rows[0][4].text, "o");
    }

    #[test]
    #[serial]
    fn apply_cursor_positioning() {
        let mut c = Complete::new(10, 3).unwrap();
        c.act(b"\x1b[2;3HX").unwrap();
        assert_eq!(c.screen().rows[1][2].text, "X");
    }

    #[test]
    #[serial]
    fn apply_color_escape() {
        let mut c = Complete::new(10, 1).unwrap();
        c.act(b"\x1b[1;31mX\x1b[0m").unwrap();
        assert!(c.screen().rows[0][0].style.bold);
    }

    /// The confirmed interop crash: stock mosh-client does
    /// `fatal_assert( input.ParseFromString( diff ) )`, so a diff that is not
    /// a serialized HostMessage aborts it.
    #[test]
    #[serial]
    fn diff_is_a_parseable_host_message() {
        let blank = Complete::new(80, 1).unwrap();
        let mut with_text = Complete::new(80, 1).unwrap();
        with_text.act(b"Hi").unwrap();

        let diff = with_text.diff_from(&blank);
        let msg = HostMessage::decode(diff.as_slice()).expect("must parse as a HostMessage");
        assert_eq!(msg.instruction.len(), 1);
        let bytes = msg.instruction[0]
            .hostbytes
            .as_ref()
            .unwrap()
            .hoststring
            .as_ref()
            .unwrap();
        assert!(String::from_utf8_lossy(bytes).contains("Hi"));
    }

    #[test]
    #[serial]
    fn diff_round_trips_through_apply_string() {
        let mut source = Complete::new(40, 4).unwrap();
        source.act(b"first line\r\n\x1b[1;32msecond\x1b[0m").unwrap();

        let mut target = Complete::new(40, 4).unwrap();
        let blank = Complete::new(40, 4).unwrap();
        target.apply_string(&source.diff_from(&blank)).unwrap();

        assert!(
            source.state_equal(&target),
            "round trip lost state:\n{:?}\nvs\n{:?}",
            source.screen().rows[0],
            target.screen().rows[0]
        );
    }

    #[test]
    #[serial]
    fn identical_states_produce_an_empty_message() {
        let mut a = Complete::new(80, 1).unwrap();
        a.act(b"Hello").unwrap();
        let mut b = Complete::new(80, 1).unwrap();
        b.act(b"Hello").unwrap();
        assert_eq!(a.diff_from(&b), Vec::<u8>::new());
        assert!(a.state_equal(&b));
    }

    #[test]
    #[serial]
    fn resize_is_announced_and_preserves_content() {
        let mut a = Complete::new(80, 24).unwrap();
        a.act(b"keep me").unwrap();
        let before = a.clone();

        a.resize(100, 30).unwrap();
        assert_eq!(a.dimensions(), (100, 30));
        assert_eq!(a.screen().rows[0][0].text, "k", "resize wiped the screen");

        let msg = HostMessage::decode(a.diff_from(&before).as_slice()).unwrap();
        let resize = msg
            .instruction
            .iter()
            .find_map(|i| i.resize.as_ref())
            .expect("resize instruction");
        assert_eq!((resize.width, resize.height), (Some(100), Some(30)));
    }

    #[test]
    #[serial]
    fn echo_ack_changes_are_carried_in_the_diff() {
        let mut a = Complete::new(20, 2).unwrap();
        let before = a.clone();
        a.set_echo_ack_num(42);

        let msg = HostMessage::decode(a.diff_from(&before).as_slice()).unwrap();
        let ack = msg
            .instruction
            .iter()
            .find_map(|i| i.echoack.as_ref())
            .expect("echoack instruction");
        assert_eq!(ack.echo_ack_num, Some(42));

        let mut target = before.clone();
        target.apply_string(&a.diff_from(&before)).unwrap();
        assert_eq!(target.echo_ack(), 42);
    }

    #[test]
    #[serial]
    fn echo_ack_advances_only_after_the_echo_timeout() {
        let mut c = Complete::new(20, 2).unwrap();
        let t0 = Instant::now();
        c.register_input_frame(7, t0);

        assert!(!c.set_echo_ack(t0 + Duration::from_millis(10)));
        assert_eq!(c.echo_ack(), 0);

        assert!(c.set_echo_ack(t0 + ECHO_TIMEOUT));
        assert_eq!(c.echo_ack(), 7);

        // Idempotent once it has caught up.
        assert!(!c.set_echo_ack(t0 + Duration::from_secs(1)));
    }

    #[test]
    #[serial]
    fn echo_ack_takes_the_newest_expired_frame() {
        let mut c = Complete::new(20, 2).unwrap();
        let t0 = Instant::now();
        c.register_input_frame(1, t0);
        c.register_input_frame(2, t0 + Duration::from_millis(10));
        c.register_input_frame(3, t0 + Duration::from_millis(100));

        c.set_echo_ack(t0 + Duration::from_millis(70));
        assert_eq!(c.echo_ack(), 2, "frame 3 has not aged out yet");
    }

    #[test]
    #[serial]
    fn a_malformed_diff_is_an_error_not_a_panic() {
        let mut c = Complete::new(20, 2).unwrap();
        assert!(c.apply_string(b"\xff\xff\xff not a protobuf").is_err());
    }

    /// A cloned state carries no emulator, but writing to it must still work.
    #[test]
    #[serial]
    fn a_clone_can_be_written_to() {
        let mut a = Complete::new(20, 2).unwrap();
        a.act(b"seed").unwrap();
        let mut b = a.clone();
        assert!(b.terminal.is_none());

        b.act(b"!").unwrap();
        assert_eq!(b.screen().rows[0][4].text, "!");
        assert_eq!(b.screen().rows[0][0].text, "s", "rehydration lost content");
    }

    #[test]
    #[serial]
    fn dec_private_mode_with_text() {
        let mut c = Complete::new(80, 1).unwrap();
        c.act(b"\x1b[?2004h$ ").unwrap();
        assert_eq!(c.screen().rows[0][0].text, "$");
        // A blank cell is canonically empty, whether it was never written or
        // was written with a space.
        assert_eq!(c.screen().rows[0][1].text, "");
        assert!(c.screen().bracketed_paste);
    }

    #[test]
    #[serial]
    fn cursor_visibility_sequence() {
        let mut c = Complete::new(10, 1).unwrap();
        c.act(b"\x1b[?25l").unwrap();
        assert!(!c.screen().cursor_visible);
    }
}
