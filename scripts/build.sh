#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$SCRIPT_DIR")"
GHOSTTY_DIR="$ROOT/third_party/ghostty"

if [ ! -f "$GHOSTTY_DIR/build.zig" ]; then
    echo "Initializing ghostty submodule..."
    git -C "$ROOT" submodule update --init --recursive third_party/ghostty
fi

if [ ! -f "$GHOSTTY_DIR/build.zig" ]; then
    echo "Error: Ghostty submodule missing at $GHOSTTY_DIR" >&2
    exit 1
fi

GHOSTTY_SOURCE_DIR="$(cd "$GHOSTTY_DIR" && pwd)"
echo "GHOSTTY_SOURCE_DIR=$GHOSTTY_SOURCE_DIR"

export GHOSTTY_SOURCE_DIR

exec cargo build --workspace "$@"
