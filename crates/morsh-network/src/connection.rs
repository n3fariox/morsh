use crate::constants::*;
use crate::fragment::{Fragment, FragmentAssembly, Fragmenter};
use crate::rtt::RttEstimator;
use morsh_crypto::{Nonce, Session};
use morsh_proto::transport::Instruction as TransportInstruction;
use std::net::SocketAddr;
use std::time::Instant;
use tokio::net::UdpSocket;

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
    if ts == 0xFFFF { 0 } else { ts }
}

/// Create a UDP socket, set nonblocking, and apply ECN marking (ECT(0)).
/// Cross-platform: uses IP_TOS on both Unix and Windows (Win10 1903+).
/// On Windows, also disables SIO_UDP_CONNRESET so ICMP Port Unreachable
/// doesn't poison subsequent recvfrom calls with WSAECONNRESET.
fn bind_udp(addr: SocketAddr) -> Result<tokio::net::UdpSocket, String> {
    let std_socket = std::net::UdpSocket::bind(addr)
        .map_err(|e| format!("Failed to bind UDP socket: {e}"))?;
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
    /// Create a server-side connection (single socket, no port hopping).
    pub async fn new_server(bind_addr: SocketAddr, session: Session) -> Result<Self, String> {
        let socket = bind_udp(bind_addr)?;

        let now = Instant::now();
        Ok(Self {
            sockets: vec![socket],
            remote_addr: None,
            session,
            rtt: RttEstimator::new(),
            fragger: Fragmenter::new(),
            assembler: FragmentAssembly::new(),
            last_port_choice: now,
            last_roundtrip_success: now,
            server: true,
            has_remote_addr: false,
            last_heard: now,
            send_seq: 0,
            saved_timestamp: 0,
            saved_timestamp_received_at: now,
        })
    }

    /// Create a client-side connection.
    pub async fn new_client(session: Session) -> Result<Self, String> {
        let socket = bind_udp("0.0.0.0:0".parse().unwrap())?;

        let now = Instant::now();
        Ok(Self {
            sockets: vec![socket],
            remote_addr: None,
            session,
            rtt: RttEstimator::new(),
            fragger: Fragmenter::new(),
            assembler: FragmentAssembly::new(),
            last_port_choice: now,
            last_roundtrip_success: now,
            server: false,
            has_remote_addr: false,
            last_heard: now,
            send_seq: 0,
            saved_timestamp: 0,
            saved_timestamp_received_at: now,
        })
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

    /// Send an encrypted, fragmented instruction to the remote.
    pub async fn send(&mut self, inst: &TransportInstruction) -> Result<(), String> {
        let remote = self.remote_addr.ok_or("No remote address")?;
        let frags = self.fragger.make_fragments(inst, max_fragment_payload(MTU_IPV4))?;

        for frag in &frags {
            let wire = self.encrypt_fragment(frag)?;
            let socket = self.sockets.last().unwrap();
            socket.send_to(&wire, remote).await
                .map_err(|e| format!("Send error: {e}"))?;
        }

        Ok(())
    }

    /// Receive and decrypt a fragment from any socket.
    pub async fn recv(&mut self) -> Result<Option<TransportInstruction>, String> {
        let mut buf = vec![0u8; MTU_IPV4 + 64];

        let socket_count = self.sockets.len();
        for idx in 0..socket_count {
            let result = if self.server && !self.has_remote_addr {
                self.sockets[idx].recv_from(&mut buf).await
                    .map(|(len, peer)| (len, Some(peer)))
            } else {
                self.sockets[idx].recv_from(&mut buf).await
                    .map(|(len, _)| (len, None))
            };

            match result {
                Ok((len, Some(peer))) if len > 0 => {
                    if !self.has_remote_addr {
                        self.remote_addr = Some(peer);
                        self.has_remote_addr = true;
                        log::info!("Learned client address: {peer}");
                    } else if self.remote_addr != Some(peer) {
                        // Client hopped to a new port - update our record
                        log::info!("Client hopped to new port: {peer} (was {:?})", self.remote_addr);
                        self.remote_addr = Some(peer);
                    }
                    log::debug!("Received {} bytes from socket {}", len, idx);
                    let data = buf[..len].to_vec();
                    let frag = self.decrypt_fragment(&data)?;
                    self.last_heard = Instant::now();
                    self.last_roundtrip_success = Instant::now();
                    return self.assembler.add_fragment(frag);
                }
                Ok((len, _)) if len > 0 => {
                    log::debug!("Received {} bytes from socket {}", len, idx);
                    let data = buf[..len].to_vec();
                    let frag = self.decrypt_fragment(&data)?;
                    self.last_heard = Instant::now();
                    self.last_roundtrip_success = Instant::now();
                    return self.assembler.add_fragment(frag);
                }
                Ok(_) => continue,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(format!("Recv error: {e}")),
            }
        }

        Ok(None)
    }

    /// Check if we should hop ports (client only).
    pub fn should_hop_port(&self, now: Instant) -> bool {
        !self.server
            && (now - self.last_port_choice > std::time::Duration::from_millis(PORT_HOP_INTERVAL_MS))
            && (now - self.last_roundtrip_success > std::time::Duration::from_millis(PORT_HOP_INTERVAL_MS))
    }

    /// Create a new socket for port hopping.
    pub async fn hop_port(&mut self) -> Result<(), String> {
        let socket = bind_udp("0.0.0.0:0".parse().unwrap())?;

        self.sockets.push(socket);
        self.prune_sockets();
        self.last_port_choice = Instant::now();
        Ok(())
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
            (1u64 << 63) | self.send_seq // Server→Client: TO_CLIENT
        } else {
            self.send_seq // Client→Server: TO_SERVER
        };
        let nonce = Nonce::from_val(nonce_val);
        self.send_seq += 1;
        let msg = morsh_crypto::Message::new(nonce, plaintext);
        let ciphertext = self.session.encrypt(&msg);
        Ok(ciphertext)
    }

    fn decrypt_fragment(&mut self, data: &[u8]) -> Result<Fragment, String> {
        let msg = self.session.decrypt(data)
            .map_err(|e| format!("Decryption error: {e}"))?;
        log::debug!("Decrypted {} -> {} bytes", data.len(), msg.text.len());
        // Stock mosh message format: [timestamp 2B BE] [timestamp_reply 2B BE] [fragment data]
        if msg.text.len() < 4 {
            return Err(format!("Message too short: {} bytes", msg.text.len()));
        }
        let timestamp = u16::from_be_bytes([msg.text[0], msg.text[1]]);
        let timestamp_reply = u16::from_be_bytes([msg.text[2], msg.text[3]]);
        log::debug!("Timestamps: ts={}, ts_reply={}", timestamp, timestamp_reply);

        // Save received timestamp for echo reply (stock mosh compatible)
        if timestamp != 0xFFFF {
            self.saved_timestamp = timestamp;
            self.saved_timestamp_received_at = Instant::now();
        }

        // Compute RTT from timestamp_reply (corrected echo of our sent timestamp)
        if timestamp_reply != 0xFFFF {
            let now = timestamp16();
            let rtt = now.wrapping_sub(timestamp_reply) as i32;
            if rtt > 0 && rtt < 5000 {
                self.rtt.update(rtt as u64);
                log::debug!("RTT measurement: {}ms", rtt);
            }
        }

        let frag_bytes = &msg.text[4..];
        if msg.text.len() >= 14 {
            let hex: Vec<String> = frag_bytes.iter().take(30).map(|b| format!("{:02x}", b)).collect();
            log::debug!("Fragment data first {} bytes: [{}]", hex.len(), hex.join(" "));
        }
        let frag = Fragment::from_bytes(frag_bytes)?;
        log::debug!("Fragment: id={}, final={}, frag_num={}, payload_len={}",
            frag.id, frag.final_flag, frag.fragment_num, frag.payload.len());
        Ok(frag)
    }
}

/// Timestamp diff accounting for 16-bit wrapping.
pub fn timestamp_diff(a: u16, b: u16) -> i32 {
    a.wrapping_sub(b) as i16 as i32
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
