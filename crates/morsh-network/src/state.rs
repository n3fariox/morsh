//! The state abstraction the State Synchronization Protocol synchronizes.
//!
//! This is the Rust equivalent of the `MyState`/`RemoteState` template
//! parameters in mosh's `TransportSender` / `Transport`
//! (`src/network/transportsender.h`, `src/network/networktransport.h`).
//!
//! The protocol never inspects a state's contents: it only ever diffs one
//! against another, applies a diff, compares two for equality, and trims a
//! common prefix.  Everything else — terminal emulation, user input queues —
//! lives behind these four operations.

use std::time::Instant;

/// A synchronizable state.
///
/// Implementations must satisfy the round-trip identity that the protocol
/// depends on:
///
/// ```text
/// let mut b = a.clone();
/// b.apply_string(&c.diff_from(&a))?;
/// assert!(b.state_equal(&c));
/// ```
pub trait State: Clone {
    /// Whether [`State::subtract`] does anything.
    ///
    /// Queue-shaped states (user input) shrink as the peer acknowledges them;
    /// terminal-shaped states have no prefix to trim. The protocol skips the
    /// whole rationalization pass when this is `false`, which avoids cloning a
    /// framebuffer on every tick just to run a no-op.
    const HAS_PREFIX: bool = false;

    /// Bytes that transform `existing` into `self`.
    fn diff_from(&self, existing: &Self) -> Vec<u8>;

    /// Apply a diff produced by [`State::diff_from`].
    ///
    /// Returns `Err` on a malformed diff.  Mosh calls `fatal_assert` here; we
    /// drop the packet instead, because the diff is attacker-influenced data
    /// and a panic in the server is a denial of service.
    fn apply_string(&mut self, diff: &[u8]) -> Result<(), String>;

    /// Discard whatever `prefix` already covers.
    ///
    /// Used by `rationalize_states` to keep queue-shaped states (user input)
    /// from growing without bound.  States with no notion of a prefix — a
    /// terminal, for instance — leave this as the default no-op.
    fn subtract(&mut self, _prefix: &Self) {}

    /// Whether two states are equivalent for synchronization purposes.
    fn state_equal(&self, other: &Self) -> bool;

    /// Take over any expensive resource `source` is holding.
    ///
    /// Called on a freshly cloned state just before a diff is applied to it,
    /// with the state it was cloned from. The two are equal at that moment, so
    /// an implementation backed by something costly to rebuild — a terminal
    /// emulator — can move it across instead of reconstructing it. `source`
    /// must remain usable afterwards; the default is to do nothing.
    fn adopt_from(&mut self, _source: &mut Self) {}
}

/// A state together with the number it was assigned and when it was recorded.
///
/// Mirrors `Network::TimestampedState` (`src/network/transportstate.h`).
#[derive(Debug, Clone)]
pub struct TimestampedState<S> {
    pub timestamp: Instant,
    pub num: u64,
    pub state: S,
}

impl<S> TimestampedState<S> {
    pub fn new(timestamp: Instant, num: u64, state: S) -> Self {
        Self {
            timestamp,
            num,
            state,
        }
    }
}
