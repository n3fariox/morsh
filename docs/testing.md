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

Validation on Linux with stock mosh 1.4.0: 217 workspace tests passed, the one
informational wire test was ignored by default and passed when requested,
Clippy passed with warnings denied, and all five local interop pairings/launch
cases passed. Missing-stock skips were also exercised. The optional localhost
SSH case skipped because passwordless SSH was unavailable. The optimized CSV
benchmark completed; host contention makes these timings unsuitable as a
hardware baseline. CI artifacts include compiler, Zig, OS, CPU and stock version
metadata alongside the performance reports.
