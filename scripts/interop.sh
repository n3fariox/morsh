#!/usr/bin/env bash
#
# Interop harness: run every client/server pairing between morsh and stock
# mosh 1.4.0 and check that a command run on the server reaches the client's
# screen.
#
#   ./scripts/interop.sh [--release]
#
# Requires stock mosh (mosh-server, mosh-client) on PATH for the mixed
# pairings; those are skipped with a warning when it is missing.
#
# NOTE: mosh-server binds to the address in $SSH_CONNECTION when it is set and
# no -i is given.  This script unsets it, because otherwise the server listens
# on an address the local client cannot reach and you spend an afternoon
# chasing a phantom failure.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE=debug
CARGO_FLAGS=()
if [ "${1:-}" = "--release" ]; then
    PROFILE=release
    CARGO_FLAGS=(--release)
fi
BIN="$ROOT/target/$PROFILE"

unset SSH_CONNECTION
export LC_ALL=C.UTF-8

SENTINEL="MORSH_INTEROP_OK"
REMOTE_CMD="echo $SENTINEL; sleep 3"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"; pkill -f "$SENTINEL" 2>/dev/null' EXIT

pass=0; fail=0; skip=0

ok()   { printf '  \033[32mPASS\033[0m %s\n' "$1"; pass=$((pass+1)); }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=$((fail+1)); }
note() { printf '  \033[33mSKIP\033[0m %s\n' "$1"; skip=$((skip+1)); }

echo "Building morsh ($PROFILE)..."
cargo build "${CARGO_FLAGS[@]}" --workspace >/dev/null || { echo "build failed"; exit 1; }

have_stock_mosh=1
command -v mosh-server >/dev/null || have_stock_mosh=0
command -v mosh-client >/dev/null || have_stock_mosh=0

# start_server <label> <server-argv...>
# Writes "PORT KEY" to stdout, or nothing if the server never announced itself.
start_server() {
    local label="$1"; shift
    local out="$WORK/$label.out"
    : > "$out"
    ( "$@" >"$out" 2>"$WORK/$label.err" & )
    for _ in $(seq 1 100); do
        if grep -q '^MOSH CONNECT' "$out" 2>/dev/null; then
            awk '/^MOSH CONNECT/ { print $3, $4; exit }' "$out"
            return 0
        fi
        sleep 0.1
    done
    return 1
}

# run_client <label> <key> <command...>  — runs the client on a pty and greps
# its output for the sentinel.
run_client() {
    local label="$1"; shift
    local key="$1"; shift
    local log="$WORK/$label.client"
    MOSH_KEY="$key" MORSH_KEY="$key" \
        timeout 12 script -qec "stty rows 24 cols 80; $*" /dev/null >"$log" 2>&1
    grep -aq "$SENTINEL" "$log"
}

echo
echo "== morsh-client -> morsh-server =="
if read -r port key < <(start_server morsh-morsh "$BIN/morsh-server" new -c 256 -i 127.0.0.1 -- /bin/sh -c "$REMOTE_CMD"); then
    if run_client morsh-morsh "$key" "$BIN/morsh-client 127.0.0.1:$port"; then
        ok "morsh-client -> morsh-server"
    else
        bad "morsh-client -> morsh-server (sentinel never rendered)"
    fi
else
    bad "morsh-server did not print MOSH CONNECT"
    sed -n '1,20p' "$WORK/morsh-morsh.err" 2>/dev/null
fi

echo
echo "== morsh-client -> stock mosh-server =="
if [ "$have_stock_mosh" = 0 ]; then
    note "stock mosh not installed"
elif read -r port key < <(start_server morsh-mosh mosh-server new -c 256 -i 127.0.0.1 -- /bin/sh -c "$REMOTE_CMD"); then
    if run_client morsh-mosh "$key" "$BIN/morsh-client 127.0.0.1:$port"; then
        ok "morsh-client -> mosh-server"
    else
        bad "morsh-client -> mosh-server (sentinel never rendered)"
    fi
else
    bad "mosh-server did not print MOSH CONNECT"
fi

echo
echo "== stock mosh-client -> morsh-server =="
if [ "$have_stock_mosh" = 0 ]; then
    note "stock mosh not installed"
elif read -r port key < <(start_server mosh-morsh "$BIN/morsh-server" new -c 256 -i 127.0.0.1 -- /bin/sh -c "$REMOTE_CMD"); then
    if run_client mosh-morsh "$key" "mosh-client 127.0.0.1 $port"; then
        ok "mosh-client -> morsh-server"
    else
        bad "mosh-client -> morsh-server (sentinel never rendered)"
        sed -n '1,10p' "$WORK/mosh-morsh.client" 2>/dev/null
    fi
else
    bad "morsh-server did not print MOSH CONNECT"
fi

echo
echo "== stock mosh's launch line against morsh-server =="
# The literal argv mosh.pl builds (mosh.pl:377-392).  This used to fail with
# "error: unexpected argument '-s' found".
if read -r port key < <(start_server launch-shape "$BIN/morsh-server" new -c 256 -s -p 60111 \
        -l LANG=en_US.UTF-8 -l LC_CTYPE=en_US.UTF-8 -- /bin/sh -c "$REMOTE_CMD"); then
    if [ "$port" = 60111 ]; then
        ok "morsh-server accepts mosh.pl's argv (and honoured -p)"
    else
        bad "morsh-server ignored -p (got port $port)"
    fi
else
    bad "morsh-server rejected mosh.pl's argv"
    sed -n '1,20p' "$WORK/launch-shape.err" 2>/dev/null
fi

echo
echo "== morsh wrapper's launch line against stock mosh-server =="
if [ "$have_stock_mosh" = 0 ]; then
    note "stock mosh not installed"
else
    if read -r port key < <(start_server wrapper-shape mosh-server new -c 256 -i 127.0.0.1 \
            -l LANG=en_US.UTF-8 -- /bin/sh -c "$REMOTE_CMD"); then
        ok "stock mosh-server accepts the wrapper's argv"
    else
        bad "stock mosh-server rejected the wrapper's argv"
    fi
fi

echo
echo "== morsh wrapper over ssh to localhost =="
if ! ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=5 localhost true 2>/dev/null; then
    note "no passwordless ssh to localhost"
else
    log="$WORK/wrapper.log"
    PATH="$BIN:$PATH" timeout 20 script -qec \
        "stty rows 24 cols 80; $BIN/morsh --ssh-mode system --server $BIN/morsh-server localhost /bin/sh -c '$REMOTE_CMD'" \
        /dev/null >"$log" 2>&1
    if grep -aq "$SENTINEL" "$log"; then
        ok "morsh -> morsh-server over ssh"
    else
        bad "morsh -> morsh-server over ssh"
        sed -n '1,20p' "$log"
    fi
fi

echo
printf 'interop: %d passed, %d failed, %d skipped\n' "$pass" "$fail" "$skip"
[ "$fail" -eq 0 ]
