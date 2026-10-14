//! End-to-end State Synchronization Protocol tests over a lossy link.
//!
//! Two real `Transport`s talk over real UDP sockets through a proxy that can
//! drop, duplicate, reorder, and delay datagrams. The whole stack is exercised:
//! encryption, nonce sequencing, compression, fragmentation, and the protocol
//! itself.
//!
//! The property under test is the one that matters: whatever the link does to
//! individual packets, both sides converge on the same state.

use morsh_crypto::{Base64Key, Session};
use morsh_network::{Connection, State, Transport};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

/// An append-only byte stream, the shape of `UserStream` without the protobuf.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Stream(Vec<u8>);

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

impl State for Stream {
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

/// What the link does to the packets crossing it.
#[derive(Clone, Copy, Default)]
struct Impairment {
    /// Drop every Nth packet in each direction (0 = never).
    drop_every: usize,
    /// Send every Nth packet twice (0 = never).
    duplicate_every: usize,
    /// Hold back every Nth packet and release it after the following one,
    /// swapping their order (0 = never).
    reorder_every: usize,
}

impl Impairment {
    fn none() -> Self {
        Self::default()
    }
}

/// One direction's worth of impairment bookkeeping.
#[derive(Default)]
struct Direction {
    count: usize,
    held: Option<Vec<u8>>,
}

/// A UDP relay between the client and the server that applies `imp`.
///
/// Deterministic rather than random: a test that fails needs to fail the same
/// way twice.
async fn spawn_proxy(server: SocketAddr, imp: Impairment) -> SocketAddr {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();

    tokio::spawn(async move {
        let mut client: Option<SocketAddr> = None;
        let mut to_server = Direction::default();
        let mut to_client = Direction::default();
        let mut buf = vec![0u8; 4096];

        loop {
            let Ok((len, from)) = sock.recv_from(&mut buf).await else {
                return;
            };
            let data = buf[..len].to_vec();

            let heading_to_server = from != server;
            if heading_to_server {
                client = Some(from);
            }
            let Some(dest) = (if heading_to_server {
                Some(server)
            } else {
                client
            }) else {
                continue;
            };
            let dir = if heading_to_server {
                &mut to_server
            } else {
                &mut to_client
            };

            dir.count += 1;
            let nth = |every: usize| every > 0 && dir.count % every == 0;

            if nth(imp.drop_every) {
                continue;
            }
            if nth(imp.reorder_every) && dir.held.is_none() {
                dir.held = Some(data);
                continue;
            }

            let _ = sock.send_to(&data, dest).await;
            if nth(imp.duplicate_every) {
                let _ = sock.send_to(&data, dest).await;
            }
            if let Some(held) = dir.held.take() {
                let _ = sock.send_to(&held, dest).await;
            }
        }
    });

    addr
}

struct Link {
    client: Transport<Stream, Stream>,
    server: Transport<Stream, Stream>,
    /// What the server has read out of the client's stream so far.
    server_received: Vec<u8>,
    /// What the client has read out of the server's stream so far.
    client_received: Vec<u8>,
}

impl Link {
    async fn new(imp: Impairment) -> Self {
        let key = *Base64Key::random().data();

        let server_conn = Connection::new_server("127.0.0.1:0".parse().unwrap(), Session::new(key))
            .await
            .unwrap();
        let server_addr = server_conn.local_addr().unwrap();

        let proxy_addr = spawn_proxy(server_addr, imp).await;

        let mut client_conn = Connection::new_client(Session::new(key)).await.unwrap();
        client_conn.set_remote_addr(proxy_addr);

        Self {
            client: Transport::new_client(client_conn, Stream::default(), Stream::default()),
            server: Transport::new_server(server_conn, Stream::default(), Stream::default()),
            server_received: Vec::new(),
            client_received: Vec::new(),
        }
    }

    fn client_sends(&mut self, bytes: &[u8]) {
        self.client
            .sender
            .current_state_mut()
            .0
            .extend_from_slice(bytes);
    }

    fn server_sends(&mut self, bytes: &[u8]) {
        self.server
            .sender
            .current_state_mut()
            .0
            .extend_from_slice(bytes);
    }

    fn drain(&mut self) {
        self.server_received
            .extend_from_slice(&self.server.get_remote_diff());
        self.client_received
            .extend_from_slice(&self.client.get_remote_diff());
    }

    /// Feed both directions a few bytes at a time, pausing in between, so
    /// that many packets carry the payload and a dropped one actually costs
    /// something. Sending everything up front would let a single surviving
    /// packet carry the whole test.
    async fn exchange_in_chunks(
        &mut self,
        client_bytes: &[u8],
        server_bytes: &[u8],
        chunks: usize,
    ) {
        for i in 0..chunks {
            let take = |b: &[u8]| {
                let per = b.len().div_ceil(chunks.max(1));
                let start = (i * per).min(b.len());
                let end = ((i + 1) * per).min(b.len());
                b[start..end].to_vec()
            };
            let c = take(client_bytes);
            let s = take(server_bytes);
            if !c.is_empty() {
                self.client_sends(&c);
            }
            if !s.is_empty() {
                self.server_sends(&s);
            }
            self.run_until(Duration::from_millis(60), |_| false).await;
        }
    }

    /// Run both endpoints until `done` holds or `budget` expires.
    async fn run_until(&mut self, budget: Duration, done: impl Fn(&Self) -> bool) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            let _ = self.client.tick().await;
            let _ = self.server.tick().await;

            tokio::select! {
                r = self.client.recv() => { let _ = r; }
                r = self.server.recv() => { let _ = r; }
                _ = tokio::time::sleep(Duration::from_millis(2)) => {}
            }

            self.drain();
            if done(self) {
                return true;
            }
        }
        self.drain();
        done(self)
    }
}

const BUDGET: Duration = Duration::from_secs(20);

fn payload(tag: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| tag.wrapping_add((i % 97) as u8)).collect()
}

/// The baseline: a clean link converges.
#[tokio::test]
async fn clean_link_converges() {
    let mut link = Link::new(Impairment::none()).await;
    let typed = payload(b'a', 64);
    link.client_sends(&typed);

    let converged = link
        .run_until(BUDGET, |l| l.server_received == typed)
        .await;

    assert!(
        converged,
        "server saw {:?} of {} bytes",
        link.server_received.len(),
        typed.len()
    );
}

/// The acceptance test from the remediation plan: drop 10% of packets in each
/// direction and require the two states to converge anyway.
///
/// This is what the assumed-receiver-state regression in `TransportSender` buys
/// us. Without a sender-side state history there is nothing to recompute a diff
/// against, and a single lost packet wedges the session permanently.
#[tokio::test]
async fn ten_percent_loss_in_both_directions_still_converges() {
    let mut link = Link::new(Impairment {
        drop_every: 10,
        ..Impairment::none()
    })
    .await;

    let typed = payload(b'a', 256);
    let printed = payload(b'A', 512);
    link.exchange_in_chunks(&typed, &printed, 16).await;

    let converged = link
        .run_until(BUDGET, |l| {
            l.server_received == typed && l.client_received == printed
        })
        .await;

    assert!(
        converged,
        "did not converge under 10% loss: server got {}/{} bytes, client got {}/{} bytes",
        link.server_received.len(),
        typed.len(),
        link.client_received.len(),
        printed.len()
    );
}

/// Heavy loss should slow convergence down, not prevent it.
#[tokio::test]
async fn thirty_percent_loss_still_converges() {
    let mut link = Link::new(Impairment {
        drop_every: 3,
        ..Impairment::none()
    })
    .await;

    let typed = payload(b'q', 128);
    link.exchange_in_chunks(&typed, &[], 8).await;

    let converged = link
        .run_until(BUDGET, |l| l.server_received == typed)
        .await;

    assert!(
        converged,
        "server saw {} of {} bytes under 33% loss",
        link.server_received.len(),
        typed.len()
    );
}

/// Every packet delivered twice must produce every byte exactly once. This is
/// the receiver's `new_num` duplicate check doing its job; without it a
/// retransmission is applied on top of the state it already produced.
#[tokio::test]
async fn duplicated_packets_are_applied_once() {
    let mut link = Link::new(Impairment {
        duplicate_every: 1,
        ..Impairment::none()
    })
    .await;

    let typed = payload(b'd', 128);
    link.exchange_in_chunks(&typed, &[], 8).await;

    let converged = link
        .run_until(BUDGET, |l| l.server_received.len() >= typed.len())
        .await;

    assert!(converged, "never received the full payload");
    assert_eq!(
        link.server_received, typed,
        "duplicate packets were applied more than once"
    );
}

/// Reordering within a window must not corrupt the state: a packet whose
/// reference state has been superseded is filed, not applied blindly.
#[tokio::test]
async fn reordered_packets_still_converge() {
    let mut link = Link::new(Impairment {
        reorder_every: 3,
        ..Impairment::none()
    })
    .await;

    let typed = payload(b'r', 192);
    let printed = payload(b'R', 192);
    link.exchange_in_chunks(&typed, &printed, 12).await;

    let converged = link
        .run_until(BUDGET, |l| {
            l.server_received == typed && l.client_received == printed
        })
        .await;

    assert!(
        converged,
        "reordering broke convergence: server {}/{}, client {}/{}",
        link.server_received.len(),
        typed.len(),
        link.client_received.len(),
        printed.len()
    );
}

/// Everything at once, plus a payload big enough to need fragmenting.
#[tokio::test]
async fn loss_duplication_and_reordering_together() {
    let mut link = Link::new(Impairment {
        drop_every: 11,
        duplicate_every: 5,
        reorder_every: 7,
    })
    .await;

    // Large enough that some instructions have to be fragmented.
    let typed = payload(b'm', 4096);
    link.exchange_in_chunks(&typed, &[], 8).await;

    let converged = link
        .run_until(BUDGET, |l| l.server_received == typed)
        .await;

    assert!(
        converged,
        "server saw {} of {} bytes",
        link.server_received.len(),
        typed.len()
    );
}

/// Input that keeps arriving while the link is lossy must all get through, in
/// order.
#[tokio::test]
async fn continuous_typing_over_a_lossy_link_converges() {
    let mut link = Link::new(Impairment {
        drop_every: 8,
        ..Impairment::none()
    })
    .await;

    let mut typed = Vec::new();
    for round in 0..8u8 {
        let chunk = payload(b'a' + round, 16);
        typed.extend_from_slice(&chunk);
        link.client_sends(&chunk);
        link.run_until(Duration::from_millis(150), |_| false).await;
    }

    let converged = link
        .run_until(BUDGET, |l| l.server_received == typed)
        .await;

    assert!(
        converged,
        "server saw {} of {} bytes",
        link.server_received.len(),
        typed.len()
    );
}

/// The client changing its source port mid-session must not end the session:
/// the server re-targets on the first authenticated packet from the new
/// address.
#[tokio::test]
async fn session_survives_a_client_port_hop() {
    let mut link = Link::new(Impairment::none()).await;

    let first = payload(b'a', 32);
    link.client_sends(&first);
    assert!(
        link.run_until(BUDGET, |l| l.server_received == first).await,
        "did not converge before the hop"
    );

    link.client.hop_port().await.unwrap();

    let second = payload(b'z', 32);
    link.client_sends(&second);
    let mut expected = first.clone();
    expected.extend_from_slice(&second);

    assert!(
        link.run_until(BUDGET, |l| l.server_received == expected).await,
        "session did not survive the port hop: server saw {} of {} bytes",
        link.server_received.len(),
        expected.len()
    );
}
