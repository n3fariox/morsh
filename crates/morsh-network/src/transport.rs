//! The State Synchronization Protocol.
//!
//! A port of mosh's `TransportSender` (`src/network/transportsender.h`,
//! `transportsender-impl.h`) and `Transport` (`networktransport-impl.h`).
//!
//! The sender keeps a history of the states it has sent. Every packet is a
//! diff from a state the receiver is *assumed* to hold; if that assumption
//! goes unconfirmed for longer than a retransmission timeout, the assumption
//! regresses to an older state and the next diff automatically covers the gap.
//! That, and not any explicit retransmission of packets, is what makes the
//! protocol tolerate loss.
//!
//! The receiver keeps a history of the states it has received, and refuses any
//! instruction whose `new_num` it already holds (duplicate suppression) or
//! whose `old_num` it does not hold (out-of-order / forged reference). Mosh's
//! comment on the latter — *"this is security-sensitive and part of how we
//! enforce idempotency"* — applies here verbatim.

use crate::connection::Connection;
use crate::constants::*;
use crate::fragment::make_chaff;
use crate::state::{State, TimestampedState};
use morsh_proto::transport::Instruction as TransportInstruction;
use std::time::{Duration, Instant};

/// `new_num` / `ack_num` sentinel meaning "shutdown". Mosh's `uint64_t(-1)`.
pub const SHUTDOWN_NUM: u64 = u64::MAX;

/// Cap on the sender's state history (mosh: 32, dropping from the middle).
const SENT_STATES_MAX: usize = 32;
/// Cap on the receiver's state history (mosh: 1024, with a quench timer).
const RECEIVED_STATES_MAX: usize = 1024;
/// How long the receiver refuses to grow its queue once it has overflowed.
const RECEIVER_QUENCH_MS: u64 = 15000;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// The sending half of one direction of the protocol.
pub struct TransportSender<S: State> {
    /// The state we want the receiver to reach.
    current_state: S,
    /// History: `[0]` is the last acknowledged state, the last element is the
    /// most recently sent one.
    sent_states: Vec<TimestampedState<S>>,
    /// Index into `sent_states` of the state we assume the receiver holds.
    assumed_receiver_state: usize,

    next_ack_time: Instant,
    /// `None` means "no send scheduled" (mosh's `uint64_t(-1)`).
    next_send_time: Option<Instant>,

    shutdown_in_progress: bool,
    shutdown_tries: u32,
    shutdown_start: Option<Instant>,

    /// What we tell the peer we have received.
    ack_num: u64,
    pending_data_ack: bool,

    /// Time to collect input before sending (mosh's `SEND_MINDELAY`).
    send_mindelay: Duration,
    /// When we last received a new state. `None` = never heard from the peer.
    last_heard: Option<Instant>,
    /// When the current state first diverged from the last sent state.
    mindelay_clock: Option<Instant>,
}

impl<S: State> TransportSender<S> {
    pub fn new(initial_state: S) -> Self {
        let now = Instant::now();
        Self {
            sent_states: vec![TimestampedState::new(now, 0, initial_state.clone())],
            current_state: initial_state,
            assumed_receiver_state: 0,
            next_ack_time: now,
            next_send_time: Some(now),
            shutdown_in_progress: false,
            shutdown_tries: 0,
            shutdown_start: None,
            ack_num: 0,
            pending_data_ack: false,
            send_mindelay: ms(SEND_MINDELAY_MS),
            last_heard: None,
            mindelay_clock: None,
        }
    }

    /// Roughly two frames per RTT, bounded by the frame-rate limits.
    pub fn send_interval(&self, srtt_ms: u64) -> Duration {
        ms(srtt_ms
            .div_ceil(2)
            .clamp(SEND_INTERVAL_MIN_MS, SEND_INTERVAL_MAX_MS))
    }

    /// The state the caller mutates. Illegal once shutdown has begun.
    pub fn current_state(&self) -> &S {
        &self.current_state
    }

    pub fn current_state_mut(&mut self) -> &mut S {
        debug_assert!(!self.shutdown_in_progress);
        &mut self.current_state
    }

    pub fn set_current_state(&mut self, state: S) {
        debug_assert!(!self.shutdown_in_progress);
        self.current_state = state;
    }

    pub fn set_ack_num(&mut self, ack_num: u64) {
        self.ack_num = ack_num;
    }

    /// Accelerate the reply ack because the peer sent us data.
    pub fn set_data_ack(&mut self) {
        self.pending_data_ack = true;
    }

    pub fn remote_heard(&mut self, when: Instant) {
        self.last_heard = Some(when);
    }

    pub fn start_shutdown(&mut self) {
        if !self.shutdown_in_progress {
            self.shutdown_start = Some(Instant::now());
            self.shutdown_in_progress = true;
        }
    }

    pub fn shutdown_in_progress(&self) -> bool {
        self.shutdown_in_progress
    }

    /// Our shutdown has been acknowledged: the peer acked the sentinel state.
    pub fn shutdown_acknowledged(&self) -> bool {
        self.sent_states[0].num == SHUTDOWN_NUM
    }

    pub fn shutdown_ack_timed_out(&self) -> bool {
        if !self.shutdown_in_progress {
            return false;
        }
        if self.shutdown_tries >= SHUTDOWN_RETRIES {
            return true;
        }
        match self.shutdown_start {
            Some(start) => Instant::now().duration_since(start) >= ms(ACTIVE_RETRY_TIMEOUT_MS),
            None => false,
        }
    }

    /// Timestamp of the oldest state still unacknowledged — i.e. the last time
    /// we had proof of end-to-end connectivity.
    pub fn sent_state_acked_timestamp(&self) -> Instant {
        self.sent_states[0].timestamp
    }

    pub fn sent_state_acked(&self) -> u64 {
        self.sent_states[0].num
    }

    pub fn sent_state_last(&self) -> u64 {
        self.sent_states.last().unwrap().num
    }

    /// Number of states held in the history (for tests).
    pub fn sent_states_len(&self) -> usize {
        self.sent_states.len()
    }

    /// Start from what is known, and give the benefit of the doubt to
    /// unacknowledged states transmitted recently enough ago.
    ///
    /// When a packet is lost this walks *back*, so the next diff is computed
    /// against an older state and re-includes whatever the lost packet carried.
    fn update_assumed_receiver_state(&mut self, now: Instant, timeout: Duration) {
        self.assumed_receiver_state = 0;
        let horizon = timeout + ms(ACK_DELAY_MS);
        for i in 1..self.sent_states.len() {
            if now.saturating_duration_since(self.sent_states[i].timestamp) < horizon {
                self.assumed_receiver_state = i;
            } else {
                return;
            }
        }
    }

    /// Cut out the common prefix of all states.
    ///
    /// Only meaningful for queue-shaped states; for a terminal `subtract` is a
    /// no-op and the whole pass is skipped, since running it would mean
    /// cloning a framebuffer on every tick to satisfy the borrow checker.
    fn rationalize_states(&mut self) {
        if !S::HAS_PREFIX {
            return;
        }
        let known = self.sent_states[0].state.clone();
        self.current_state.subtract(&known);
        for sent in self.sent_states.iter_mut() {
            sent.state.subtract(&known);
        }
    }

    /// Recompute the send and ack deadlines. Mosh's `calculate_timers`.
    fn calculate_timers(&mut self, now: Instant, timeout: Duration, srtt_ms: u64) {
        self.update_assumed_receiver_state(now, timeout);
        self.rationalize_states();

        if self.pending_data_ack && self.next_ack_time > now + ms(ACK_DELAY_MS) {
            self.next_ack_time = now + ms(ACK_DELAY_MS);
        }

        let interval = self.send_interval(srtt_ms);
        let last = self.sent_states.last().unwrap();
        let last_timestamp = last.timestamp;
        let active = self
            .last_heard
            .is_some_and(|h| h + ms(ACTIVE_RETRY_TIMEOUT_MS) > now);

        if !self.current_state.state_equal(&last.state) {
            // We have something new to say.
            let clock = *self.mindelay_clock.get_or_insert(now);
            self.next_send_time = Some((clock + self.send_mindelay).max(last_timestamp + interval));
        } else if !self
            .current_state
            .state_equal(&self.sent_states[self.assumed_receiver_state].state)
            && active
        {
            // Already sent, but the receiver may not have it: retransmit at
            // the frame rate.
            let mut t = last_timestamp + interval;
            if let Some(clock) = self.mindelay_clock {
                t = t.max(clock + self.send_mindelay);
            }
            self.next_send_time = Some(t);
        } else if !self.current_state.state_equal(&self.sent_states[0].state) && active {
            // Sent and plausibly received, but never acknowledged: probe once
            // per timeout.
            self.next_send_time = Some(last_timestamp + timeout + ms(ACK_DELAY_MS));
        } else {
            self.next_send_time = None;
        }

        if self.shutdown_in_progress || self.ack_num == SHUTDOWN_NUM {
            self.next_ack_time = last_timestamp + interval;
        }
    }

    /// Milliseconds until the next event this sender needs to act on.
    pub fn wait_time_ms(
        &mut self,
        now: Instant,
        timeout: Duration,
        srtt_ms: u64,
        has_remote_addr: bool,
    ) -> u64 {
        self.calculate_timers(now, timeout, srtt_ms);

        if !has_remote_addr {
            return u64::MAX;
        }

        let mut next = self.next_ack_time;
        if let Some(send) = self.next_send_time {
            next = next.min(send);
        }
        next.saturating_duration_since(now).as_millis() as u64
    }

    /// Decide what, if anything, to put on the wire now. Mosh's `tick`.
    ///
    /// Returns the instruction the caller should send; all sender state has
    /// already been advanced as though the send succeeded, which is what mosh
    /// does too ("probably sent").
    pub fn tick(
        &mut self,
        now: Instant,
        timeout: Duration,
        srtt_ms: u64,
        has_remote_addr: bool,
    ) -> Option<TransportInstruction> {
        self.calculate_timers(now, timeout, srtt_ms);

        if !has_remote_addr {
            return None;
        }

        let send_due = self.next_send_time.is_some_and(|t| now >= t);
        let ack_due = now >= self.next_ack_time;
        if !send_due && !ack_due {
            return None;
        }

        let mut diff = self
            .current_state
            .diff_from(&self.sent_states[self.assumed_receiver_state].state);
        self.attempt_prospective_resend_optimization(&mut diff);

        if diff.is_empty() {
            let mut inst = None;
            if ack_due {
                inst = Some(self.send_empty_ack(now));
                self.mindelay_clock = None;
            }
            if send_due {
                self.next_send_time = None;
                self.mindelay_clock = None;
            }
            inst
        } else {
            let inst = self.send_to_receiver(diff, now);
            self.mindelay_clock = None;
            Some(inst)
        }
    }

    fn send_empty_ack(&mut self, now: Instant) -> TransportInstruction {
        let new_num = if self.shutdown_in_progress {
            SHUTDOWN_NUM
        } else {
            self.sent_state_last().wrapping_add(1)
        };

        let state = self.current_state.clone();
        self.add_sent_state(now, new_num, state);
        let inst = self.build_instruction(Vec::new(), new_num);

        self.next_ack_time = now + ms(ACK_INTERVAL_MS);
        self.next_send_time = None;
        inst
    }

    fn send_to_receiver(&mut self, diff: Vec<u8>, now: Instant) -> TransportInstruction {
        let last_num = self.sent_state_last();
        let mut new_num = if self
            .current_state
            .state_equal(&self.sent_states.last().unwrap().state)
        {
            last_num // previously sent
        } else {
            last_num.wrapping_add(1) // new state
        };
        if self.shutdown_in_progress {
            new_num = SHUTDOWN_NUM;
        }

        if new_num == last_num {
            self.sent_states.last_mut().unwrap().timestamp = now;
        } else {
            let state = self.current_state.clone();
            self.add_sent_state(now, new_num, state);
        }

        // `old_num` must be the assumed receiver state, not the state number
        // we are sending: it names the state the diff is *from*.
        let inst = self.build_instruction(diff, new_num);

        self.assumed_receiver_state = self.sent_states.len() - 1;
        self.next_ack_time = now + ms(ACK_INTERVAL_MS);
        self.next_send_time = None;
        inst
    }

    fn add_sent_state(&mut self, timestamp: Instant, num: u64, state: S) {
        self.sent_states
            .push(TimestampedState::new(timestamp, num, state));
        if self.sent_states.len() > SENT_STATES_MAX {
            // Drop from the middle, so both the acknowledged floor and the
            // recent history survive.
            let victim = self.sent_states.len() - 16;
            self.sent_states.remove(victim);
            if self.assumed_receiver_state >= victim {
                self.assumed_receiver_state = self.assumed_receiver_state.saturating_sub(1);
            }
        }
    }

    fn build_instruction(&mut self, diff: Vec<u8>, new_num: u64) -> TransportInstruction {
        if new_num == SHUTDOWN_NUM {
            self.shutdown_tries += 1;
        }
        self.pending_data_ack = false;

        TransportInstruction {
            protocol_version: Some(MORSH_PROTOCOL_VERSION),
            old_num: Some(self.sent_states[self.assumed_receiver_state].num),
            new_num: Some(new_num),
            ack_num: Some(self.ack_num),
            throwaway_num: Some(self.sent_states[0].num),
            diff: Some(diff),
            chaff: Some(make_chaff()),
        }
    }

    /// Cull states the receiver has confirmed.
    pub fn process_acknowledgment_through(&mut self, ack_num: u64) {
        // Ignore an ack for a state we have already culled.
        if !self.sent_states.iter().any(|s| s.num == ack_num) {
            return;
        }
        let assumed_num = self.sent_states[self.assumed_receiver_state].num;
        self.sent_states.retain(|s| s.num >= ack_num);
        debug_assert!(!self.sent_states.is_empty());
        self.assumed_receiver_state = self
            .sent_states
            .iter()
            .position(|s| s.num == assumed_num)
            .unwrap_or(0);
    }

    /// Consider diffing against the known-acknowledged state instead, when
    /// doing so costs little and removes all doubt about what the peer holds.
    fn attempt_prospective_resend_optimization(&mut self, proposed_diff: &mut Vec<u8>) {
        if self.assumed_receiver_state == 0 {
            return;
        }

        let resend_diff = self.current_state.diff_from(&self.sent_states[0].state);

        if resend_diff.len() <= proposed_diff.len()
            || (resend_diff.len() < 1000
                && resend_diff.len().saturating_sub(proposed_diff.len()) < 100)
        {
            self.assumed_receiver_state = 0;
            *proposed_diff = resend_diff;
        }
    }
}

/// Both halves of the protocol plus the connection they run over.
pub struct Transport<M: State, R: State> {
    connection: Connection,
    pub sender: TransportSender<M>,
    received_states: Vec<TimestampedState<R>>,
    receiver_quench_timer: Option<Instant>,
    /// The remote state as of the last `get_remote_diff` call.
    last_receiver_state: R,
}

/// What `Transport::recv` did with a datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvOutcome {
    /// A fragment was consumed but the instruction is not complete yet.
    Incomplete,
    /// The instruction was a duplicate, referenced a state we no longer hold,
    /// or was dropped because the receive queue is full.
    Ignored,
    /// A state arrived out of order and was filed behind a newer one, so the
    /// newest remote state is unchanged.
    OutOfOrderState { num: u64 },
    /// A new newest remote state was accepted.
    NewState { num: u64 },
}

impl<M: State, R: State> Transport<M, R> {
    pub fn new_client(connection: Connection, initial_state: M, initial_remote: R) -> Self {
        Self::new(connection, initial_state, initial_remote)
    }

    pub fn new_server(connection: Connection, initial_state: M, initial_remote: R) -> Self {
        Self::new(connection, initial_state, initial_remote)
    }

    fn new(connection: Connection, initial_state: M, initial_remote: R) -> Self {
        Self {
            connection,
            sender: TransportSender::new(initial_state),
            received_states: vec![TimestampedState::new(
                Instant::now(),
                0,
                initial_remote.clone(),
            )],
            receiver_quench_timer: None,
            last_receiver_state: initial_remote,
        }
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }

    pub fn has_remote_addr(&self) -> bool {
        self.connection.has_remote()
    }

    /// Milliseconds until this transport next needs to be ticked.
    pub fn wait_time_ms(&mut self) -> u64 {
        let now = Instant::now();
        let timeout = self.connection.timeout();
        let srtt = self.connection.srtt_ms();
        let has_remote = self.connection.has_remote();
        self.sender.wait_time_ms(now, timeout, srtt, has_remote)
    }

    /// Send a diff or an empty ack if one is due.
    pub async fn tick(&mut self) -> Result<(), String> {
        let now = Instant::now();
        let timeout = self.connection.timeout();
        let srtt = self.connection.srtt_ms();
        let has_remote = self.connection.has_remote();

        let Some(inst) = self.sender.tick(now, timeout, srtt, has_remote) else {
            return Ok(());
        };
        tracing::debug!(
            "SEND [{}=>{}] ack={} throwaway={} len={}",
            inst.old_num.unwrap_or(0),
            inst.new_num.unwrap_or(0),
            inst.ack_num.unwrap_or(0),
            inst.throwaway_num.unwrap_or(0),
            inst.diff.as_ref().map(|d| d.len()).unwrap_or(0),
        );
        self.connection.send(&inst).await
    }

    /// Receive and process one datagram.
    ///
    /// Cancel-safe. The only await is [`Connection::recv_raw`]; everything
    /// that mutates receiver state runs synchronously afterwards, so dropping
    /// this future in a `select!` cannot lose a packet that has already been
    /// taken off the socket.
    pub async fn recv(&mut self) -> Result<RecvOutcome, String> {
        let (data, peer) = self.connection.recv_raw().await?;
        self.process_datagram(&data, peer)
    }

    /// The synchronous half of [`Transport::recv`], split out so tests can
    /// drive the protocol without sockets.
    pub fn process_datagram(&mut self, data: &[u8], peer: std::net::SocketAddr) -> Result<RecvOutcome, String> {
        let Some(inst) = self.connection.process_incoming(data, peer)? else {
            return Ok(RecvOutcome::Incomplete);
        };
        self.process_instruction(inst)
    }

    fn process_instruction(&mut self, inst: TransportInstruction) -> Result<RecvOutcome, String> {
        if inst.protocol_version != Some(MORSH_PROTOCOL_VERSION) {
            return Err(format!(
                "mosh protocol version mismatch (got {:?}, expected {MORSH_PROTOCOL_VERSION})",
                inst.protocol_version
            ));
        }

        let new_num = inst.new_num.unwrap_or(0);
        let old_num = inst.old_num.unwrap_or(0);
        let ack_num = inst.ack_num.unwrap_or(0);
        let throwaway_num = inst.throwaway_num.unwrap_or(0);
        let diff = inst.diff.unwrap_or_default();

        self.sender.process_acknowledgment_through(ack_num);

        // Inform the network layer of end-to-end connectivity.
        let acked_at = self.sender.sent_state_acked_timestamp();
        self.connection.set_last_roundtrip_success(acked_at);

        // First, make sure we don't already have the new state.
        if self.received_states.iter().any(|s| s.num == new_num) {
            tracing::debug!("Ignoring duplicate state {new_num}");
            return Ok(RecvOutcome::Ignored);
        }

        // Now, make sure we do have the old state. This is security-sensitive
        // and part of how we enforce idempotency.
        if !self.received_states.iter().any(|s| s.num == old_num) {
            tracing::debug!(
                "Ignoring out-of-order packet; reference state {old_num} discarded or not yet received"
            );
            return Ok(RecvOutcome::Ignored);
        }

        // Culling shifts the history, so the reference is re-found afterwards
        // rather than held across the call.
        self.process_throwaway_until(throwaway_num);
        let Some(reference_index) = self.received_states.iter().position(|s| s.num == old_num)
        else {
            tracing::debug!("Reference state {old_num} was culled by throwaway_num {throwaway_num}");
            return Ok(RecvOutcome::Ignored);
        };

        // Do not accept state if our queue is full. Better than dropping from
        // the middle, because we must not ack a state and then discard it.
        if self.received_states.len() > RECEIVED_STATES_MAX {
            let now = Instant::now();
            match self.receiver_quench_timer {
                Some(until) if now < until => {
                    tracing::warn!("Receiver queue full, discarding state {new_num}");
                    return Ok(RecvOutcome::Ignored);
                }
                _ => self.receiver_quench_timer = Some(now + ms(RECEIVER_QUENCH_MS)),
            }
        }

        // Apply the diff to the *referenced* state, not to the current one.
        let mut new_state = TimestampedState::new(
            Instant::now(),
            new_num,
            self.received_states[reference_index].state.clone(),
        );
        // The copy is about to become the state the reference was heading for,
        // so it, not the reference, is the one that should hold any live
        // resource backing it.
        new_state
            .state
            .adopt_from(&mut self.received_states[reference_index].state);
        if !diff.is_empty() {
            new_state.state.apply_string(&diff)?;
        }

        let timestamp = new_state.timestamp;

        // Insert in sorted order.
        match self.received_states.iter().position(|s| s.num > new_num) {
            Some(pos) => {
                tracing::debug!("Received OUT-OF-ORDER state {new_num} [ack {ack_num}]");
                self.received_states.insert(pos, new_state);
                return Ok(RecvOutcome::OutOfOrderState { num: new_num });
            }
            None => {
                tracing::debug!("Received state {new_num} [from {old_num}, ack {ack_num}]");
                self.received_states.push(new_state);
            }
        }

        self.sender
            .set_ack_num(self.received_states.last().unwrap().num);
        self.sender.remote_heard(timestamp);
        if !diff.is_empty() {
            self.sender.set_data_ack();
        }

        Ok(RecvOutcome::NewState { num: new_num })
    }

    /// The sender uses `throwaway_num` to tell us the earliest received state
    /// it still needs us to keep around.
    fn process_throwaway_until(&mut self, throwaway_num: u64) {
        // Mosh asserts the history is never emptied. Clamping the cutoff to
        // the newest state we hold guarantees that without an abort, even if
        // the peer sends a throwaway_num ahead of anything we have.
        let Some(newest) = self.received_states.last().map(|s| s.num) else {
            return;
        };
        let cutoff = throwaway_num.min(newest);
        self.received_states.retain(|s| s.num >= cutoff);
    }

    /// Number of remote states held (for tests).
    pub fn received_states_len(&self) -> usize {
        self.received_states.len()
    }

    pub fn get_remote_state_num(&self) -> u64 {
        self.received_states.last().unwrap().num
    }

    pub fn get_latest_remote_state(&self) -> &TimestampedState<R> {
        self.received_states.last().unwrap()
    }

    /// Diff between the remote state we last handed out and the current one,
    /// then rationalize the received-state history.
    pub fn get_remote_diff(&mut self) -> Vec<u8> {
        let ret = self
            .received_states
            .last()
            .unwrap()
            .state
            .diff_from(&self.last_receiver_state);

        if R::HAS_PREFIX {
            let oldest = self.received_states[0].state.clone();
            for s in self.received_states.iter_mut() {
                s.state.subtract(&oldest);
            }
        }

        self.last_receiver_state = self.received_states.last().unwrap().state.clone();
        ret
    }

    pub fn start_shutdown(&mut self) {
        self.sender.start_shutdown();
    }

    pub fn shutdown_in_progress(&self) -> bool {
        self.sender.shutdown_in_progress()
    }

    pub fn shutdown_acknowledged(&self) -> bool {
        self.sender.shutdown_acknowledged()
    }

    pub fn shutdown_ack_timed_out(&self) -> bool {
        self.sender.shutdown_ack_timed_out()
    }

    /// The peer requested shutdown and we have already sent one ack for it.
    pub fn counterparty_shutdown_ack_sent(&self) -> bool {
        self.connection.fragmenter_last_ack_sent() == SHUTDOWN_NUM
    }

    pub fn get_current_state(&self) -> &M {
        self.sender.current_state()
    }

    pub fn get_current_state_mut(&mut self) -> &mut M {
        self.sender.current_state_mut()
    }

    pub fn set_current_state(&mut self, state: M) {
        self.sender.set_current_state(state);
    }

    /// Adaptive send interval in milliseconds, for the prediction engine.
    pub fn send_interval_ms(&self) -> u64 {
        self.sender
            .send_interval(self.connection.srtt_ms())
            .as_millis() as u64
    }

    pub fn should_hop_port(&self, now: Instant) -> bool {
        self.connection.should_hop_port(now)
    }

    pub async fn hop_port(&mut self) -> Result<(), String> {
        self.connection.hop_port().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial append-only state, standing in for `UserStream`.
    #[derive(Clone, Debug, Default, PartialEq)]
    struct Bytes(Vec<u8>);

    impl Bytes {
        fn new(s: &[u8]) -> Self {
            Self(s.to_vec())
        }
    }

    fn common_prefix(a: &[u8], b: &[u8]) -> usize {
        a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
    }

    impl State for Bytes {
        const HAS_PREFIX: bool = true;

        fn diff_from(&self, existing: &Self) -> Vec<u8> {
            self.0[common_prefix(&self.0, &existing.0)..].to_vec()
        }

        fn apply_string(&mut self, diff: &[u8]) -> Result<(), String> {
            self.0.extend_from_slice(diff);
            Ok(())
        }

        fn subtract(&mut self, prefix: &Self) {
            let n = common_prefix(&self.0, &prefix.0);
            self.0.drain(..n);
        }

        fn state_equal(&self, other: &Self) -> bool {
            self.0 == other.0
        }
    }

    const TIMEOUT: Duration = Duration::from_millis(50);
    const SRTT: u64 = 50;

    fn sender() -> TransportSender<Bytes> {
        TransportSender::new(Bytes::default())
    }

    /// Drive the sender forward in 1 ms steps until it produces a packet.
    ///
    /// Mosh's sender is timer-driven: after new input it waits `SEND_MINDELAY`
    /// to collect more, so a single `tick` right after the input never sends.
    fn tick_until(
        s: &mut TransportSender<Bytes>,
        from: Instant,
        budget_ms: u64,
    ) -> Option<(TransportInstruction, Instant)> {
        for step in 0..=budget_ms {
            let at = from + ms(step);
            if let Some(inst) = s.tick(at, TIMEOUT, SRTT, true) {
                return Some((inst, at));
            }
        }
        None
    }

    #[test]
    fn first_tick_sends_the_whole_state() {
        let mut s = sender();
        s.current_state_mut().0.extend_from_slice(b"hello");

        let (inst, _) = tick_until(&mut s, Instant::now(), 100).expect("should send");
        assert_eq!(inst.old_num, Some(0));
        assert_eq!(inst.new_num, Some(1));
        assert_eq!(inst.diff, Some(b"hello".to_vec()));
        assert_eq!(inst.throwaway_num, Some(0));
        assert_eq!(inst.protocol_version, Some(MORSH_PROTOCOL_VERSION));
    }

    /// `old_num` names the state the diff is *from*, not the state number we
    /// are sending.
    #[test]
    fn old_num_is_the_assumed_receiver_state() {
        let mut s = sender();
        let t0 = Instant::now();
        s.current_state_mut().0.extend_from_slice(b"a");
        let (first, sent_at) = tick_until(&mut s, t0, 100).unwrap();
        assert_eq!((first.old_num, first.new_num), (Some(0), Some(1)));

        s.process_acknowledgment_through(1);
        s.remote_heard(sent_at + ms(5));
        s.current_state_mut().0.extend_from_slice(b"b");

        let (second, _) = tick_until(&mut s, sent_at + ms(10), 200).unwrap();
        assert_eq!((second.old_num, second.new_num), (Some(1), Some(2)));
        assert_eq!(second.diff, Some(b"b".to_vec()));
    }

    /// The core loss-recovery mechanism. An unacknowledged state ages out of
    /// the "assumed received" window, the assumption regresses to the last
    /// acknowledged state, and the next diff re-includes what the lost packet
    /// carried. No packet is explicitly retransmitted.
    #[test]
    fn assumed_receiver_state_regresses_when_unacknowledged() {
        let mut s = sender();
        let t0 = Instant::now();
        s.remote_heard(t0);

        s.current_state_mut().0.extend_from_slice(b"aaa");
        let (first, sent_at) = tick_until(&mut s, t0, 100).unwrap();
        assert_eq!(first.diff, Some(b"aaa".to_vec()));

        // That packet is lost: no acknowledgment ever arrives. More input
        // shows up after the assumption window has expired.
        let later = sent_at + TIMEOUT + ms(ACK_DELAY_MS) + ms(5);
        s.remote_heard(later);
        s.current_state_mut().0.extend_from_slice(b"bbb");

        let (second, _) = tick_until(&mut s, later, 200).unwrap();
        assert_eq!(second.old_num, Some(0), "should diff from the acked state");
        assert_eq!(
            second.diff,
            Some(b"aaabbb".to_vec()),
            "the retransmission must re-include the lost bytes"
        );
    }

    #[test]
    fn acknowledgment_culls_history() {
        let mut s = sender();
        let mut now = Instant::now();
        s.remote_heard(now);
        for i in 0..5u8 {
            s.current_state_mut().0.push(b'a' + i);
            let (_, at) = tick_until(&mut s, now, 500).expect("send");
            now = at + ms(1);
            s.remote_heard(now);
        }
        assert_eq!(s.sent_states_len(), 6);
        assert_eq!(s.sent_state_last(), 5);

        s.process_acknowledgment_through(3);
        assert_eq!(s.sent_state_acked(), 3);
        assert_eq!(s.sent_states_len(), 3);
    }

    #[test]
    fn acknowledgment_of_a_culled_state_is_ignored() {
        let mut s = sender();
        s.current_state_mut().0.push(b'x');
        tick_until(&mut s, Instant::now(), 100).unwrap();
        let before = s.sent_states_len();
        s.process_acknowledgment_through(999);
        assert_eq!(s.sent_states_len(), before);
        assert_eq!(s.sent_state_acked(), 0);
    }

    #[test]
    fn sent_state_history_is_bounded() {
        let mut s = sender();
        let mut now = Instant::now();
        s.remote_heard(now);
        for i in 0..200u32 {
            s.current_state_mut().0.push((i % 251) as u8);
            let Some((_, at)) = tick_until(&mut s, now, 500) else {
                panic!("stopped sending at iteration {i}");
            };
            now = at + ms(1);
            s.remote_heard(now);
        }
        assert!(
            s.sent_states_len() <= SENT_STATES_MAX,
            "history grew to {}",
            s.sent_states_len()
        );
    }

    #[test]
    fn nothing_is_sent_without_a_remote_address() {
        let mut s = sender();
        s.current_state_mut().0.push(b'x');
        for step in 0..500 {
            assert!(s
                .tick(Instant::now() + ms(step), TIMEOUT, SRTT, false)
                .is_none());
        }
    }

    #[test]
    fn empty_ack_is_sent_once_the_ack_interval_elapses() {
        let mut s = sender();
        let t0 = Instant::now();
        s.current_state_mut().0.push(b'x');
        let (_, sent_at) = tick_until(&mut s, t0, 100).unwrap();
        s.process_acknowledgment_through(1);
        s.remote_heard(sent_at);

        // Nothing new to say, and the ack interval has not elapsed.
        assert!(s.tick(sent_at + ms(100), TIMEOUT, SRTT, true).is_none());

        let inst = s
            .tick(sent_at + ms(ACK_INTERVAL_MS + 1), TIMEOUT, SRTT, true)
            .expect("empty ack");
        assert_eq!(inst.diff, Some(Vec::new()));
        assert_eq!(inst.new_num, Some(2));
    }

    #[test]
    fn a_state_that_did_not_change_reuses_its_number() {
        let mut s = sender();
        let t0 = Instant::now();
        s.remote_heard(t0);
        s.current_state_mut().0.extend_from_slice(b"abc");
        let (first, sent_at) = tick_until(&mut s, t0, 100).unwrap();
        assert_eq!(first.new_num, Some(1));

        // No new input, no ack: the sender probes again with the same state.
        let later = sent_at + TIMEOUT + ms(ACK_DELAY_MS) + ms(5);
        s.remote_heard(later);
        let (second, _) = tick_until(&mut s, later, 500).unwrap();
        assert_eq!(second.new_num, Some(1), "unchanged state keeps its number");
        assert_eq!(second.old_num, Some(0));
        assert_eq!(second.diff, Some(b"abc".to_vec()));
    }

    #[test]
    fn shutdown_uses_the_sentinel_state_number() {
        let mut s = sender();
        let t0 = Instant::now();
        s.current_state_mut().0.push(b'x');
        let (_, sent_at) = tick_until(&mut s, t0, 100).unwrap();

        s.start_shutdown();
        assert!(s.shutdown_in_progress());
        assert!(!s.shutdown_acknowledged());

        let (inst, _) = tick_until(&mut s, sent_at + ms(1), 500).expect("shutdown packet");
        assert_eq!(inst.new_num, Some(SHUTDOWN_NUM));

        s.process_acknowledgment_through(SHUTDOWN_NUM);
        assert!(s.shutdown_acknowledged());
    }

    #[test]
    fn shutdown_gives_up_after_enough_tries() {
        let mut s = sender();
        s.start_shutdown();
        let mut now = Instant::now();
        for _ in 0..SHUTDOWN_RETRIES + 2 {
            now += ms(ACK_INTERVAL_MS + 1);
            s.tick(now, TIMEOUT, SRTT, true);
        }
        assert!(s.shutdown_ack_timed_out());
    }

    #[test]
    fn chaff_is_attached_to_every_instruction() {
        let mut s = sender();
        s.current_state_mut().0.push(b'x');
        let (inst, _) = tick_until(&mut s, Instant::now(), 100).unwrap();
        let chaff = inst.chaff.expect("chaff present");
        assert!(chaff.len() <= CHAFF_MAX);
    }

    #[test]
    fn send_interval_tracks_rtt_within_bounds() {
        let s = sender();
        assert_eq!(s.send_interval(10), ms(SEND_INTERVAL_MIN_MS));
        assert_eq!(s.send_interval(100), ms(50));
        assert_eq!(s.send_interval(5000), ms(SEND_INTERVAL_MAX_MS));
    }

    // ---- receiver-side guards -------------------------------------------

    /// Build a `Transport` whose connection is never used, so the receiver
    /// logic can be driven directly through `process_instruction`.
    async fn receiver() -> Transport<Bytes, Bytes> {
        let session = morsh_crypto::Session::new(*morsh_crypto::Base64Key::random().data());
        let conn = Connection::new_client(session).await.unwrap();
        Transport::new_client(conn, Bytes::default(), Bytes::default())
    }

    fn inst(old: u64, new: u64, diff: &[u8]) -> TransportInstruction {
        TransportInstruction {
            protocol_version: Some(MORSH_PROTOCOL_VERSION),
            old_num: Some(old),
            new_num: Some(new),
            ack_num: Some(0),
            throwaway_num: Some(0),
            diff: Some(diff.to_vec()),
            chaff: None,
        }
    }

    #[tokio::test]
    async fn duplicate_new_num_is_suppressed() {
        let mut t = receiver().await;
        assert_eq!(
            t.process_instruction(inst(0, 1, b"ab")).unwrap(),
            RecvOutcome::NewState { num: 1 }
        );
        assert_eq!(
            t.process_instruction(inst(0, 1, b"ab")).unwrap(),
            RecvOutcome::Ignored,
            "a retransmission must not be applied twice"
        );
        assert_eq!(t.get_latest_remote_state().state, Bytes::new(b"ab"));
    }

    /// The guard mosh calls security-sensitive: a diff whose reference state
    /// we do not hold is dropped rather than applied to whatever we do hold.
    #[tokio::test]
    async fn unknown_old_num_is_rejected() {
        let mut t = receiver().await;
        assert_eq!(
            t.process_instruction(inst(7, 8, b"junk")).unwrap(),
            RecvOutcome::Ignored
        );
        assert_eq!(t.get_remote_state_num(), 0);
        assert_eq!(t.get_latest_remote_state().state, Bytes::default());
    }

    #[tokio::test]
    async fn diff_is_applied_to_the_referenced_state_not_the_newest() {
        let mut t = receiver().await;
        t.process_instruction(inst(0, 1, b"a")).unwrap();
        t.process_instruction(inst(1, 2, b"b")).unwrap();
        assert_eq!(t.get_latest_remote_state().state, Bytes::new(b"ab"));

        // A packet that was in flight before state 2, referencing state 1.
        assert_eq!(
            t.process_instruction(inst(1, 3, b"c")).unwrap(),
            RecvOutcome::NewState { num: 3 }
        );
        assert_eq!(
            t.get_latest_remote_state().state,
            Bytes::new(b"ac"),
            "the diff must be applied to state 1, not to state 2"
        );
    }

    #[tokio::test]
    async fn out_of_order_state_is_filed_behind_the_newest() {
        let mut t = receiver().await;
        t.process_instruction(inst(0, 1, b"a")).unwrap();
        t.process_instruction(inst(1, 5, b"e")).unwrap();
        assert_eq!(t.get_remote_state_num(), 5);

        // State 3 shows up late, referencing state 1.
        assert_eq!(
            t.process_instruction(inst(1, 3, b"c")).unwrap(),
            RecvOutcome::OutOfOrderState { num: 3 }
        );
        assert_eq!(
            t.get_remote_state_num(),
            5,
            "a late state must not become the newest"
        );
        assert_eq!(t.get_latest_remote_state().state, Bytes::new(b"ae"));
    }

    #[tokio::test]
    async fn protocol_version_mismatch_is_rejected() {
        let mut t = receiver().await;
        let mut i = inst(0, 1, b"a");
        i.protocol_version = Some(MORSH_PROTOCOL_VERSION + 1);
        assert!(t.process_instruction(i).is_err());
        assert_eq!(t.get_remote_state_num(), 0);
    }

    #[tokio::test]
    async fn throwaway_num_culls_the_received_history() {
        let mut t = receiver().await;
        for n in 1..=5u64 {
            let mut i = inst(n - 1, n, b"x");
            i.throwaway_num = Some(0);
            t.process_instruction(i).unwrap();
        }
        assert_eq!(t.received_states_len(), 6);

        let mut i = inst(5, 6, b"x");
        i.throwaway_num = Some(4);
        t.process_instruction(i).unwrap();
        assert_eq!(t.received_states_len(), 3, "states below 4 should be gone");

        // And the history is never emptied, even by an absurd throwaway_num.
        let mut i = inst(6, 7, b"x");
        i.throwaway_num = Some(u64::MAX - 1);
        t.process_instruction(i).unwrap();
        assert!(t.received_states_len() >= 1);
    }

    #[tokio::test]
    async fn get_remote_diff_returns_only_what_is_new() {
        let mut t = receiver().await;
        t.process_instruction(inst(0, 1, b"ab")).unwrap();
        assert_eq!(t.get_remote_diff(), b"ab".to_vec());
        t.process_instruction(inst(1, 2, b"cd")).unwrap();
        assert_eq!(t.get_remote_diff(), b"cd".to_vec());
        assert_eq!(t.get_remote_diff(), Vec::<u8>::new());
    }

    #[tokio::test]
    async fn acknowledgment_from_the_peer_reaches_the_sender() {
        let mut t = receiver().await;
        t.sender.current_state_mut().0.extend_from_slice(b"typed");
        tick_until(&mut t.sender, Instant::now(), 100).unwrap();
        assert_eq!(t.sender.sent_state_acked(), 0);

        let mut i = inst(0, 1, b"");
        i.ack_num = Some(1);
        t.process_instruction(i).unwrap();
        assert_eq!(t.sender.sent_state_acked(), 1);
    }
}
