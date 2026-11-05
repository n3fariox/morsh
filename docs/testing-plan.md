# Testing survey and implementation plan

The workspace has eleven crates. Most tests are inline unit tests: crypto
round trips, protobuf round trips, fragment assembly, sender state history,
terminal rendering, input encoding, prediction, and launcher parsing.
`morsh-network/tests/transport_integration.rs` already exercises real UDP loss,
duplication, reordering, and port hopping. `scripts/interop.sh` exercises both
stock pairings but is absent from CI, only checks server output, requires both
stock binaries even for a single pairing, and uses broad process-name cleanup.
There are no benchmark targets or independent captured wire fixtures. Existing
large transport test data repeats every 97 bytes and compresses extremely well.

Priority work:

1. Capture stock 1.4.0 traffic with provenance and a regeneration tool; replay
   fixed packets offline through OCB authentication, timestamps, fragment/zlib,
   transport protobuf, and terminal/user state decoding. Keep fixture bytes
   independent of the implementation and test corruption rejection.
2. Add fixed-work release benchmarks for output floods, redraws, scrolls,
   Unicode, screen diffs, input serialization, local input-to-render processing,
   and encrypted wire encoding. Report timing without wall-clock pass/fail gates.
3. Add deterministic packet/byte budgets and truly incompressible fragmented
   payload coverage; instrument the existing UDP proxy for loss, delay, traffic,
   convergence throughput, initial contact, and roaming measurements. Assert
   correctness with generous deadlines; make timing runs opt-in.
4. Strengthen interop with an input/response exchange, independent binary skips,
   bounded owned-process cleanup, and a Linux CI lane with stock mosh installed.
5. Document commands, benchmark interpretation, fixture provenance and limits.

Later work: a virtual-clock transport simulator (production transport currently
uses std::time::Instant), more stock versions, captured multi-fragment stock
sessions, sustained real PTY keystroke latency distributions, and controlled
hardware baselines. None of these first steps establishes full compatibility.
