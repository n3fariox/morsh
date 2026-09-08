use crate::constants::*;
use crate::fragment::{Fragment, FragmentAssembly, Fragmenter};
use crate::rtt::RttEstimator;
use morsh_crypto::{Nonce, Session};
use morsh_proto::transport::Instruction as TransportInstruction;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

/// Largest datagram we will read. Mosh uses `Session::RECEIVE_MTU` (2048).
const RECEIVE_MTU: usize = 2048;

/// Direction bit in the nonce: set for server→client, clear for client→server.
const TO_CLIENT_BIT: u64 = 1u64 << 63;

/// Encrypted UDP connection with fragmentation and RTT estimation.
pub struct Connection {
    sockets: Vec<UdpSocket>,
    remote_addr: Option<SocketAddr>,
    session: Session,
    rtt: RttEstimator,
    fragger: Fragmenter,
    assembler: FragmentAssembly,
    last_port_choice: Instant,
    last_roundtrip_success: Instant,
    server: bool,
    has_remote_addr: bool,
    last_heard: Instant,
    /// Send nonce sequence counter. Each send increments this.
    send_seq: u64,
    /// Lowest sequence number we will still treat as fresh. Rejecting replays
    /// here is security-sensitive: a replayed packet could otherwise corrupt
    /// the RTT estimate or re-target the server at an attacker's address.
    expected_receiver_seq: u64,
    /// Last received timestamp for RTT calculation (stock mosh compatible).
    saved_timestamp: u16,
    /// When the saved_timestamp was received.
    saved_timestamp_received_at: Instant,
}

/// Current time in milliseconds as a 16-bit value (mod 65536), never 0xFFFF.
fn timestamp16() -> u16 {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let ts = (ms % 65536) as u16;
    if ts == 0xFFFF {
        0
    } else {
        ts
    }
}

/// Create a UDP socket, set nonblocking, and apply ECN marking (ECT(0)).
/// Cross-platform: uses IP_TOS on both Unix and Windows (Win10 1903+).
/// On Windows, also disables SIO_UDP_CONNRESET so ICMP Port Unreachable
/// doesn't poison subsequent recvfrom calls with WSAECONNRESET.
fn bind_udp(addr: SocketAddr) -> Result<tokio::net::UdpSocket, String> {
    let std_socket =
        std::net::UdpSocket::bind(addr).map_err(|e| format!("Failed to bind UDP socket: {e}"))?;
    std_socket
        .set_nonblocking(true)
        .map_err(|e| format!("Failed to set nonblocking: {e}"))?;

    let sock_ref = socket2::SockRef::from(&std_socket);
    let _ = sock_ref.set_tos(0x02);

    #[cfg(windows)]
    disable_udp_connreset(&std_socket);

    tokio::net::UdpSocket::from_std(std_socket)
        .map_err(|e| format!("Failed to create tokio socket: {e}"))
}

/// On Windows, disable SIO_UDP_CONNRESET so that receiving an ICMP Port
/// Unreachable message does not cause the next recvfrom to fail with
/// WSAECONNRESET. This is the default behavior of the Windows TCP/IP stack
/// and breaks UDP robustness; stock mosh disables it too.
#[cfg(windows)]
fn disable_udp_connreset(socket: &std::net::UdpSocket) {
    use std::os::windows::io::AsRawSocket;

    // SIO_UDP_CONNRESET = 0x58000001
    const SIO_UDP_CONNRESET: u32 = 0x58000001;

    extern "system" {
        fn WSAIoctl(
            s: usize,
            dwIoControlCode: u32,
            lpvInBuffer: *const std::ffi::c_void,
            cbInBuffer: u32,
            lpvOutBuffer: *mut std::ffi::c_void,
            cbOutBuffer: u32,
            lpcbBytesReturned: *mut u32,
            lpOverlapped: *mut std::ffi::c_void,
            lpCompletionRoutine: *mut std::ffi::c_void,
        ) -> i32;
    }

    let sock = socket.as_raw_socket() as usize;
    let value: u32 = 0; // FALSE — disable the error
    let mut bytes_returned: u32 = 0;

    unsafe {
        WSAIoctl(
            sock,
            SIO_UDP_CONNRESET,
            &value as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }
}

impl Connection {
    fn build(socket: UdpSocket, session: Session, server: bool) -> Self {
        let now = Instant::now();
        Self {
            sockets: vec![socket],
            remote_addr: None,
            session,
            rtt: RttEstimator::new(),
            fragger: Fragmenter::new(),
            assembler: FragmentAssembly::new(),
            last_port_choice: now,
            last_roundtrip_success: now,
            server,
            has_remote_addr: false,
            last_heard: now,
            send_seq: 0,
            expected_receiver_seq: 0,
            saved_timestamp: 0,
            saved_timestamp_received_at: now,
        }
    }

    /// Create a server-side connection (single socket, no port hopping).
    pub async fn new_server(bind_addr: SocketAddr, session: Session) -> Result<Self, String> {
        Ok(Self::build(bind_udp(bind_addr)?, session, true))
    }

    /// Create a client-side connection.
    pub async fn new_client(session: Session) -> Result<Self, String> {
        Ok(Self::build(
            bind_udp("0.0.0.0:0".parse().unwrap())?,
            session,
            false,
        ))
    }

    /// Set the remote address (called on first receive for server).
    pub fn set_remote_addr(&mut self, addr: SocketAddr) {
        self.remote_addr = Some(addr);
        self.has_remote_addr = true;
    }

    pub fn has_remote(&self) -> bool {
        self.has_remote_addr
    }

    pub fn remote_addr(&self) -> Option<SocketAddr> {
        self.remote_addr
    }

    /// Get the local address of the socket (what we're bound to).
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.sockets.last().and_then(|s| s.local_addr().ok())
    }

    pub fn rtt(&self) -> &RttEstimator {
        &self.rtt
    }

    pub fn rtt_mut(&mut self) -> &mut RttEstimator {
        &mut self.rtt
    }

    /// Smoothed round-trip time in milliseconds.
    pub fn srtt_ms(&self) -> u64 {
        self.rtt.srtt_ms()
    }

    /// Retransmission timeout, mosh's `Connection::timeout()`.
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.rtt.rto_ms())
    }

    /// Time of the last packet whose ack proved end-to-end connectivity.
    pub fn set_last_roundtrip_success(&mut self, when: Instant) {
        self.last_roundtrip_success = when;
    }

    /// Send an encrypted, fragmented instruction to the remote.
    pub async fn send(&mut self, inst: &TransportInstruction) -> Result<(), String> {
        let remote = self.remote_addr.ok_or("No remote address")?;
        let frags = self
            .fragger
            .make_fragments(inst, max_fragment_payload(MTU_IPV4))?;

        for frag in &frags {
            let wire = self.encrypt_fragment(frag)?;
            let socket = self.sockets.last().unwrap();
            socket
                .send_to(&wire, remote)
                .await
                .map_err(|e| format!("Send error: {e}"))?;
        }

        Ok(())
    }

    /// Await one datagram on *any* open socket.
    ///
    /// Cancel-safe: the only await point is `recv_from`, which tokio
    /// guarantees loses no data if the future is dropped. All sockets are
    /// polled concurrently, so a datagram arriving on a socket opened by
    /// [`Connection::hop_port`] is seen even while the original socket is idle
    /// — a sequential loop would block forever on socket 0 and never poll the
    /// others.
    pub async fn recv_raw(&self) -> Result<(Vec<u8>, SocketAddr), String> {
        if self.sockets.is_empty() {
            return Err("No sockets open".into());
        }

        let mut receives = Vec::with_capacity(self.sockets.len());
        for sock in &self.sockets {
            receives.push(Box::pin(async move {
                let mut buf = [0u8; RECEIVE_MTU];
                match sock.recv_from(&mut buf).await {
                    Ok((len, peer)) => Ok((buf[..len].to_vec(), peer)),
                    Err(e) => Err(format!("Recv error: {e}")),
                }
            }));
        }

        let (result, _index, _remaining) = futures::future::select_all(receives).await;
        result
    }

    /// Decrypt, validate, and reassemble one datagram.
    ///
    /// Returns `Ok(None)` while a fragmented instruction is still incomplete.
    /// Purely synchronous, so callers can run it after a cancel-safe
    /// [`Connection::recv_raw`] without risking a lost packet.
    pub fn process_incoming(
        &mut self,
        data: &[u8],
        peer: SocketAddr,
    ) -> Result<Option<TransportInstruction>, String> {
        let msg = self
            .session
            .decrypt(data)
            .map_err(|e| format!("Decryption error: {e}"))?;

        // Prevent malicious playback to the sender: a packet we sent must
        // never be accepted as one we received.
        let nonce_val = msg.nonce.val();
        let to_client = nonce_val & TO_CLIENT_BIT != 0;
        if to_client == self.server {
            return Err(format!(
                "Wrong packet direction (to_client={to_client}, we are {})",
                if self.server { "server" } else { "client" }
            ));
        }
        let seq = nonce_val & !TO_CLIENT_BIT;

        if msg.text.len() < 4 {
            return Err(format!("Message too short: {} bytes", msg.text.len()));
        }
        let frag = Fragment::from_bytes(&msg.text[4..])?;

        // Out-of-order (or replayed) packets are still delivered to the state
        // synchronizer — which has its own idempotency guards — but they must
        // not be used for timestamps or for re-targeting the peer address.
        if seq < self.expected_receiver_seq {
            tracing::debug!(
                "Out-of-order packet seq={} (expected >= {}), not used for timing/targeting",
                seq,
                self.expected_receiver_seq
            );
            return self.assembler.add_fragment(frag);
        }
        self.expected_receiver_seq = seq + 1;

        let timestamp = u16::from_be_bytes([msg.text[0], msg.text[1]]);
        let timestamp_reply = u16::from_be_bytes([msg.text[2], msg.text[3]]);

        if timestamp != 0xFFFF {
            self.saved_timestamp = timestamp;
            self.saved_timestamp_received_at = Instant::now();
        }

        // The only RTT measurement: how long ago the peer echoed back a
        // timestamp of ours. `timestamp_diff` keeps the 16-bit wraparound
        // signed, so a measurement across a rollover is not read as ~65 s.
        if timestamp_reply != 0xFFFF {
            let rtt = timestamp_diff(timestamp16(), timestamp_reply);
            if rtt > 0 && rtt < 5000 {
                self.rtt.update(rtt as u64);
                tracing::debug!("RTT measurement: {rtt}ms");
            }
        }

        let now = Instant::now();
        self.last_heard = now;
        self.has_remote_addr = true;

        // Roaming: re-target whenever the source differs, but only after the
        // packet has authenticated. Only the client roams.
        if self.server && self.remote_addr != Some(peer) {
            tracing::info!("Server now attached to client at {peer} (was {:?})", self.remote_addr);
            self.remote_addr = Some(peer);
        }

        self.assembler.add_fragment(frag)
    }

    /// Check if we should hop ports (client only).
    pub fn should_hop_port(&self, now: Instant) -> bool {
        !self.server
            && (now - self.last_port_choice > std::time::Duration::from_millis(PORT_HOP_INTERVAL_MS))
            && (now - self.last_roundtrip_success
                > std::time::Duration::from_millis(PORT_HOP_INTERVAL_MS))
    }

    /// Create a new socket for port hopping.
    pub async fn hop_port(&mut self) -> Result<(), String> {
        let socket = bind_udp("0.0.0.0:0".parse().unwrap())?;

        self.sockets.push(socket);
        self.prune_sockets();
        self.last_port_choice = Instant::now();
        tracing::info!("Hopped to new local port {:?}", self.local_addr());
        Ok(())
    }

    /// Number of open sockets (for tests).
    pub fn socket_count(&self) -> usize {
        self.sockets.len()
    }

    /// The ack number on the last instruction we fragmented. `u64::MAX` means
    /// we have already acknowledged the counterparty's shutdown.
    pub fn fragmenter_last_ack_sent(&self) -> u64 {
        self.fragger.last_ack_sent()
    }

    fn prune_sockets(&mut self) {
        while self.sockets.len() > MAX_PORTS_OPEN {
            self.sockets.remove(0);
        }
    }

    pub fn time_since_last_heard(&self, now: Instant) -> std::time::Duration {
        now.duration_since(self.last_heard)
    }

    pub fn record_rtt(&mut self, rtt_ms: u64) {
        self.rtt.update(rtt_ms);
    }

    /// Get the current send sequence number (for testing).
    pub fn send_seq(&self) -> u64 {
        self.send_seq
    }

    /// Encrypt a fragment (public for testing).
    /// Stock mosh message format: [timestamp 2B BE] [timestamp_reply 2B BE] [fragment data]
    pub fn encrypt_fragment(&mut self, frag: &Fragment) -> Result<Vec<u8>, String> {
        let frag_bytes = frag.to_bytes();
        // Build timestamps (stock mosh compatible):
        // timestamp = current ms mod 65536 (never 0xFFFF)
        // timestamp_reply = 0xFFFF (no reply), or corrected echo of received timestamp
        let ts = timestamp16();
        let ts_reply = {
            let elapsed = Instant::now().duration_since(self.saved_timestamp_received_at);
            if elapsed.as_millis() < 1000 && self.saved_timestamp != 0 {
                self.saved_timestamp.wrapping_add(elapsed.as_millis() as u16)
            } else {
                0xFFFF
            }
        };

        let mut plaintext = Vec::with_capacity(4 + frag_bytes.len());
        plaintext.extend_from_slice(&ts.to_be_bytes());
        plaintext.extend_from_slice(&ts_reply.to_be_bytes());
        plaintext.extend_from_slice(&frag_bytes);

        // Build nonce: direction bit (bit 63) + sequence number
        // Stock mosh convention: TO_SERVER = 0, TO_CLIENT = 1 << 63
        // Client→Server uses TO_SERVER (0), Server→Client uses TO_CLIENT (1<<63)
        let nonce_val = if self.server {
            TO_CLIENT_BIT | self.send_seq
        } else {
            self.send_seq
        };
        let nonce = Nonce::from_val(nonce_val);
        self.send_seq += 1;
        let msg = morsh_crypto::Message::new(nonce, plaintext);
        let ciphertext = self.session.encrypt(&msg);
        Ok(ciphertext)
    }
}

/// Timestamp diff accounting for 16-bit wrapping.
pub fn timestamp_diff(a: u16, b: u16) -> i32 {
    a.wrapping_sub(b) as i16 as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use morsh_crypto::Base64Key;

    #[test]
    fn timestamp_diff_basic() {
        assert_eq!(timestamp_diff(100, 50), 50);
        assert_eq!(timestamp_diff(50, 100), -50);
    }

    #[test]
    fn timestamp_diff_wrapping() {
        assert_eq!(timestamp_diff(0, 65535), 1);
        assert_eq!(timestamp_diff(65535, 0), -1);
    }

    fn key() -> [u8; 16] {
        *Base64Key::random().data()
    }

    fn instruction(new_num: u64) -> TransportInstruction {
        TransportInstruction {
            protocol_version: Some(MORSH_PROTOCOL_VERSION),
            old_num: Some(new_num.saturating_sub(1)),
            new_num: Some(new_num),
            ack_num: Some(0),
            throwaway_num: Some(0),
            diff: Some(b"payload".to_vec()),
            chaff: None,
        }
    }

    /// A packet the server itself sent must not be accepted back by the
    /// server: `dos_assert( p.direction == (server ? TO_SERVER : TO_CLIENT) )`.
    #[tokio::test]
    async fn rejects_packets_sent_in_our_own_direction() {
        let k = key();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut server = Connection::new_server(addr, Session::new(k)).await.unwrap();
        let peer: SocketAddr = "127.0.0.1:12345".parse().unwrap();

        // Encrypt as the server would, then feed it back to the server.
        let frags = server
            .fragger
            .make_fragments(&instruction(1), max_fragment_payload(MTU_IPV4))
            .unwrap();
        let wire = server.encrypt_fragment(&frags[0]).unwrap();

        let err = server.process_incoming(&wire, peer).unwrap_err();
        assert!(err.contains("direction"), "{err}");
    }

    #[tokio::test]
    async fn accepts_packets_from_the_other_direction() {
        let k = key();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut server = Connection::new_server(addr, Session::new(k)).await.unwrap();
        let mut client = Connection::new_client(Session::new(k)).await.unwrap();
        let peer: SocketAddr = "127.0.0.1:12345".parse().unwrap();

        let frags = client
            .fragger
            .make_fragments(&instruction(1), max_fragment_payload(MTU_IPV4))
            .unwrap();
        let wire = client.encrypt_fragment(&frags[0]).unwrap();

        let inst = server.process_incoming(&wire, peer).unwrap().unwrap();
        assert_eq!(inst.new_num, Some(1));
        assert_eq!(server.remote_addr(), Some(peer));
    }

    /// A replayed packet is still delivered (the state synchronizer dedups it)
    /// but must not move the server to the replayer's address.
    #[tokio::test]
    async fn replayed_packet_does_not_retarget_the_server() {
        let k = key();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut server = Connection::new_server(addr, Session::new(k)).await.unwrap();
        let mut client = Connection::new_client(Session::new(k)).await.unwrap();
        let good: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let attacker: SocketAddr = "10.9.9.9:666".parse().unwrap();

        let first = {
            let frags = client
                .fragger
                .make_fragments(&instruction(1), max_fragment_payload(MTU_IPV4))
                .unwrap();
            client.encrypt_fragment(&frags[0]).unwrap()
        };
        let second = {
            let frags = client
                .fragger
                .make_fragments(&instruction(2), max_fragment_payload(MTU_IPV4))
                .unwrap();
            client.encrypt_fragment(&frags[0]).unwrap()
        };

        server.process_incoming(&first, good).unwrap();
        server.process_incoming(&second, good).unwrap();
        assert_eq!(server.remote_addr(), Some(good));

        // Replay of the first packet from somewhere else.
        server.process_incoming(&first, attacker).unwrap();
        assert_eq!(
            server.remote_addr(),
            Some(good),
            "replayed packet re-targeted the server"
        );
    }

    /// Roaming: a fresh packet from a new address does move the server.
    #[tokio::test]
    async fn fresh_packet_from_new_address_retargets_the_server() {
        let k = key();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut server = Connection::new_server(addr, Session::new(k)).await.unwrap();
        let mut client = Connection::new_client(Session::new(k)).await.unwrap();
        let first_addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let roamed: SocketAddr = "127.0.0.1:23456".parse().unwrap();

        for (n, from) in [(1u64, first_addr), (2, roamed)] {
            let frags = client
                .fragger
                .make_fragments(&instruction(n), max_fragment_payload(MTU_IPV4))
                .unwrap();
            let wire = client.encrypt_fragment(&frags[0]).unwrap();
            server.process_incoming(&wire, from).unwrap();
        }

        assert_eq!(server.remote_addr(), Some(roamed));
    }

    /// Garbage must not authenticate.
    #[tokio::test]
    async fn rejects_undecryptable_datagrams() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut server = Connection::new_server(addr, Session::new(key()))
            .await
            .unwrap();
        let peer: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert!(server.process_incoming(&[0u8; 40], peer).is_err());
    }
}
