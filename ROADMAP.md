# Mosh-Rust: Windows-Friendly Mosh in Rust — Implementation Plan

## Overview

A ground-up Rust rewrite of Mosh, starting wire-compatible with stock mosh servers, targeting Windows Terminal (Win 10 1903+) as the primary platform, with cross-platform support. Both client and server, replacing the Perl wrapper with a native Rust binary.

---

## Architecture

```
mosh-rust/
├── Cargo.toml              # Workspace root
├── crates/
│   ├── mosh-crypto/        # AES-128-OCB3 encryption, key handling, PRNG
│   ├── mosh-proto/         # Protobuf definitions (prost-generated)
│   ├── mosh-terminal/      # VT processing wrapper around libghostty-vt
│   ├── mosh-network/       # UDP transport, fragmentation, RTT estimation, port hopping
│   ├── mosh-statesync/     # State diffing, sync protocol, Complete/UserStream
│   └── mosh-prediction/    # Predictive echo overlay engine
├── src/
│   ├── client.rs           # mosh-client binary (Windows + cross-platform)
│   ├── server.rs           # mosh-server binary (Windows ConPTY + Unix forkpty)
│   └── wrapper.rs          # mosh wrapper binary (replaces mosh.pl — SSH launch + connection)
└── proto/                  # Protocol Buffer definitions (copied from mosh)
    ├── transportinstruction.proto
    ├── hostinput.proto
    └── userinput.proto
```

> **Note:** Terminal emulation (VT parser, framebuffer, display) is handled by
> **libghostty-vt** via the `libghostty-rs` Rust bindings — no custom
> `mosh-terminal` crate needed. See Phase 2 details below.

---

## Crate Dependencies

| Purpose | Crate | Why |
|---------|-------|-----|
| Crypto AEAD | `ocb3` + `aes` 0.8 | Pure Rust AES-128-OCB3, matches mosh wire format (cipher 0.4 compatible) |
| Protobuf | `prost` | Mature, idiomatic Rust. prost-build for codegen. Extensions flattened (field numbers preserved) |
| Terminal emulation | `libghostty-vt` via `libghostty-rs` | Battle-tested VT parser, framebuffer, key encoding. Zero runtime deps. |
| Terminal I/O | `crossterm` | Cross-platform raw mode, input events, resize, colors |
| PTY | `portable-pty` | ConPTY on Windows, openpty on Unix. WezTerm-proven |
| Networking | `tokio` (net + time) | Async UDP via IOCP on Windows, timers for retransmit |
| SSH | `russh` | Pure Rust SSH client, async, Pageant support on Windows |
| Compression | `flate2` | zlib compression (replaces C zlib) |
| Logging | `tracing` + `tracing-subscriber` | Structured logging |

---

## Implementation Phases

### Phase 1: Foundation — Crypto + Proto + Wire Format ✅ DONE
**Goal:** Encrypt/decrypt mosh packets. Validate wire compatibility with stock mosh.

1. **`mosh-crypto` crate** ✅
   - `Session` struct: AES-128-OCB3 encrypt/decrypt with `ocb3` crate
   - `Nonce`: 12-byte nonce construction (direction bit + 63-bit sequence number, padded to 12 bytes)
   - `Base64Key`: 16-byte key ↔ 22-char base64 string (no padding)
   - `Prng`: CSPRNG using `getrandom` crate (works on Windows via BCrypt API)
   - Block counter limit (2^47) with panic on overflow

2. **`mosh-proto` crate** ✅
   - Copied 3 `.proto` files from `~/projects/mosh/src/protobufs/`
   - Prost-build codegen with flattened extensions (proto2 `extend` → direct fields)
   - Separate modules (`transport`, `host`, `client`) to avoid name conflicts
   - 4 tests: roundtrip for all 3 message types + field number verification

3. **Wire compatibility test** — TODO
   - Generate test packets using the original C++ mosh code
   - Decrypt, parse, and verify in Rust
   - Encrypt and send to C++ mosh — verify it decrypts correctly

### Phase 2: Terminal Emulation via libghostty-vt
**Goal:** VT parsing, framebuffer state, key encoding — powered by libghostty-vt.

1. **`mosh-terminal` crate** ✅
   - Wraps `libghostty-vt` for VT sequence processing
   - `MoshTerminal` struct with `write()`, `resize()`, `dimensions()`, `next_frame()`
   - Zig 0.15.2 via mise.toml, builds against local Ghostty checkout (`GHOSTTY_SOURCE_DIR`)
   - 4 tests passing: create, write, resize, frame counter

2. **Adapt for Mosh's state sync needs** ✅
   - `ScreenSnapshot` / `CellData` / `CellStyle` — owned grid for easy diffing
   - `DisplayDiff::full_redraw()` — VT escape sequences from snapshot
   - `DisplayDiff::diff()` — minimal VT diff between two snapshots (cell-by-cell)
   - `KeyMap` wrapper around libghostty-vt's `key::Encoder` — char/enter/backspace/arrows/modifiers

3. **Mosh-specific adaptations** ✅
   - Snapshot model captures full grid, cursor, colors, palette
   - VT generation uses SGR sequences for styles, CUP for cursor positioning
   - Diff algorithm skips unchanged rows, emits only changed runs
   - Style resets emitted correctly between styled/unstyled cells

### Phase 3: State Synchronization
**Goal:** The SSP (State Synchronization Protocol) — diffs, transport sender/receiver.

1. **`mosh-statesync` crate** ✅
   - `Complete`: Screen snapshot with VT diff_from() / apply_string() interface
   - `UserStream`: Queue of user events (keystrokes + resize), protobuf serialization
   - Minimal VT parser: CUP, SGR (colors/bold/italic/underline), ED, EL, CR/LF/BS
   - 6 tests: create, apply, diff, cursor, color, same-states

2. **`mosh-network` crate** ✅
   - `Connection`: UDP socket management via `tokio::net::UdpSocket`
   - `Fragmenter` / `FragmentAssembly`: zlib-compressed MTU-aware fragmentation with 10-byte headers
   - `RttEstimator`: RFC 6298 SRTT/RTTVAR with clamped RTO (50ms-1000ms)
   - ECN marking (IP_TOS ECT(0))
   - Port hopping support (client-side, 10s interval)
   - Timestamp diff with 16-bit wrapping arithmetic
   - 13 tests: fragment roundtrip, multi-fragment, assembly, chaff, RTT, RTO, send interval, timestamps

### Phase 4: Prediction Engine
**Goal:** Speculative local echo for low-latency feel.

1. **`mosh-prediction` crate**
   - `PredictionEngine`: Track per-cell predictions, cursor moves, epochs
   - `ConditionalOverlayCell`: Predicted replacement cells
   - `ConditionalCursorMove`: Predicted cursor positions
   - Confidence triggers: SRTT-based, flag-based, glitch-based with hysteresis
   - Epoch system: Tentative vs confirmed predictions, culling on mismatch
   - `NotificationEngine`: Status overlay (connection state, warnings)
   - Display modes: Always, Never, Adaptive (default)

### Phase 5: Client Binary ✅ DONE
**Goal:** `mosh-client` that can connect to a stock mosh-server.

1. **Client main loop** (`crates/mosh-client/src/main.rs`) ✅
   - Read `MOSH_KEY` from environment
   - Parse server address from command line
   - Create `Connection` with UDP socket
   - Initialize `Transport` (client orientation)
   - Enter event loop using `tokio::select!`:
     - `crossterm::EventStream` → raw keystrokes → `UserStream` → send
     - `Transport::recv_diff()` → `Complete::apply_string()` → write to stdout
     - Timer ticks → send diffs
     - Resize events → resize message
   - Raw mode + alternate screen buffer

### Phase 6: Server Binary ✅ DONE
**Goal:** `mosh-server` that creates a PTY and serves a shell.

1. **Server main loop** (`crates/mosh-server/src/main.rs`) ✅
   - Parse args (IP, port, key, command)
   - Open UDP socket, bind, print `MOSH CONNECT <IP> <PORT> <KEY>` to stdout
   - Spawn child process via PTY (`portable-pty`)
   - Enter `serve()` loop:
     - Read PTY master → feed through `Complete` → compute diff → send UDP
     - Receive UDP → decrypt keystrokes → write to PTY master
     - Handle shell exit

### Phase 7: Wrapper Binary (replaces mosh.pl)
**Goal:** Native Rust `mosh` command that SSHes to remote and launches mosh-server.

1. **SSH connection** via `russh`
   - Connect to remote host, authenticate (key, password, agent/Pageant)
   - Execute `mosh-server new` on remote
   - Read `MOSH CONNECT` line from stdout
   - Parse IP, port, base64 key

2. **Connection management**
   - Set `MOSH_KEY` environment variable
   - Spawn `mosh-client` (or invoke client code in-process via library API)
   - Handle SSH session cleanup
   - Forward locale settings, X forwarding if requested

3. **CLI** via `clap`
   - Match mosh.pl's argument style: `mosh [--port=PORT] [--ssh=COMMAND] user@host [command]`
   - `--start-server` flag to skip SSH and just start server
   - `--experimental` flag for future protocol extensions

### Phase 8: Windows Polish
**Goal:** First-class Windows experience.

1. **Windows Terminal integration**
   - Detect Windows Terminal vs classic console
   - Use VT escape sequences (Windows Terminal supports full xterm)
   - Graceful fallback for classic console (limited colors, no mouse)
   - Set console title with connection info

2. **Windows-specific networking**
   - `WSAStartup()` initialization
   - Firewall prompt handling (`netsh advfirewall`)
   - IPv4/IPv6 dual-stack (IPV6_V6ONLY socket option)
   - Handle Windows-specific UDP socket options

3. **Installer / Distribution**
   - Ship as single `.exe` (statically linked, no runtime dependencies)
   - Optional `.msi` installer via WiX or `cargo-wix`
   - Add to `PATH`, register as SSH ProxyCommand for git

---

## Key Design Decisions

### Serialization: prost without extensions
Mosh's proto2 extensions (`extend Instruction`) have only 3 fields each. Instead of fighting prost's lack of extension support, define the instruction messages with all fields directly:
```rust
// Instead of extensions, flatten into the Instruction struct
pub struct Instruction {
    pub hostbytes: Option<HostBytes>,
    pub resize: Option<ResizeMessage>,
    pub echoack: Option<EchoAck>,
}
```
This is wire-compatible since protobuf field numbers are what matters on the wire.

### Async vs sync event loop
Use `tokio` for UDP I/O and timers, but keep the terminal I/O synchronous (crossterm is sync). Use `tokio::task::spawn_blocking` for terminal reads if needed, or use a `tokio::sync::mpsc` channel between a terminal-reading thread and the async event loop.

### Cross-platform PTY abstraction
`portable-pty` handles this. The server code uses a `PtySystem` trait — on Windows it's ConPTY, on Unix it's openpty. The `serve()` loop is identical on both platforms.

### Protocol divergence strategy
Phase 1-7 maintains wire compatibility. Phase 8+ can introduce:
- Binary diff encoding (more efficient than ANSI escapes for large changes)
- Connection multiplexing
- UDP-lite for video applications
- Better prediction algorithms

---

## Estimated Effort

| Phase | Scope | Estimated Lines |
|-------|-------|----------------|
| Phase 1: Crypto + Proto ✅ | 2 crates | ~500 (done) |
| Phase 2: Terminal (libghostty-vt) ✅ | Integration + adapter | ~500 (done) |
| Phase 3: State Sync ✅ | 2 crates | ~2,000 (done) |
| Phase 4: Prediction | 1 crate | ~1,200 |
| Phase 5: Client ✅ | 1 binary | ~200 (done) |
| Phase 6: Server ✅ | 1 binary | ~250 (done) |
| Phase 7: Wrapper | 1 binary | ~600 |
| Phase 8: Windows Polish | Tweaks | ~500 |
| **Total** | | **~7,100** |

---

## Phase 1 Status: COMPLETE ✅

- Workspace: `Cargo.toml` with two member crates
- `mosh-crypto`: 17 tests passing (Session, Nonce, Base64Key, PRNG, block counter)
- `mosh-proto`: 4 tests passing (all message types + field number verification)
- Wire-compatible proto2 extensions handled via field-number-preserving flattening

## Next: Phase 2 — Terminal Emulation via libghostty-vt

1. Install Zig 0.15.x toolchain
2. Add `libghostty-rs` dependency to workspace
3. Build adapter layer: libghostty Terminal ↔ Mosh state sync interface
4. Verify dirty-cell tracking can generate minimal ANSI diffs
