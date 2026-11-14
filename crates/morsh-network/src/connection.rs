use crate::constants::*;
use crate::fragment::{Fragment, FragmentAssembly, Fragmenter, Prepared};
use crate::rtt::RttEstimator;
use morsh_crypto::{Nonce, Session, NONCE_WIRE_LEN};
use morsh_proto::transport::Instruction as TransportInstruction;
use std::net::SocketAddr;
use std::task::Poll;
use std::time::{Duration, Instant};
#[cfg(not(any(target_os = "linux", target_os = "android")))]
use tokio::io::ReadBuf;
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
    /// Application datagram MTU (mosh's `MTU`): set from the address family,
    /// and dropped to [`MTU_FALLBACK`] if the kernel reports `EMSGSIZE`.
    mtu: usize,
    /// The peer's most recent timestamp and when it arrived, waiting to be
    /// echoed back exactly once (mosh's `saved_timestamp` /
    /// `saved_timestamp_received_at`).
    saved_timestamp: Option<(u16, Instant)>,
    /// Reused for every outgoing datagram, so sending does not allocate.
    send_buf: Vec<u8>,
    /// Every incoming datagram lands here and is decrypted in place.
    recv_buf: Box<[u8]>,
}

/// Mosh's `Packet::toMessage` framing, sealed in place: `buf` becomes
/// `[nonce][timestamp][timestamp_reply][fragment header][payload][tag]`.
fn frame_and_seal(
    session: &mut Session,
    buf: &mut Vec<u8>,
    nonce: &Nonce,
    timestamps: [u16; 2],
    header: &[u8],
    payload: &[u8],
) {
    buf.clear();
    buf.resize(NONCE_WIRE_LEN, 0);
    buf.extend_from_slice(&timestamps[0].to_be_bytes());
    buf.extend_from_slice(&timestamps[1].to_be_bytes());
    buf.extend_from_slice(header);
    buf.extend_from_slice(payload);
    session.seal(nonce, buf);
}

/// Current time in milliseconds as a 16-bit value (mod 65536), never 0xFFFF.
///
/// Monotonic, like mosh's `timestamp()`: a wall-clock step (NTP, suspend)
/// between sending a timestamp and receiving its echo must not turn into a
/// bogus RTT sample.
fn timestamp16() -> u16 {
    static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let ms = ORIGIN.get_or_init(Instant::now).elapsed().as_millis();
    let ts = (ms % 65536) as u16;
    if ts == 0xFFFF {
        0
    } else {
        ts
    }
}

/// Mosh's `Connection::set_MTU`: the guaranteed 1280-byte path MTU minus a
/// family-dependent guess at the headers.
fn mtu_for(addr: &SocketAddr) -> usize {
    match addr {
        SocketAddr::V4(_) => MTU_IPV4,
        SocketAddr::V6(_) => MTU_IPV6,
    }
}

/// The wildcard address of `addr`'s family, for a client socket.
fn unspecified_like(addr: &SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V4(_) => (std::net::Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    }
}

/// Whether a send failed because the datagram was too big for the path.
fn is_emsgsize(e: &std::io::Error) -> bool {
    #[cfg(unix)]
    const EMSGSIZE: i32 = libc::EMSGSIZE;
    #[cfg(windows)]
    const EMSGSIZE: i32 = 10040; // WSAEMSGSIZE
    #[cfg(not(any(unix, windows)))]
    const EMSGSIZE: i32 = -1;
    e.raw_os_error() == Some(EMSGSIZE)
}

/// Mosh disables path MTU discovery so the kernel never sets DF: an
/// oversize datagram is then fragmented by the network rather than dropped
/// at a hop whose ICMP "fragmentation needed" never reaches us.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn disable_pmtu_discovery(socket: &std::net::UdpSocket) {
    use std::os::fd::AsRawFd;
    let flag: libc::c_int = libc::IP_PMTUDISC_DONT;
    // SAFETY: a valid fd and a correctly sized c_int option value.
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MTU_DISCOVER,
            (&flag as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        tracing::debug!(
            "setsockopt(IP_MTU_DISCOVER): {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Explicit Congestion Notification on received datagrams, as in mosh's
/// `recv_one`: `IP_RECVTOS` delivers each datagram's TOS byte, and a
/// Congestion Experienced mark makes us echo the peer's timestamp 500 ms
/// stale, which gradually slows it to its minimum frame rate.
#[cfg(any(target_os = "linux", target_os = "android"))]
mod ecn {
    use std::io;
    use std::net::SocketAddr;
    use std::os::fd::AsRawFd;

    pub fn enable(socket: &std::net::UdpSocket) {
        let on: libc::c_int = 1;
        // SAFETY: a valid fd and a correctly sized c_int option value.
        let rc = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::IPPROTO_IP,
                libc::IP_RECVTOS,
                (&on as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            tracing::debug!("setsockopt(IP_RECVTOS): {}", io::Error::last_os_error());
        }
    }

    /// One non-blocking `recvmsg`: payload length, source, and whether the
    /// datagram was marked Congestion Experienced.
    pub fn recv(socket: &impl AsRawFd, buf: &mut [u8]) -> io::Result<(usize, SocketAddr, bool)> {
        // SAFETY: every pointer in `msg` refers to a live local of the size
        // given alongside it, and the cmsg walk stays within `control` via
        // the libc CMSG_* helpers.
        unsafe {
            let mut storage: libc::sockaddr_storage = std::mem::zeroed();
            let mut iov = libc::iovec {
                iov_base: buf.as_mut_ptr().cast(),
                iov_len: buf.len(),
            };
            let mut control = [0u64; 16];
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_name = (&mut storage as *mut libc::sockaddr_storage).cast();
            msg.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = std::mem::size_of_val(&control) as _;

            let n = libc::recvmsg(socket.as_raw_fd(), &mut msg, libc::MSG_DONTWAIT);
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if msg.msg_flags & libc::MSG_TRUNC != 0 {
                return Err(io::Error::other("Received oversize datagram"));
            }

            let mut congestion = false;
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::IPPROTO_IP
                    && ((*cmsg).cmsg_type == libc::IP_TOS || (*cmsg).cmsg_type == libc::IP_RECVTOS)
                {
                    congestion = *libc::CMSG_DATA(cmsg) & 0x03 == 0x03;
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }

            let peer = socket2::SockAddr::new(storage, msg.msg_namelen)
                .as_socket()
                .ok_or_else(|| io::Error::other("datagram from a non-IP address"))?;
            Ok((n as usize, peer, congestion))
        }
    }
}

/// Create a UDP socket, set nonblocking, and apply ECN marking (ECT(0)).
/// Cross-platform: uses IP_TOS on both Unix and Windows (Win10 1903+).
/// On Windows, also disables SIO_UDP_CONNRESET so ICMP Port Unreachable
/// doesn't poison subsequent recvfrom calls with WSAECONNRESET.
fn bind_udp(addr: SocketAddr) -> Result<tokio::net::UdpSocket, String> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .map_err(|e| format!("Failed to create UDP socket: {e}"))?;
    // Mosh's `try_bind`: a socket bound to the IPv6 wildcard also accepts
    // IPv4 (the OS default differs: on by default on BSD and Windows).
    if let SocketAddr::V6(v6) = addr {
        if v6.ip().is_unspecified() {
            if let Err(e) = socket.set_only_v6(false) {
                tracing::debug!("setsockopt(IPV6_V6ONLY, off): {e}");
            }
        }
    }
    socket
        .bind(&addr.into())
        .map_err(|e| format!("Failed to bind UDP socket: {e}"))?;
    let std_socket: std::net::UdpSocket = socket.into();
    std_socket
        .set_nonblocking(true)
        .map_err(|e| format!("Failed to set nonblocking: {e}"))?;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        disable_pmtu_discovery(&std_socket);
        ecn::enable(&std_socket);
    }

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
    fn build(socket: UdpSocket, session: Session, server: bool, mtu: usize) -> Self {
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
            mtu,
            saved_timestamp: None,
            send_buf: Vec::with_capacity(RECEIVE_MTU),
            recv_buf: vec![0; RECEIVE_MTU].into_boxed_slice(),
        }
    }

    /// Create a server-side connection (single socket, no port hopping).
    pub async fn new_server(bind_addr: SocketAddr, session: Session) -> Result<Self, String> {
        Ok(Self::build(
            bind_udp(bind_addr)?,
            session,
            true,
            mtu_for(&bind_addr),
        ))
    }

    /// Create a client-side connection. Its socket is IPv4 until
    /// [`Connection::set_remote_addr`] names a server of another family.
    pub async fn new_client(session: Session) -> Result<Self, String> {
        let any: SocketAddr = (std::net::Ipv4Addr::UNSPECIFIED, 0).into();
        Ok(Self::build(bind_udp(any)?, session, false, MTU_FALLBACK))
    }

    /// Point the client at its server.
    ///
    /// Mosh's client creates its socket in the server's address family and
    /// sizes its MTU from it; an IPv4 socket cannot reach an IPv6 server, so
    /// the socket is replaced if the family differs.
    pub fn set_remote_addr(&mut self, addr: SocketAddr) {
        self.mtu = mtu_for(&addr);
        let family_differs = self
            .local_addr()
            .is_some_and(|local| local.is_ipv4() != addr.is_ipv4());
        if family_differs {
            match bind_udp(unspecified_like(&addr)) {
                Ok(socket) => self.sockets = vec![socket],
                Err(e) => tracing::warn!("Cannot open a socket for {addr}: {e}"),
            }
        }
        self.remote_addr = Some(addr);
        self.has_remote_addr = true;
    }

    /// Current application datagram MTU (for tests).
    pub fn mtu(&self) -> usize {
        self.mtu
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

    /// Smoothed round-trip time in milliseconds, mosh's `get_SRTT()`.
    pub fn srtt(&self) -> f64 {
        self.rtt.srtt()
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
    ///
    /// Like mosh's `Connection::send`, a failed `sendto` does not abort the
    /// instruction: every fragment is still attempted (a transient error
    /// while roaming must not strand the rest), and the last error is
    /// returned for the caller to display.
    pub async fn send(&mut self, inst: &TransportInstruction) -> Result<(), String> {
        let Some(remote) = self.remote_addr.filter(|_| self.has_remote_addr) else {
            return Ok(());
        };
        let prepared = self.prepare_instruction(inst)?;

        let mut error = None;
        for index in 0..prepared.count {
            self.seal_datagram(&prepared, index)?;
            let Some(socket) = self.sockets.last() else {
                return Err("No sockets open".into());
            };
            let wire = &self.send_buf;
            match socket.send_to(wire, remote).await {
                Ok(n) if n == wire.len() => {}
                Ok(n) => error = Some(format!("sendto: short write ({n} of {})", wire.len())),
                Err(e) => {
                    if is_emsgsize(&e) {
                        self.mtu = MTU_FALLBACK; // payload MTU of last resort
                    }
                    error = Some(format!("sendto: {e}"));
                }
            }
        }

        self.check_server_association(Instant::now());
        error.map_or(Ok(()), Err)
    }

    /// Serialize, compress and split `inst` for the current MTU. The first
    /// half of [`Connection::send`]; public for benchmarks.
    pub fn prepare_instruction(&mut self, inst: &TransportInstruction) -> Result<Prepared, String> {
        self.fragger.prepare(inst, max_fragment_payload(self.mtu))
    }

    /// Frame and encrypt fragment `index` of a prepared instruction,
    /// returning the datagram. Each call consumes a sequence number. The
    /// second half of [`Connection::send`]; public for benchmarks.
    pub fn seal_datagram(&mut self, prepared: &Prepared, index: usize) -> Result<&[u8], String> {
        let now = Instant::now();
        let timestamps = [timestamp16(), self.take_timestamp_reply(now)];
        let nonce = self.next_nonce()?;
        let (header, payload) = self.fragger.fragment(prepared, index);
        frame_and_seal(
            &mut self.session,
            &mut self.send_buf,
            &nonce,
            timestamps,
            &header,
            payload,
        );
        Ok(&self.send_buf)
    }

    /// The nonce for the next outgoing packet: the direction bit (set for
    /// server→client, mosh's `TO_CLIENT`) and a 63-bit sequence number that
    /// must never repeat under one key.
    fn next_nonce(&mut self) -> Result<Nonce, String> {
        if self.send_seq & TO_CLIENT_BIT != 0 {
            return Err("Sequence numbers exhausted; refusing to reuse a nonce".into());
        }
        let direction = if self.server { TO_CLIENT_BIT } else { 0 };
        let nonce = Nonce::from_val(direction | self.send_seq);
        self.send_seq += 1;
        Ok(nonce)
    }

    /// Mosh's server forgets a client it has not heard from for
    /// `SERVER_ASSOCIATION_TIMEOUT`, and stops sending until the client
    /// (possibly from a new address) is heard again.
    fn check_server_association(&mut self, now: Instant) {
        if self.server
            && self.has_remote_addr
            && now.saturating_duration_since(self.last_heard)
                > Duration::from_millis(SERVER_ASSOCIATION_TIMEOUT_MS)
        {
            self.has_remote_addr = false;
            tracing::info!("Server now detached from client.");
        }
    }

    /// Receive one datagram on *any* open socket and process it; see
    /// [`Connection::process_incoming`].
    ///
    /// Cancel-safe: the only await point polls the sockets, and tokio's
    /// `poll_recv_from` loses no data when it returns `Pending`; everything
    /// after it is synchronous. All sockets are polled, so a datagram
    /// arriving on a socket opened by [`Connection::hop_port`] is seen even
    /// while the original socket is idle.
    pub async fn recv(&mut self) -> Result<Option<TransportInstruction>, String> {
        let (len, peer, congestion) = self.recv_datagram().await?;
        self.process_received(len, peer, congestion)
    }

    /// Await a datagram into `recv_buf`: its length, source, and whether it
    /// carried a Congestion Experienced mark.
    async fn recv_datagram(&mut self) -> Result<(usize, SocketAddr, bool), String> {
        let Self {
            sockets, recv_buf, ..
        } = self;
        if sockets.is_empty() {
            return Err("No sockets open".into());
        }
        std::future::poll_fn(|cx| {
            for socket in sockets.iter() {
                match poll_recv(socket, cx, recv_buf) {
                    Poll::Ready(result) => {
                        return Poll::Ready(result.map_err(|e| format!("Recv error: {e}")))
                    }
                    Poll::Pending => {}
                }
            }
            Poll::Pending
        })
        .await
    }

    /// Decrypt, validate, and reassemble one datagram.
    ///
    /// Returns `Ok(None)` while a fragmented instruction is still incomplete.
    pub fn process_incoming(
        &mut self,
        data: &[u8],
        peer: SocketAddr,
    ) -> Result<Option<TransportInstruction>, String> {
        let Some(dest) = self.recv_buf.get_mut(..data.len()) else {
            return Err(format!("Received oversize datagram ({} bytes)", data.len()));
        };
        dest.copy_from_slice(data);
        self.process_received(data.len(), peer, false)
    }

    /// Process the datagram in `recv_buf[..len]`, decrypting it in place.
    fn process_received(
        &mut self,
        len: usize,
        peer: SocketAddr,
        congestion: bool,
    ) -> Result<Option<TransportInstruction>, String> {
        let (nonce, text) = self
            .session
            .open(&mut self.recv_buf[..len])
            .map_err(|e| format!("Decryption error: {e}"))?;

        // Prevent malicious playback to the sender: a packet we sent must
        // never be accepted as one we received.
        let nonce_val = nonce.val();
        let to_client = nonce_val & TO_CLIENT_BIT != 0;
        if to_client == self.server {
            return Err(format!(
                "Wrong packet direction (to_client={to_client}, we are {})",
                if self.server { "server" } else { "client" }
            ));
        }
        let seq = nonce_val & !TO_CLIENT_BIT;

        let Some((timestamps, fragment)) = text.split_first_chunk::<4>() else {
            return Err(format!("Message too short: {} bytes", text.len()));
        };

        // Out-of-order (or replayed) packets are still delivered to the state
        // synchronizer — which has its own idempotency guards — but they must
        // not be used for timestamps or for re-targeting the peer address.
        if seq < self.expected_receiver_seq {
            tracing::debug!(
                "Out-of-order packet seq={} (expected >= {}), not used for timing/targeting",
                seq,
                self.expected_receiver_seq
            );
            return self.assembler.add_fragment_bytes(fragment);
        }
        self.expected_receiver_seq = seq + 1;

        let timestamp = u16::from_be_bytes([timestamps[0], timestamps[1]]);
        let timestamp_reply = u16::from_be_bytes([timestamps[2], timestamps[3]]);

        let now = Instant::now();
        if timestamp != 0xFFFF {
            let mut saved = timestamp;
            if congestion {
                // Signal the counterparty to slow down.
                saved = saved.wrapping_sub(CONGESTION_TIMESTAMP_PENALTY_MS as u16);
                if self.server {
                    tracing::info!("Received explicit congestion notification.");
                }
            }
            self.saved_timestamp = Some((saved, now));
        }

        // The only RTT measurement: how long ago the peer echoed back a
        // timestamp of ours. `timestamp_diff` keeps the 16-bit wraparound
        // signed, so a measurement across a rollover is not read as ~65 s,
        // and the estimator discards negative and oversized samples. Zero is
        // a real sample on a LAN, as in mosh.
        if timestamp_reply != 0xFFFF {
            let rtt = timestamp_diff(timestamp16(), timestamp_reply);
            self.rtt.update(f64::from(rtt));
            tracing::trace!("RTT measurement: {rtt}ms");
        }

        self.last_heard = now;
        self.has_remote_addr = true;

        // Roaming: re-target whenever the source differs, but only after the
        // packet has authenticated. Only the client roams.
        if self.server && self.remote_addr != Some(peer) {
            tracing::info!("Server now attached to client at {peer} (was {:?})", self.remote_addr);
            self.remote_addr = Some(peer);
        }

        let result = self.assembler.add_fragment_bytes(fragment);
        self.prune_sockets(now);
        result
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
        let any = match &self.remote_addr {
            Some(remote) => unspecified_like(remote),
            None => (std::net::Ipv4Addr::UNSPECIFIED, 0).into(),
        };
        // Mosh's `setup()` records the choice before the socket exists, so a
        // failed bind still waits a full interval before the next attempt.
        let now = Instant::now();
        self.last_port_choice = now;
        let socket = bind_udp(any)?;

        self.sockets.push(socket);
        self.prune_sockets(now);
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

    /// Mosh's `prune_sockets`: once the newest socket has been in use for
    /// `MAX_OLD_SOCKET_AGE`, the older ones are closed (late replies to them
    /// are no longer worth polling for); and never more than
    /// `MAX_PORTS_OPEN` are kept.
    fn prune_sockets(&mut self, now: Instant) {
        if self.sockets.len() <= 1 {
            return;
        }
        if now.saturating_duration_since(self.last_port_choice)
            > Duration::from_millis(MAX_OLD_SOCKET_AGE_MS)
        {
            let newest = self.sockets.len() - 1;
            self.sockets.drain(..newest);
        }
        if self.sockets.len() > MAX_PORTS_OPEN {
            let excess = self.sockets.len() - MAX_PORTS_OPEN;
            self.sockets.drain(..excess);
        }
    }

    pub fn time_since_last_heard(&self, now: Instant) -> std::time::Duration {
        now.duration_since(self.last_heard)
    }

    /// Get the current send sequence number (for testing).
    pub fn send_seq(&self) -> u64 {
        self.send_seq
    }

    /// The `timestamp_reply` for the next outgoing packet. Mosh's
    /// `new_packet`: a timestamp received in the last second is echoed once,
    /// advanced by how long we held it, and then forgotten, so only the first
    /// packet after a receive carries an RTT sample for the peer.
    fn take_timestamp_reply(&mut self, now: Instant) -> u16 {
        match self.saved_timestamp.take() {
            Some((ts, at)) => {
                let held = now.saturating_duration_since(at).as_millis();
                if held < 1000 {
                    ts.wrapping_add(held as u16)
                } else {
                    0xFFFF
                }
            }
            None => 0xFFFF,
        }
    }

    /// Frame and encrypt a single fragment (for tests).
    /// Stock mosh message format: [timestamp 2B BE] [timestamp_reply 2B BE] [fragment data]
    pub fn encrypt_fragment(&mut self, frag: &Fragment) -> Result<Vec<u8>, String> {
        let timestamps = [timestamp16(), self.take_timestamp_reply(Instant::now())];
        let nonce = self.next_nonce()?;
        let bytes = frag.to_bytes();
        let mut wire = Vec::new();
        frame_and_seal(
            &mut self.session,
            &mut wire,
            &nonce,
            timestamps,
            &[],
            &bytes,
        );
        Ok(wire)
    }
}

/// Poll one socket for a datagram.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn poll_recv(
    socket: &UdpSocket,
    cx: &mut std::task::Context<'_>,
    buf: &mut [u8],
) -> Poll<std::io::Result<(usize, SocketAddr, bool)>> {
    loop {
        if let Err(e) = std::task::ready!(socket.poll_recv_ready(cx)) {
            return Poll::Ready(Err(e));
        }
        match socket.try_io(tokio::io::Interest::READABLE, || ecn::recv(socket, buf)) {
            // Readiness was stale and has been cleared; poll again so the
            // waker is registered.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            result => return Poll::Ready(result),
        }
    }
}

/// Poll one socket for a datagram (no ECN information on this platform).
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn poll_recv(
    socket: &UdpSocket,
    cx: &mut std::task::Context<'_>,
    buf: &mut [u8],
) -> Poll<std::io::Result<(usize, SocketAddr, bool)>> {
    let mut read = ReadBuf::new(buf);
    socket
        .poll_recv_from(cx, &mut read)
        .map_ok(|peer| (read.filled().len(), peer, false))
}

/// Timestamp diff accounting for 16-bit wrapping.
pub fn timestamp_diff(a: u16, b: u16) -> i32 {
    a.wrapping_sub(b) as i16 as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fragment::Fragmenter;
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

    /// Encrypt a client→server packet with chosen timestamps, bypassing
    /// `encrypt_fragment`, so tests can control what the server sees.
    fn client_packet(k: [u8; 16], seq: u64, ts: u16, ts_reply: u16) -> Vec<u8> {
        let frag = Fragmenter::new()
            .make_fragments(&instruction(1), max_fragment_payload(MTU_IPV4))
            .unwrap()
            .remove(0);
        let mut text = Vec::new();
        text.extend_from_slice(&ts.to_be_bytes());
        text.extend_from_slice(&ts_reply.to_be_bytes());
        text.extend_from_slice(&frag.to_bytes());
        Session::new(k).encrypt(&morsh_crypto::Message::new(Nonce::from_val(seq), text))
    }

    /// The `timestamp_reply` field of a packet the server sent.
    fn reply_of(k: [u8; 16], wire: &[u8]) -> u16 {
        let msg = Session::new(k).decrypt(wire).unwrap();
        u16::from_be_bytes([msg.text[2], msg.text[3]])
    }

    /// Mosh's `new_packet` echoes a received timestamp once and then forgets
    /// it; repeating it on every packet for a second would feed the peer a
    /// stream of samples for one measurement.
    #[tokio::test]
    async fn received_timestamp_is_echoed_exactly_once() {
        let k = key();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut server = Connection::new_server(addr, Session::new(k)).await.unwrap();
        let peer: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        server
            .process_incoming(&client_packet(k, 0, 1234, 0xFFFF), peer)
            .unwrap();

        let frag = server
            .fragger
            .make_fragments(&instruction(1), max_fragment_payload(MTU_IPV4))
            .unwrap()
            .remove(0);
        let first = server.encrypt_fragment(&frag).unwrap();
        let second = server.encrypt_fragment(&frag).unwrap();
        let echoed = reply_of(k, &first);
        assert!(
            (1234..1234 + 50).contains(&echoed),
            "echo {echoed} should be 1234 advanced by the hold time"
        );
        assert_eq!(reply_of(k, &second), 0xFFFF, "echoed a second time");
    }

    /// 0 is an ordinary timestamp (only 0xFFFF means "none").
    #[tokio::test]
    async fn timestamp_zero_is_echoed() {
        let k = key();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut server = Connection::new_server(addr, Session::new(k)).await.unwrap();
        let peer: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        server
            .process_incoming(&client_packet(k, 0, 0, 0xFFFF), peer)
            .unwrap();
        let frag = server
            .fragger
            .make_fragments(&instruction(1), max_fragment_payload(MTU_IPV4))
            .unwrap()
            .remove(0);
        let wire = server.encrypt_fragment(&frag).unwrap();
        assert!(reply_of(k, &wire) < 50);
    }

    /// No timestamp received yet: nothing to echo.
    #[tokio::test]
    async fn no_echo_before_anything_is_received() {
        let k = key();
        let mut client = Connection::new_client(Session::new(k)).await.unwrap();
        let frag = client
            .fragger
            .make_fragments(&instruction(1), max_fragment_payload(MTU_IPV4))
            .unwrap()
            .remove(0);
        let wire = client.encrypt_fragment(&frag).unwrap();
        assert_eq!(reply_of(k, &wire), 0xFFFF);
    }

    /// An echo of our own timestamp is an RTT sample.
    #[tokio::test]
    async fn echoed_timestamp_updates_the_rtt_estimate() {
        let k = key();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut server = Connection::new_server(addr, Session::new(k)).await.unwrap();
        let peer: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let sent = timestamp16().wrapping_sub(80);
        server
            .process_incoming(&client_packet(k, 0, 0xFFFF, sent), peer)
            .unwrap();
        let srtt = server.srtt();
        assert!((80.0..200.0).contains(&srtt), "srtt {srtt}");
    }

    fn ago(d: Duration) -> Instant {
        Instant::now()
            .checked_sub(d)
            .expect("monotonic clock too young")
    }

    /// Mosh's server stops sending to a client it has not heard from for
    /// SERVER_ASSOCIATION_TIMEOUT, and re-attaches when the client returns.
    #[tokio::test]
    async fn server_detaches_from_a_silent_client() {
        let k = key();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut server = Connection::new_server(addr, Session::new(k)).await.unwrap();
        let peer: SocketAddr = "127.0.0.1:9".parse().unwrap();
        server
            .process_incoming(&client_packet(k, 0, 1, 0xFFFF), peer)
            .unwrap();
        assert!(server.has_remote());

        server.last_heard = ago(Duration::from_millis(SERVER_ASSOCIATION_TIMEOUT_MS + 1000));
        server.send(&instruction(1)).await.unwrap();
        assert!(!server.has_remote(), "server should detach");

        let seq = server.send_seq();
        server.send(&instruction(2)).await.unwrap();
        assert_eq!(server.send_seq(), seq, "detached server must not send");

        server
            .process_incoming(&client_packet(k, 1, 2, 0xFFFF), peer)
            .unwrap();
        assert!(server.has_remote(), "a fresh packet re-attaches");
    }

    /// Mosh closes the older sockets once the newest port has been in use
    /// for MAX_OLD_SOCKET_AGE; polling them forever is wasted work.
    #[tokio::test]
    async fn old_sockets_are_closed_once_the_new_port_has_aged() {
        let k = key();
        let mut client = Connection::new_client(Session::new(k)).await.unwrap();
        client.set_remote_addr("127.0.0.1:9".parse().unwrap());
        client.hop_port().await.unwrap();
        client.hop_port().await.unwrap();
        assert_eq!(client.socket_count(), 3);
        let newest = client.local_addr();

        // A reply arrives before the new port is old enough: keep them all.
        let server_packet = |seq: u64| {
            let mut text = vec![0xFF, 0xFF, 0xFF, 0xFF];
            text.extend_from_slice(
                &Fragmenter::new()
                    .make_fragments(&instruction(1), 500)
                    .unwrap()[0]
                    .to_bytes(),
            );
            Session::new(k).encrypt(&morsh_crypto::Message::new(
                Nonce::from_val(TO_CLIENT_BIT | seq),
                text,
            ))
        };
        let from: SocketAddr = "127.0.0.1:9".parse().unwrap();
        client.process_incoming(&server_packet(0), from).unwrap();
        assert_eq!(client.socket_count(), 3);

        client.last_port_choice = ago(Duration::from_millis(MAX_OLD_SOCKET_AGE_MS + 1000));
        client.process_incoming(&server_packet(1), from).unwrap();
        assert_eq!(client.socket_count(), 1);
        assert_eq!(client.local_addr(), newest, "the newest socket survives");
    }

    #[tokio::test]
    async fn at_most_max_ports_open() {
        let mut client = Connection::new_client(Session::new(key())).await.unwrap();
        client.set_remote_addr("127.0.0.1:9".parse().unwrap());
        for _ in 0..MAX_PORTS_OPEN + 5 {
            client.hop_port().await.unwrap();
        }
        assert_eq!(client.socket_count(), MAX_PORTS_OPEN);
    }

    /// An IPv4 socket cannot reach an IPv6 server. Mosh creates the client
    /// socket in the server's family and uses the IPv6 MTU for it.
    #[tokio::test]
    async fn client_reaches_an_ipv6_server() {
        let k = key();
        let v6: SocketAddr = "[::1]:0".parse().unwrap();
        let Ok(mut server) = Connection::new_server(v6, Session::new(k)).await else {
            eprintln!("no IPv6 loopback; skipping");
            return;
        };
        assert_eq!(server.mtu(), MTU_IPV6);
        let mut client = Connection::new_client(Session::new(k)).await.unwrap();
        client.set_remote_addr(server.local_addr().unwrap());
        assert_eq!(client.mtu(), MTU_IPV6);
        assert!(client.local_addr().unwrap().is_ipv6());

        client
            .send(&instruction(1))
            .await
            .expect("send to IPv6 server");
        let inst = tokio::time::timeout(Duration::from_secs(5), server.recv())
            .await
            .expect("datagram should arrive")
            .unwrap()
            .unwrap();
        assert_eq!(inst.new_num, Some(1));

        client.hop_port().await.unwrap();
        assert!(
            client.local_addr().unwrap().is_ipv6(),
            "hop keeps the family"
        );
    }

    /// Mosh's `try_bind` clears IPV6_V6ONLY on a `::` socket.
    #[tokio::test]
    async fn ipv6_wildcard_server_socket_is_dual_stack() {
        let any: SocketAddr = "[::]:0".parse().unwrap();
        let Ok(server) = Connection::new_server(any, Session::new(key())).await else {
            eprintln!("no IPv6; skipping");
            return;
        };
        let only_v6 = socket2::SockRef::from(&server.sockets[0])
            .only_v6()
            .unwrap();
        assert!(!only_v6);
    }

    #[tokio::test]
    async fn ipv4_mtu_is_used_for_an_ipv4_peer() {
        let mut client = Connection::new_client(Session::new(key())).await.unwrap();
        client.set_remote_addr("127.0.0.1:9".parse().unwrap());
        assert_eq!(client.mtu(), MTU_IPV4);
    }

    /// Mosh attempts every fragment even when `sendto` fails; morsh used to
    /// give up on the instruction at the first error.
    #[tokio::test]
    async fn every_fragment_is_attempted_when_sendto_fails() {
        let mut client = Connection::new_client(Session::new(key())).await.unwrap();
        // Broadcast without SO_BROADCAST: every sendto fails (EACCES).
        client.set_remote_addr("255.255.255.255:9".parse().unwrap());
        let mut big = instruction(1);
        big.diff = Some(
            (0..20_000u32)
                .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
                .collect(),
        );
        let fragments = Fragmenter::new()
            .make_fragments(&big, max_fragment_payload(client.mtu()))
            .unwrap()
            .len() as u64;
        assert!(fragments > 1);

        let err = client.send(&big).await.unwrap_err();
        assert!(err.contains("sendto"), "{err}");
        assert_eq!(client.send_seq(), fragments, "every fragment attempted");
    }

    #[cfg(unix)]
    #[test]
    fn emsgsize_is_recognised() {
        assert!(is_emsgsize(&std::io::Error::from_raw_os_error(
            libc::EMSGSIZE
        )));
        assert!(!is_emsgsize(&std::io::Error::from_raw_os_error(
            libc::EACCES
        )));
    }

    /// The saved (to-be-echoed) timestamp after the server receives a
    /// client packet with timestamp 1234 over a socket marked with `tos`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    async fn saved_timestamp_after_tos(tos: u32) -> u16 {
        let k = key();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut server = Connection::new_server(addr, Session::new(k)).await.unwrap();
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        socket2::SockRef::from(&sender).set_tos(tos).unwrap();
        sender
            .send_to(
                &client_packet(k, 0, 1234, 0xFFFF),
                server.local_addr().unwrap(),
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), server.recv())
            .await
            .expect("datagram should arrive")
            .unwrap();
        server.saved_timestamp.unwrap().0
    }

    /// Mosh's ECN response: a Congestion Experienced mark makes the echoed
    /// timestamp 500 ms stale, so the peer's RTT estimate (and frame
    /// interval) rises.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[tokio::test]
    async fn congestion_experienced_penalises_the_echoed_timestamp() {
        assert_eq!(saved_timestamp_after_tos(0x02).await, 1234, "ECT(0) only");
        assert_eq!(
            saved_timestamp_after_tos(0x03).await,
            1234 - CONGESTION_TIMESTAMP_PENALTY_MS as u16,
            "CE must be penalised"
        );
    }

    /// Mosh rejects a datagram larger than its receive buffer rather than
    /// processing a truncated copy.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[tokio::test]
    async fn oversize_datagram_is_reported_as_such() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut server = Connection::new_server(addr, Session::new(key()))
            .await
            .unwrap();
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        sender
            .send_to(&[0u8; RECEIVE_MTU + 1], server.local_addr().unwrap())
            .unwrap();
        let err = tokio::time::timeout(Duration::from_secs(5), server.recv())
            .await
            .expect("datagram should arrive")
            .unwrap_err();
        assert!(err.contains("oversize"), "{err}");
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
