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

### Differences from `mosh`

`morsh` replaces `mosh.pl` and `morsh-server` replaces `mosh-server`. Where
either behaves differently, it is listed here; anything not listed is meant
to match `mosh`.

**Connecting over SSH**

- **Built-in SSH client for machines without `ssh`.** By default
  (`--ssh-mode auto`) `morsh` uses the system `ssh`, like `mosh`, and only
  switches to its built-in [russh](https://github.com/Eugeny/russh) client
  when no `ssh` is installed. A failing `ssh` is not retried with russh.
  `--ssh-mode system` and `--ssh-mode russh` force one or the other. The
  built-in client:
  - checks the server's host key against `~/.ssh/known_hosts`,
    `known_hosts2` and the system-wide file, asks before trusting a new host
    (or follows `StrictHostKeyChecking`), and refuses a changed key. It does
    not understand wildcard, `@cert-authority` or `@revoked` entries, or a
    custom `UserKnownHostsFile`;
  - offers key files only: no ssh-agent, Pageant, certificates or passwords;
  - reads host, user, port, key files, `HostKeyAlias` and
    `StrictHostKeyChecking` from `ssh -G` when an `ssh` binary exists, and
    can't follow `ProxyJump`/`ProxyCommand`.
- **Finding the server's address** follows `mosh`'s default
  (`--experimental-remote-ip=proxy`): system `ssh` connects through
  `morsh --fake-proxy`, which reports the address it reached; the built-in
  client records it directly. Unlike `mosh`, a `ProxyJump`/`ProxyCommand` in
  your ssh config is kept rather than replaced, and the host name is then
  resolved locally instead.
- `host:port` is accepted as the host argument as well as `user@host`.

**Starting the server**

- `morsh-server` is tried first, then stock `mosh-server`; `--server` names a
  single program instead.
- `--remote-shell auto|posix|cmd|powershell` supports Windows SSH servers.
  `auto` (the default) reads the server's SSH banner before logging in: any
  non-Windows server gets a POSIX shell, as `mosh` assumes. Only for Windows
  servers, or hosts behind a jump host, does it log in once more to run
  `uname -s` and tell `cmd` from POSIX.
- `-D`/`--no-daemonize` and `--server-log-file` are for debugging the server.

**`morsh-server`**

It accepts `mosh-server`'s command line (`new`, `-s`, `-i`, `-p`, `-c`, `-l`,
`-v`, `-@`, `--`, the legacy positional form), so stock `mosh` can use it with
`mosh --server=morsh-server` or a `mosh-server` symlink (`interop.sh` checks
the command line and the protocol, but not that whole path over ssh). At
runtime:

- **The shell is not a login shell.** `mosh-server` starts `$SHELL` as `-bash`
  (and so reads `~/.profile`); `morsh-server` starts it plainly, and falls
  back to `/bin/sh` (`cmd.exe` on Windows) rather than the passwd entry.
- **Locale:** `mosh-server` keeps the server's own locale when it is UTF-8 and
  only applies the client's `-l` variables when it is not. `morsh-server`
  always sets `LANG=en_US.UTF-8` and then the client's `-l` variables, for the
  shell only.
- **Default bind address:** with neither `-s` nor `-i`, `morsh-server` still
  binds to the address in `$SSH_CONNECTION`; `mosh-server` listens on all
  addresses (what `mosh --bind-server=any` asks for).
- `TERM` is always `xterm-256color`; `-c` is accepted but ignored
  (`mosh-server` uses `xterm` unless `-c 256`). `-v` is accepted but does
  nothing.
- No motd, no "you have a detached Mosh session" warning, no utmp entries, and
  no `cd` to `$HOME` before starting the shell.
- `MOSH_SERVER_NETWORK_TMOUT` and `MOSH_SERVER_SIGNAL_TMOUT` are ignored.
- After `MOSH CONNECT` it also prints `MOSH CONNECTION ID: 1`, and it reports
  its log file on stderr instead of `[mosh-server detached, pid = N]`.
- Additions: Windows hosts, `-D`/`--no-daemonize`, `--log-file`, and
  `MORSH_KEY` in the environment to choose the session key.

**Not supported yet**

- `--family`, `-4`, `-6`; addresses are tried IPv4 first, like
  `--family=prefer-inet`.
- `--experimental-remote-ip=local|remote`.
- `--local`, `--client`, `--no-init`, `--no-ssh-pty`, `-o`/`--predict-overwrite`,
  `--predict=experimental`, and the `-a`/`-n`/`-p` short options.
- `--port` is accepted but not yet passed to the server.
- The terminal's colour count (`mosh-server -c`) is not sent.
- `--ssh` is split on whitespace, not with shell quoting.

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
