#!/bin/bash
# CL-3a on the host: the full-screen chat of `assist` on a pseudo-terminal, against a real settingsd (temporary
# stick) and the fake provider (assist_fake_api.py, recorded answers; no real provider is contacted). Typing with
# umlauts, line editing, the streamed answer, the j/n question before a settings change (the one level, EINS K2:
# Fragt vorher), the same tool list as every chat (EINS K3), scrolling, Esc/Ctrl-D.
# Port: lane range + 41.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT="$(pwd)"
command -v python3 >/dev/null || { echo "RED: python3 missing"; exit 1; }
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/cl-31-assist-tty}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
# A fresh stick each run (xtask keeps its own output.log in the run directory).
rm -rf "$RUN_DIR/stick" "$RUN_DIR/fb" "$RUN_DIR/sys" "$RUN_DIR/fake.log" "$RUN_DIR/assist-chat.log" "$RUN_DIR/s.sock"
mkdir -p "$RUN_DIR/stick" "$RUN_DIR/sys"
LO="${BITSOS_PORT_LO:-30000}"
PORT=$((LO + 41))
KEY="sk-fake-tty-5d1c-not-a-real-key"
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"

(cd user && CARGO_TARGET_DIR="$TARGET" cargo build --offline -p assist -p settingsd) > "$RUN_DIR/build.log" 2>&1 \
    || { tail -20 "$RUN_DIR/build.log"; echo "RED: build"; exit 1; }
if grep -q "^warning" "$RUN_DIR/build.log"; then grep -A6 "^warning" "$RUN_DIR/build.log" | head -30; echo "RED: compiler warnings"; exit 1; fi
BIN="$TARGET/debug"

PIDS=""
stop() { for p in $PIDS; do kill "$p" 2>/dev/null || true; done; }
trap stop EXIT
SOCK="$RUN_DIR/s.sock"
# A socket path holds 107 bytes at most.
[ "${#SOCK}" -lt 100 ] || { echo "RED: the run directory is too deep for a socket path: $SOCK"; exit 1; }
"$BIN/settingsd" serve --stick "$RUN_DIR/stick" --fallback "$RUN_DIR/fb" --sys "$RUN_DIR/sys" --socket "$SOCK" \
    > "$RUN_DIR/settingsd.log" 2>&1 &
PIDS="$PIDS $!"
python3 -I host/tests/cl/assist_fake_api.py "$PORT" "$KEY" "$RUN_DIR/fake.log" > "$RUN_DIR/fake.out" 2>&1 &
PIDS="$PIDS $!"
for _ in $(seq 100); do [ -S "$SOCK" ] && grep -q "fake AI provider" "$RUN_DIR/fake.out" 2>/dev/null && break; sleep 0.1; done
[ -S "$SOCK" ] || { cat "$RUN_DIR/settingsd.log"; echo "RED: settingsd did not start"; exit 1; }
grep -q "fake AI provider" "$RUN_DIR/fake.out" || { cat "$RUN_DIR/fake.out"; echo "RED: the fake provider did not start (port $PORT busy?)"; exit 1; }

python3 -I host/tests/cl/assist_tty_check.py "$BIN/assist" "$BIN/settings" "$SOCK" "$PORT" "$KEY" "$RUN_DIR/fake.log" "$RUN_DIR"
