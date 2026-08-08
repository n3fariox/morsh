# morsh — mobile (rust) shell

A Rust implementation of [Mosh](https://mosh.org) (mobile shell), a replacement for SSH that tolerates network latency, roaming, and IP address changes. Compatible with stock `mosh-server` on the wire.

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

- **Wire-compatible** with stock `mosh-server` — uses the same AES-128-OCB3 encryption, zlib compression, fragment format, and protobuf messages
- **Roaming** — survives IP address changes (port hopping)
- **Prediction engine** — local echo with adaptive display (underlines pending characters)
- **Escape sequence** — Ctrl-^ (`0x1E`) then `.` to quit; Ctrl-^ twice sends literal `0x1E`
- **Cross-platform** — Linux, macOS, Windows

## Disclaimer

This codebase was heavily AI-generated (by [opencode](https://opencode.ai) / Claude) and is not audited for correctness or security. Use at your own risk.

## License

GNU General Public License v3.0 or later — see [LICENSE](LICENSE).
