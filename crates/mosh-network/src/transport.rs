use crate::constants::*;
use crate::connection::Connection;
use crate::fragment::add_chaff;
use mosh_crypto::Nonce;
use mosh_proto::transport::Instruction as TransportInstruction;
use std::time::{Duration, Instant};

/// State of the transport sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendState {
    /// Normal sending at adaptive intervals.
    Active,
    /// Waiting for initial handshake.
    Pending,
    /// Shutting down — sending shutdown markers.
    Shutdown,
    /// Fully shut down.
    Done,
}

/// Tracks the sender side of a state synchronization session.
///
/// Manages nonce sequencing, ack tracking, and send timing for one
/// direction of the transport.
pub struct TransportSender {
    /// Current state number (monotonically increasing).
    state_num: u64,
    /// What the remote has acknowledged.
    acked_state_num: u64,
    /// Number of throwaway packets received.
    throwaway_num: u64,
    /// Nonce sequence counter.
    nonce_seq: u64,
    /// Whether this is the client side (sets high bit on nonces).
    client_side: bool,
    /// Last time we sent a packet.
    last_send_time: Instant,
    /// Adaptive send interval based on RTT.
    send_interval: Duration,
    /// Current send state.
    state: SendState,
    /// Shutdown retry counter.
    shutdown_retries: u32,
}

/// Tracks the receiver side of a state synchronization session.
pub struct TransportReceiver {
    /// Current state number we've applied.
    state_num: u64,
    /// What we've acknowledged to the remote.
    acked_state_num: u64,
    /// The remote's last state number (from their new_num field).
    remote_state_num: u64,
    /// Nonce sequence counter for received packets.
    nonce_seq: u64,
    /// Whether this is the client side.
    client_side: bool,
    /// Last time we received a packet.
    last_recv_time: Instant,
    /// Shutdown state.
    shutdown_received: bool,
    /// Whether we have a pending delayed ACK to send.
    pending_ack: bool,
    /// When the delayed ACK should fire (after ACK_DELAY_MS from last receive).
    ack_deadline: Option<Instant>,
    /// Last time we sent an ACK (for periodic ACK interval).
    last_ack_time: Instant,
}

/// A received state diff with metadata.
#[derive(Debug)]
pub struct ReceivedDiff {
    /// The diff bytes (protobuf-encoded state).
    pub diff: Vec<u8>,
    /// Remote's view of our state number.
    pub old_num: u64,
    /// Remote's new state number.
    pub new_num: u64,
    /// Remote's ack of our state.
    pub ack_num: u64,
    /// Throwaway count.
    pub throwaway_num: u64,
}

/// Combined transport for bidirectional state synchronization.
///
/// Ties together Connection, nonce management, ack tracking, and send timing.
pub struct Transport {
    pub sender: TransportSender,
    pub receiver: TransportReceiver,
    connection: Connection,
}

impl TransportSender {
    pub fn new(client_side: bool) -> Self {
        let now = Instant::now();
        Self {
            state_num: 0,
            acked_state_num: 0,
            throwaway_num: 0,
            nonce_seq: 0,
            client_side,
            last_send_time: now - Duration::from_secs(1), // Allow immediate first send
            send_interval: Duration::from_millis(SEND_INTERVAL_MAX_MS),
            state: SendState::Pending,
            shutdown_retries: 0,
        }
    }

    /// Get the current state number.
    pub fn state_num(&self) -> u64 {
        self.state_num
    }

    /// Get the acked state number.
    pub fn acked_state_num(&self) -> u64 {
        self.acked_state_num
    }

    /// Get the current send state.
    pub fn state(&self) -> SendState {
        self.state
    }

    /// Set the send state.
    pub fn set_state(&mut self, state: SendState) {
        self.state = state;
    }

    /// Update the send interval based on measured RTT.
    pub fn update_send_interval(&mut self, rtt_ms: u64) {
        // Mosh formula: clamp to [SEND_INTERVAL_MIN_MS, SEND_INTERVAL_MAX_MS]
        // Prefer 2x RTT for smooth updates, but at least min, at most max
        let interval_ms = (rtt_ms * 2)
            .max(SEND_INTERVAL_MIN_MS)
            .min(SEND_INTERVAL_MAX_MS);
        self.send_interval = Duration::from_millis(interval_ms);
    }

    /// Check if it's time to send.
    pub fn should_send(&self, now: Instant) -> bool {
        match self.state {
            SendState::Active => now.duration_since(self.last_send_time) >= self.send_interval,
            SendState::Shutdown => {
                now.duration_since(self.last_send_time) >= Duration::from_millis(MIN_RTO_MS)
            }
            _ => false,
        }
    }

    /// Mark that we just sent a packet.
    pub fn record_send(&mut self, now: Instant) {
        self.last_send_time = now;
    }

    /// Increment state number (call after computing a new diff).
    pub fn advance_state(&mut self) {
        self.state_num += 1;
    }

    /// Record an ack from the remote.
    pub fn record_ack(&mut self, ack_num: u64) {
        if ack_num > self.acked_state_num {
            self.acked_state_num = ack_num;
        }
    }

    /// Record receiving a throwaway packet.
    pub fn record_throwaway(&mut self) {
        self.throwaway_num += 1;
    }

    /// Get the throwaway count.
    pub fn throwaway_num(&self) -> u64 {
        self.throwaway_num
    }

    /// Get the next nonce for sending.
    fn next_send_nonce(&mut self) -> Nonce {
        let seq = self.nonce_seq;
        self.nonce_seq += 1;
        let val = if self.client_side {
            (1u64 << 63) | seq
        } else {
            seq
        };
        Nonce::from_val(val)
    }

    /// Build a TransportInstruction for a state diff.
    pub fn build_instruction(
        &mut self,
        diff: Vec<u8>,
        ack_num: u64,
        throwaway_num: u64,
    ) -> TransportInstruction {
        let old_num = self.acked_state_num;
        let new_num = self.state_num;

        TransportInstruction {
            protocol_version: Some(MOSH_PROTOCOL_VERSION),
            old_num: Some(old_num),
            new_num: Some(new_num),
            ack_num: Some(ack_num),
            throwaway_num: Some(throwaway_num),
            diff: Some(diff),
            chaff: None,
        }
    }

    /// Build a shutdown instruction (no diff).
    pub fn build_shutdown_instruction(&mut self, ack_num: u64) -> TransportInstruction {
        TransportInstruction {
            protocol_version: Some(MOSH_PROTOCOL_VERSION),
            old_num: Some(self.state_num),
            new_num: Some(self.state_num),
            ack_num: Some(ack_num),
            throwaway_num: Some(self.throwaway_num),
            diff: None,
            chaff: None,
        }
    }
}

impl TransportReceiver {
    pub fn new(client_side: bool) -> Self {
        let now = Instant::now();
        Self {
            state_num: 0,
            acked_state_num: 0,
            remote_state_num: 0,
            nonce_seq: 0,
            client_side,
            last_recv_time: now,
            shutdown_received: false,
            pending_ack: false,
            ack_deadline: None,
            last_ack_time: now,
        }
    }

    /// Get the current state number.
    pub fn state_num(&self) -> u64 {
        self.state_num
    }

    /// Get the acked state number.
    pub fn acked_state_num(&self) -> u64 {
        self.acked_state_num
    }

    /// Get the remote's last state number.
    pub fn remote_state_num(&self) -> u64 {
        self.remote_state_num
    }

    /// Whether we've received a shutdown.
    pub fn shutdown_received(&self) -> bool {
        self.shutdown_received
    }

    /// Mark that we received a shutdown.
    pub fn set_shutdown_received(&mut self) {
        self.shutdown_received = true;
    }

    /// Record that we applied a new state.
    pub fn advance_state(&mut self) {
        self.state_num += 1;
    }

    /// Record that we sent an ack.
    pub fn record_ack_sent(&mut self, ack_num: u64) {
        if ack_num > self.acked_state_num {
            self.acked_state_num = ack_num;
        }
    }

    /// Record that we received a packet. Schedules a delayed ACK.
    pub fn record_recv(&mut self, now: Instant) {
        self.last_recv_time = now;
        // Schedule a delayed ACK if one isn't already pending
        if !self.pending_ack {
            self.pending_ack = true;
            self.ack_deadline = Some(now + Duration::from_millis(ACK_DELAY_MS));
        }
    }

    /// Check if we should send an ACK now.
    /// Returns true if the delayed ACK deadline has passed, or if the
    /// periodic ACK interval has elapsed.
    pub fn should_send_ack(&self, now: Instant) -> bool {
        // Delayed ACK: we received data and the deadline has passed
        if let Some(deadline) = self.ack_deadline {
            if now >= deadline {
                return true;
            }
        }
        // Periodic ACK: send empty ACKs at ACK_INTERVAL even without data
        now.duration_since(self.last_ack_time) >= Duration::from_millis(ACK_INTERVAL_MS)
    }

    /// Mark that we just sent an ACK. Clears the pending ACK state.
    pub fn ack_sent(&mut self, now: Instant) {
        self.pending_ack = false;
        self.ack_deadline = None;
        self.last_ack_time = now;
    }

    /// Get the next nonce for receiving.
    fn next_recv_nonce(&mut self) -> Nonce {
        let seq = self.nonce_seq;
        self.nonce_seq += 1;
        let val = if self.client_side {
            seq // Client receives server's nonces (no high bit)
        } else {
            (1u64 << 63) | seq // Server receives client's nonces (high bit set)
        };
        Nonce::from_val(val)
    }

    /// Parse a received TransportInstruction.
    pub fn parse_instruction(inst: &TransportInstruction) -> ReceivedDiff {
        ReceivedDiff {
            diff: inst.diff.clone().unwrap_or_default(),
            old_num: inst.old_num.unwrap_or(0),
            new_num: inst.new_num.unwrap_or(0),
            ack_num: inst.ack_num.unwrap_or(0),
            throwaway_num: inst.throwaway_num.unwrap_or(0),
        }
    }
}

impl Transport {
    /// Create a new transport (client side).
    pub fn new_client(connection: Connection) -> Self {
        Self {
            sender: TransportSender::new(true),
            receiver: TransportReceiver::new(true),
            connection,
        }
    }

    /// Create a new transport (server side).
    pub fn new_server(connection: Connection) -> Self {
        Self {
            sender: TransportSender::new(false),
            receiver: TransportReceiver::new(false),
            connection,
        }
    }

    /// Get a reference to the connection.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Get a mutable reference to the connection.
    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }

    /// Send a state diff. Clears any pending ACK since the diff piggybacks it.
    pub async fn send_diff(
        &mut self,
        diff: Vec<u8>,
        ack_num: u64,
        throwaway_num: u64,
    ) -> Result<(), String> {
        let mut inst = self.sender.build_instruction(diff, ack_num, throwaway_num);
        // Add chaff for traffic analysis resistance
        let chaff_byte = (self.sender.state_num() & 0xFF) as u8;
        add_chaff(&mut inst, chaff_byte);
        self.connection.send(&inst).await?;
        let now = Instant::now();
        self.sender.record_send(now);
        self.receiver.ack_sent(now);
        Ok(())
    }

    /// Send a shutdown marker.
    pub async fn send_shutdown(&mut self, ack_num: u64) -> Result<(), String> {
        let inst = self.sender.build_shutdown_instruction(ack_num);
        self.connection.send(&inst).await?;
        let now = Instant::now();
        self.sender.record_send(now);
        self.sender.shutdown_retries += 1;
        self.receiver.ack_sent(now);
        Ok(())
    }

    /// Receive a state diff (non-blocking).
    pub async fn recv_diff(&mut self) -> Result<Option<ReceivedDiff>, String> {
        match self.connection.recv().await? {
            Some(inst) => {
                self.receiver.record_recv(Instant::now());
                let diff = TransportReceiver::parse_instruction(&inst);

                // Track the remote's state number for acks
                if diff.new_num > self.receiver.remote_state_num {
                    self.receiver.remote_state_num = diff.new_num;
                }

                // Update our ack tracking
                self.receiver.record_ack_sent(diff.ack_num);

                // Check if this is a shutdown
                if diff.diff.is_empty() && diff.old_num == diff.new_num {
                    self.receiver.set_shutdown_received();
                }

                Ok(Some(diff))
            }
            None => Ok(None),
        }
    }

    /// Check if it's time to send.
    pub fn should_send(&self, now: Instant) -> bool {
        self.sender.should_send(now)
    }

    /// Check if we should send an empty ACK now.
    pub fn should_send_ack(&self, now: Instant) -> bool {
        self.receiver.should_send_ack(now)
    }

    /// Send an empty ACK (no diff). Used for delayed ACKs and periodic ACKs.
    pub async fn send_ack(&mut self, ack_num: u64) -> Result<(), String> {
        let inst = self.sender.build_instruction(Vec::new(), ack_num, self.sender.throwaway_num);
        self.connection.send(&inst).await?;
        let now = Instant::now();
        self.sender.record_send(now);
        self.receiver.ack_sent(now);
        Ok(())
    }

    /// Update send interval from RTT.
    pub fn update_send_interval(&mut self, rtt_ms: u64) {
        self.sender.update_send_interval(rtt_ms);
    }

    /// Check if we should hop ports.
    pub fn should_hop_port(&self, now: Instant) -> bool {
        self.connection.should_hop_port(now)
    }

    /// Hop to a new port.
    pub async fn hop_port(&mut self) -> Result<(), String> {
        self.connection.hop_port().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_state_tracking() {
        let mut sender = TransportSender::new(true);
        assert_eq!(sender.state_num(), 0);
        assert_eq!(sender.acked_state_num(), 0);

        sender.advance_state();
        assert_eq!(sender.state_num(), 1);

        sender.record_ack(5);
        assert_eq!(sender.acked_state_num(), 5);

        // Ack doesn't go backward
        sender.record_ack(3);
        assert_eq!(sender.acked_state_num(), 5);
    }

    #[test]
    fn sender_send_interval() {
        let mut sender = TransportSender::new(true);

        // Initial interval is max
        assert_eq!(sender.send_interval, Duration::from_millis(SEND_INTERVAL_MAX_MS));

        // Update based on 100ms RTT → 200ms interval
        sender.update_send_interval(100);
        assert_eq!(sender.send_interval, Duration::from_millis(200));

        // Update based on 10ms RTT → min interval
        sender.update_send_interval(10);
        assert_eq!(sender.send_interval, Duration::from_millis(SEND_INTERVAL_MIN_MS));

        // Update based on 500ms RTT → max interval
        sender.update_send_interval(500);
        assert_eq!(sender.send_interval, Duration::from_millis(SEND_INTERVAL_MAX_MS));
    }

    #[test]
    fn sender_should_send() {
        let mut sender = TransportSender::new(true);
        sender.set_state(SendState::Active);
        let now = Instant::now();

        // Should send immediately (last_send_time is ~1s in the past)
        assert!(sender.should_send(now));

        // After recording a send, should not send until interval passes
        sender.record_send(now);
        assert!(!sender.should_send(now + Duration::from_millis(50)));
        assert!(sender.should_send(now + Duration::from_millis(SEND_INTERVAL_MAX_MS + 1)));
    }

    #[test]
    fn sender_build_instruction() {
        let mut sender = TransportSender::new(true);
        sender.advance_state();
        sender.advance_state();

        let inst = sender.build_instruction(b"hello".to_vec(), 3, 0);
        assert_eq!(inst.protocol_version, Some(2));
        assert_eq!(inst.old_num, Some(0));
        assert_eq!(inst.new_num, Some(2));
        assert_eq!(inst.ack_num, Some(3));
        assert_eq!(inst.throwaway_num, Some(0));
        assert_eq!(inst.diff, Some(b"hello".to_vec()));
    }

    #[test]
    fn receiver_state_tracking() {
        let mut receiver = TransportReceiver::new(true);
        assert_eq!(receiver.state_num(), 0);

        receiver.advance_state();
        assert_eq!(receiver.state_num(), 1);

        assert!(!receiver.shutdown_received());
        receiver.set_shutdown_received();
        assert!(receiver.shutdown_received());
    }

    #[test]
    fn receiver_parse_instruction() {
        let inst = TransportInstruction {
            protocol_version: Some(2),
            old_num: Some(5),
            new_num: Some(6),
            ack_num: Some(3),
            throwaway_num: Some(1),
            diff: Some(b"data".to_vec()),
            chaff: None,
        };

        let diff = TransportReceiver::parse_instruction(&inst);
        assert_eq!(diff.old_num, 5);
        assert_eq!(diff.new_num, 6);
        assert_eq!(diff.ack_num, 3);
        assert_eq!(diff.throwaway_num, 1);
        assert_eq!(diff.diff, b"data");
    }

    #[test]
    fn nonce_direction_bit() {
        let mut client_sender = TransportSender::new(true);
        let mut server_sender = TransportSender::new(false);

        let client_nonce = client_sender.next_send_nonce();
        let server_nonce = server_sender.next_send_nonce();

        // Client nonce should have high bit set
        let client_val = client_nonce.val();
        assert!(client_val & (1u64 << 63) != 0, "Client nonce should have direction bit");

        // Server nonce should not have high bit set
        let server_val = server_nonce.val();
        assert!(server_val & (1u64 << 63) == 0, "Server nonce should not have direction bit");
    }

    #[test]
    fn shutdown_instruction() {
        let mut sender = TransportSender::new(false);
        sender.advance_state();

        let inst = sender.build_shutdown_instruction(1);
        assert!(inst.diff.is_none());
        assert_eq!(inst.old_num, Some(1));
        assert_eq!(inst.new_num, Some(1));
    }

    #[test]
    fn delayed_ack_scheduled_on_recv() {
        let mut receiver = TransportReceiver::new(true);
        let now = Instant::now();

        // Initially no pending ACK
        assert!(!receiver.pending_ack);
        assert!(!receiver.should_send_ack(now));

        // After recording a receive, a delayed ACK is scheduled
        receiver.record_recv(now);
        assert!(receiver.pending_ack);
        assert!(!receiver.should_send_ack(now)); // Too early

        // After ACK_DELAY_MS, should send
        assert!(receiver.should_send_ack(now + Duration::from_millis(ACK_DELAY_MS)));
    }

    #[test]
    fn delayed_ack_cleared_on_send() {
        let mut receiver = TransportReceiver::new(true);
        let now = Instant::now();

        receiver.record_recv(now);
        assert!(receiver.pending_ack);

        // Sending an ACK clears the pending state
        receiver.ack_sent(now + Duration::from_millis(50));
        assert!(!receiver.pending_ack);
        assert!(receiver.ack_deadline.is_none());
    }

    #[test]
    fn periodic_ack_interval() {
        let mut receiver = TransportReceiver::new(true);
        let now = Instant::now();

        // No pending ACK, but periodic ACK interval triggers
        assert!(receiver.should_send_ack(now + Duration::from_millis(ACK_INTERVAL_MS)));

        // After sending an ACK, reset the interval
        receiver.ack_sent(now + Duration::from_millis(ACK_INTERVAL_MS));
        assert!(!receiver.should_send_ack(now + Duration::from_millis(ACK_INTERVAL_MS + 100)));

        // But after another full interval, it triggers again
        assert!(receiver.should_send_ack(now + Duration::from_millis(ACK_INTERVAL_MS * 2)));
    }

    #[test]
    fn multiple_recacks_only_one_pending() {
        let mut receiver = TransportReceiver::new(true);
        let now = Instant::now();

        receiver.record_recv(now);
        let deadline1 = receiver.ack_deadline;

        // Second receive should NOT reset the deadline
        receiver.record_recv(now + Duration::from_millis(50));
        assert_eq!(receiver.ack_deadline, deadline1);
    }
}
