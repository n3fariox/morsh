# Testing and performance

See [the survey and gap plan](testing-plan.md) for scope and follow-up work.
Build prerequisites are Rust, protoc and Zig 0.15.2 (`mise install`); the native
Ghostty build is cached after its first build. Run from the repository root:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
./scripts/interop.sh                   # optional --release
cargo bench -p morsh-statesync --bench performance > tui-performance.csv
cargo test -p morsh-network --test transport_integration wire_performance_report -- --ignored --nocapture
cargo test -p morsh-statesync --test regression_budgets -- --nocapture
```

Interop uses Python 3 Unix PTYs and a C.UTF-8 locale. Each pairing independently
skips its missing stock binary; the morsh/morsh pairing always runs. It checks
both server output and a typed input/response, plus stock launch arguments and
port selection. Servers and clients have bounded lifetimes and owned-process
cleanup. Linux CI installs stock mosh and runs the interop suite; ordinary Rust
tests need neither stock binaries nor root/network configuration privileges.
See [fixture provenance and regeneration](../tests/fixtures/stock-mosh/README.md).

Correctness gates include offline stock exchange replay, authentication failure
for a bit flip at every packet byte, protobuf/fragment goldens, state history,
Unicode screen reconstruction, unchanged/sparse output size, and packet/byte
budgets for batched input, compressible redraws and incompressible payloads.
Budgets allow compressor variation and bound **UDP payload** bytes (nonce,
timestamps, fragment header, compressed protobuf and OCB tag), excluding IP/UDP
headers. Fixed maximum-length chaff avoids random pass/fail outcomes.

The TUI benchmark emits CSV for 120x40 emulation, output floods, scrolling,
Unicode/wide/combining text, snapshotting, unchanged/one-cell diffs, full redraw,
input batching, local input-to-render processing, compression/fragmentation,
and encryption. Each case warms up, then records 21 samples of 20 fixed-work
iterations; median and p95 are batch-average nanoseconds, not individual
keystroke tail latency. Units name bytes, cells, or local operations. Black-box
barriers retain computed results. Terminal instances are reused to model steady
state. Timing runs in Cargo's optimized bench profile and has no speed threshold.

Wide-terminal coverage models two independently addressed tmux-style panes,
a divider and an unchanged status row. CI tests use 320x80, 512x100, 513x100 and
1024x128 terminals, exercise both active halves, pane-local scrolling, combining
text/wide glyphs at pane edges, cursor positions beyond column 255, and resize
round trips. Tests require exact reconstructed screens and unchanged neighboring
cells; sparse edits must stay below 512 protobuf bytes and 256 UDP payload bytes
in one packet. Half-pane redraws have an output-size budget that rejects a
full-screen repaint. `--nocapture` reports VT/protobuf bytes and packet/UDP bytes
for every redraw, with fixed maximum-length chaff.

```sh
cargo test -p morsh-statesync --test ultrawide -- --nocapture
MORSH_BENCH_FILTER=split_ cargo bench -p morsh-statesync --bench performance
MORSH_BENCH_FILTER=split_1024x128 cargo bench -p morsh-statesync --bench performance
```

The `split_` CSV workloads compare 120x40, 320x80, 512x100 and 1024x128 screens.
They separately time pane emulation, snapshots, half-pane diffs, idle diffs,
pane-local scrolling, and a key echo in the right pane including snapshot and
state-diff generation. These larger workloads use 21 samples of **3** iterations
(the CSV records the batch size), keeping CI runtime bounded. The optional
`MORSH_BENCH_FILTER` is a substring match on workload names. This distinguishes
small rendered/wire updates from CPU costs that still scale with the whole
screen. Traces are synthetic, so these checks do not require tmux installed and
do not claim to test tmux itself. Geometry is measured in character cells;
monitor pixels and display scaling do not uniquely determine terminal geometry.

An illustrative optimized run on Linux x86_64 (Ryzen 5 5600, Rust 1.98.1,
Ghostty ReleaseSafe) produced these median milliseconds per operation:

| Geometry | Snapshot | Half-pane diff | Local key to render |
|---|---:|---:|---:|
| 120x40 | 0.34 | 0.18 | 0.59 |
| 320x80 | 1.76 | 0.93 | 3.26 |
| 512x100 | 3.60 | 1.83 | 6.57 |
| 1024x128 | 9.57 | 4.77 | 16.89 |

These are examples, not portable thresholds. The current snapshot refresh and
`DisplayDiff::diff` clone/scan screen cells even for small updates; this makes
whole-screen CPU cost visible despite sparse output. The idle-diff workload
explicitly calls the renderer and does not measure the client's idle CPU usage.

Compare the same commit/compiler/native library, idle hardware, CPU settings,
and workload across runs. Record `rustc -Vv`, `zig version`, OS and CPU with
saved reports. Use several runs before calling a small change a regression.
The local key benchmark models immediate PTY echo; it excludes OS scheduling,
network, physical terminal writes, compositor and prediction overhead.

The opt-in wire report uses the actual encrypted transports over loopback UDP.
It reports aggregate attempted packets and UDP payload bytes (both directions),
dropped/forwarded counts, first-contact time, bidirectional 32 KiB transfer
convergence time/throughput, and post-port-hop convergence time. Scenarios are
clean, 40 ms RTT, and 40 ms RTT with deterministic loss, duplication and
reordering. Drops count toward attempted traffic; forwarded includes duplicates.
Contact is the first application update, not SSH authentication. The transfer
offers 16 chunks with 60 ms of protocol pumping per chunk; its goodput reflects
this paced workload rather than maximum link capacity. Port-hop time
ends when the server receives the next input; it excludes SSH and IP mobility.
These totals include contact/transfer/roam; throughput uses transfer payload and
time only. Repeat runs vary due to real scheduling, timestamps and random chaff.

Ordinary network tests assert convergence with generous 20-second deadlines,
never latency/throughput thresholds. The combined-impairment test uses a fixed
8 KiB pseudorandom stream, drops the first packet in each direction, and
requires large datagrams and actual drops, so
fragmentation/loss cannot accidentally disappear through compression. Proxy
propagation delay is asynchronous per batch, not serialized bandwidth throttling.
CI saves both performance reports as artifacts for comparison; timing numbers
are informational, while failed correctness assertions still fail CI.

New Rust test and benchmark files are rustfmt-formatted. The existing workspace
has unrelated formatting differences under `cargo fmt --all -- --check`; avoid
including a workspace-wide formatting rewrite in performance changes.

A stress experiment with 32 KiB of incompressible input and a drop every 11th
packet stalled after 8 KiB: once a recovery instruction spans at least 11
fragments, every attempt necessarily loses one. The fragment assembler requires
a complete instruction; this is an adversarial limitation of periodic loss, not
a timing threshold to relax. The bounded convergence workload uses 8 KiB (still
multiple full-size fragments); a deterministic fragment-level regression also
checks recovery after an incomplete instruction. Larger sustained streams and
burst-loss recovery remain follow-up work.

Initial validation on Linux with stock mosh 1.4.0: 217 workspace tests passed, the one
informational wire test was ignored by default and passed when requested,
Clippy passed with warnings denied, and all five local interop pairings/launch
cases passed. Missing-stock skips were also exercised. The optional localhost
SSH case skipped because passwordless SSH was unavailable. The optimized CSV
benchmark completed; host contention makes these timings unsuitable as a
hardware baseline. CI artifacts include compiler, Zig, OS, CPU and stock version
metadata alongside the performance reports.

Ultrawide follow-up validation: 221 workspace tests passed (including the four
new split-pane tests), Clippy passed with warnings denied, and the full optimized
benchmark plus the filtered 1024x128 benchmark completed. New Rust files passed
rustfmt checking. Timing and size budgets remain separate.
