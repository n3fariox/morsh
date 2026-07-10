use crate::constants::*;
use crate::fragment::{Fragment, FragmentAssembly, Fragmenter};
use crate::rtt::RttEstimator;
use mosh_crypto::{Nonce, Session};
use mosh_proto::transport::Instruction as TransportInstruction;
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
}

/// Create a UDP socket, set nonblocking, and apply ECN marking (ECT(0)).
/// Cross-platform: uses IP_TOS on both Unix and Windows (Win10 1903+).
fn bind_udp(addr: SocketAddr) -> Result<tokio::net::UdpSocket, String> {
    let std_socket = std::net::UdpSocket::bind(addr)
        .map_err(|e| format!("Failed to bind UDP socket: {e}"))?;
    std_socket
        .set_nonblocking(true)
        .map_err(|e| format!("Failed to set nonblocking: {e}"))?;

    let sock_ref = socket2::SockRef::from(&std_socket);
    let _ = sock_ref.set_tos(0x02);

    tokio::net::UdpSocket::from_std(std_socket)
        .map_err(|e| format!("Failed to create tokio socket: {e}"))
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
                // Server uses recv_from to learn client address
                let (len, peer) = match self.sockets[idx].recv_from(&mut buf).await {
                    Ok(r) => r,
                    Err(e) => return Err(format!("Recv error: {e}")),
                };
                // Learn client address
                self.remote_addr = Some(peer);
                self.has_remote_addr = true;
                log::info!("Learned client address: {peer}");
                Ok(len)
            } else {
                self.sockets[idx].try_recv(&mut buf)
            };

            match result {
                Ok(len) => {
                    let data = buf[..len].to_vec();
                    let frag = self.decrypt_fragment(&data)?;
                    self.last_heard = Instant::now();
                    self.last_roundtrip_success = Instant::now();
                    return self.assembler.add_fragment(frag);
                }
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
    pub fn encrypt_fragment(&mut self, frag: &Fragment) -> Result<Vec<u8>, String> {
        let plaintext = frag.to_bytes();
        // Build nonce: direction bit (bit 63) + sequence number
        let nonce_val = if self.server {
            self.send_seq // Server sends without high bit
        } else {
            (1u64 << 63) | self.send_seq // Client sends with high bit set
        };
        let nonce = Nonce::from_val(nonce_val);
        self.send_seq += 1;
        let msg = mosh_crypto::Message::new(nonce, plaintext);
        let ciphertext = self.session.encrypt(&msg);
        Ok(ciphertext)
    }

    fn decrypt_fragment(&mut self, data: &[u8]) -> Result<Fragment, String> {
        let msg = self.session.decrypt(data)
            .map_err(|e| format!("Decryption error: {e}"))?;
        Fragment::from_bytes(&msg.text)
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
