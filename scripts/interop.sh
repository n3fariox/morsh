#!/usr/bin/env bash
# Linux/Unix PTY interop, independent stock binary skips. See docs/testing.md.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
profile=debug
flags=()
if [[ "${1:-}" == --release ]]; then profile=release; flags=(--release); fi
cargo build --workspace "${flags[@]}"
exec python3 scripts/interop.py "${CARGO_TARGET_DIR:-target}/$profile"
