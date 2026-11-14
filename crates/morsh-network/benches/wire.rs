//! Fixed-work benchmarks of the datagram hot path, without a framework.
//!
//! Reports median/p95 nanoseconds and heap allocations per operation. Times
//! are informational; the allocation counts are deterministic.
//!
//! Run: `cargo bench -p morsh-network --bench wire`
//! (`MORSH_BENCH_FILTER=recv` to select workloads).

use morsh_crypto::{Base64Key, Session};
use morsh_network::{constants::*, fragment::Fragmenter, Connection};
use morsh_proto::transport::Instruction;
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const ITERATIONS: usize = 200;
const SAMPLES: usize = 21;
const WARMUP: usize = 20;
/// Every call `measure` makes to its closure.
const CALLS: usize = WARMUP + SAMPLES * ITERATIONS;

fn measure(name: &str, mut work: impl FnMut()) {
    if std::env::var("MORSH_BENCH_FILTER").is_ok_and(|f| !name.contains(&f)) {
        return;
    }
    for _ in 0..WARMUP {
        work();
    }
    let allocs_before = ALLOCS.load(Ordering::Relaxed);
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            work();
        }
        samples.push(start.elapsed().as_nanos() as f64 / ITERATIONS as f64);
    }
    // The samples vector was allocated before counting started.
    let allocs =
        (ALLOCS.load(Ordering::Relaxed) - allocs_before) as f64 / (SAMPLES * ITERATIONS) as f64;
    samples.sort_by(f64::total_cmp);
    println!(
        "{name},{:.0},{:.0},{allocs:.1}",
        samples[SAMPLES / 2],
        samples[SAMPLES * 19 / 20],
    );
}

fn instruction(new_num: u64, diff: Vec<u8>) -> Instruction {
    Instruction {
        protocol_version: Some(MORSH_PROTOCOL_VERSION),
        old_num: Some(new_num.saturating_sub(1)),
        new_num: Some(new_num),
        ack_num: Some(new_num),
        throwaway_num: Some(new_num.saturating_sub(1)),
        diff: Some(diff),
        chaff: Some(vec![0x5a; 7]),
    }
}

/// A keystroke-sized diff.
fn small_diff() -> Vec<u8> {
    b"\x0a\x08\x12\x06\x22\x04ls -".to_vec()
}

/// A full-screen redraw of varied text: compresses to several fragments.
fn large_diff() -> Vec<u8> {
    let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789 .,-";
    let mut x = 0x2545_f491_u32;
    let mut d = Vec::new();
    for row in 0..60u32 {
        d.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());
        for _ in 0..200 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            d.push(alphabet[(x >> 24) as usize % alphabet.len()]);
        }
    }
    d
}

/// Every datagram `conn` would put on the wire for `inst`, handed to
/// `emit` one at a time exactly as `Connection::send` does.
fn for_each_datagram(conn: &mut Connection, inst: &Instruction, mut emit: impl FnMut(&[u8])) {
    let prepared = conn.prepare_instruction(inst).unwrap();
    for i in 0..prepared.count {
        emit(conn.seal_datagram(&prepared, i).unwrap());
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!("workload,median_ns,p95_ns,allocs_per_op");
    let key = *Base64Key::random().data();
    let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();

    for (label, diff) in [("small", small_diff()), ("large", large_diff())] {
        {
            let inst = instruction(1, diff.clone());
            let frags = Fragmenter::new()
                .make_fragments(&inst, max_fragment_payload(MTU_IPV4))
                .unwrap();
            eprintln!(
                "{label}: {} diff bytes, {} fragments",
                diff.len(),
                frags.len()
            );
        }
        // Send: fragment, compress, frame and encrypt one instruction.
        let mut client = Connection::new_client(Session::new(key)).await.unwrap();
        client.set_remote_addr(peer);
        // Building the instruction (two allocations: diff and chaff) is
        // part of each operation, as it is in the transport.
        let mut n = 0u64;
        measure(&format!("send_{label}"), || {
            n += 1;
            let inst = instruction(n, diff.clone());
            for_each_datagram(&mut client, black_box(&inst), |d| {
                black_box(d);
            });
        });

        // Receive: decrypt, validate, reassemble and decode, in order.
        let mut sender = Connection::new_client(Session::new(key)).await.unwrap();
        sender.set_remote_addr(peer);
        let datagrams: Vec<Vec<Vec<u8>>> = (1..=CALLS as u64)
            .map(|n| {
                let mut out = Vec::new();
                for_each_datagram(&mut sender, &instruction(n, diff.clone()), |d| {
                    out.push(d.to_vec())
                });
                out
            })
            .collect();
        let mut server = Connection::new_server(server_addr, Session::new(key))
            .await
            .unwrap();
        let mut next = datagrams.iter();
        measure(&format!("recv_{label}"), || {
            let mut done = None;
            for d in next.next().unwrap() {
                done = server.process_incoming(black_box(d), peer).unwrap();
            }
            black_box(done.expect("instruction completes"));
        });
    }
}
