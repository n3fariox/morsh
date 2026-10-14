# morsh — mobile (rust) shell

A Rust implementation of [Mosh](https://mosh.org) (mobile shell), a replacement for SSH that tolerates network latency, roaming, and IP address changes. Interoperable with stock mosh 1.4.0 in both directions — see [Compatibility](#compatibility).

## Quick Start

```sh
# Build
cargo build --release

# Usage (like stock mosh)
./target/release/morsh user@host

# Or start server + client manually:
./target/release/morsh-server new -p 60001
# → MOSH CONNECT 60001 <base64-key>
MORSH_KEY=<key> ./target/release/morsh-client 127.0.0.1:60001
```

The `morsh` wrapper SSHes to the remote host, starts `morsh-server`, parses the `MOSH CONNECT` line, and launches `morsh-client` locally.

## Install via mise

[mise](https://mise.jdx.dev) installs the prebuilt release binaries — no Rust toolchain, zig, or protoc needed:

```sh
# latest release
mise use -g github:n3fariox/morsh

# or a specific version
mise use -g github:n3fariox/morsh@v0.0.2
```

Or pin it in a `mise.toml`:

```toml
[tools]
"github:n3fariox/morsh" = "v0.0.2"
```

All three binaries (`morsh`, `morsh-client`, `morsh-server`) are installed onto PATH. Versions track GitHub releases; `latest` resolves to the newest published release (tags without a release are not installable).

Only x86_64 builds are published (Linux musl, Windows MSVC) — on other platforms or architectures, use the source build in [Quick Start](#quick-start). Inside this repo, `mise install` also provides the pinned build tools (`zig`, `protoc`).

## Architecture

```
┌──────────────────────┐      SSH       ┌──────────────────────┐
│   morsh (wrapper)    │ ──────────────> │  morsh-server       │
│   (morsh-wrapper)    │  spawn + config │  (SSH daemon child) │
└──────────┬───────────┘                 └──────────┬───────────┘
           │ UDP (AES-128-OCB3, zlib, mosh proto)  │
           └───────────────────────────────────────┘
     morsh-client (local terminal)
     (crossterm, prediction engine)
```

| Crate | Role |
|---|---|
| `morsh-wrapper` | SSH launcher (replaces `mosh.pl`) |
| `morsh-client` | Local terminal with prediction |
| `morsh-server` | Remote PTY host |
| `morsh-network` | UDP transport, encryption, fragments |
| `morsh-crypto` | AES-128-OCB3 (AES-NI via `aes` + `ocb3`) |
| `morsh-proto` | Protobuf messages (wire-compatible with stock mosh) |
| `morsh-terminal` | VT screen adapter (via `libghostty-vt`) |
| `morsh-statesync` | State synchronization protocol |
| `morsh-prediction` | Local echo prediction |

## Key Features

- **Wire-compatible** with stock mosh — the same AES-128-OCB3 encryption, zlib compression, fragment format, and protobuf messages
- **State Synchronization Protocol** — the sender keeps a history of the states it has sent and diffs against the one the receiver is assumed to hold, so a lost packet is recovered by the next diff rather than by retransmission; the receiver refuses duplicates and diffs whose reference state it does not hold
- **Roaming** — the server re-targets on the first authenticated packet from a new address, and the client polls every socket it has opened
- **Prediction engine** — local echo with adaptive display (underlines pending characters)
- **Escape sequence** — Ctrl-^ (`0x1E`) then `.` to quit; Ctrl-^ twice sends literal `0x1E`
- **Cross-platform** — Linux, macOS, Windows

## Compatibility

Verified against stock mosh 1.4.0 by `scripts/interop.sh`, which runs each
pairing end to end and checks that output from the remote command reaches the
client's screen:

| Pairing | Status |
|---|---|
| `morsh-client` → `morsh-server` | Works |
| `morsh-client` → stock `mosh-server` | Works |
| stock `mosh-client` → `morsh-server` | Works |
| stock `mosh`'s launch line (`mosh-server new -c 256 -s -p … -l … -- …`) | Accepted |
| `morsh`'s launch line against stock `mosh-server` | Accepted |

Run it yourself:

```sh
./scripts/interop.sh          # add --release to test the release binaries
```

Loss, duplication, reordering, and port hopping are covered by
`crates/morsh-network/tests/transport_integration.rs`, which runs two real
transports over UDP through a proxy that impairs the link.

### Known gaps

- The client does not yet implement Ctrl-^ Ctrl-Z suspend.
- Terminal replies to host queries (DA, DSR, cursor position reports) are not
  sent back to the child process; `libghostty-vt` does not surface them
  through the API morsh uses.
- A terminal state reconstructed from a screen snapshot — which happens only
  when a state deep in the receive history is written to — restores cells,
  cursor, title, and input modes, but not scrollback or the scrolling region.

## Disclaimer

This codebase was heavily AI-generated (by [opencode](https://opencode.ai) / Claude) and is not audited for correctness or security. Use at your own risk.

## License

GNU General Public License v3.0 or later — see [LICENSE](LICENSE).
