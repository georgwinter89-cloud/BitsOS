#!/bin/bash
# slow
# WY-1a: bitscomp on V2 itself in QEMU (-smp 4, 1280x800 GOP): booted with MCP, bitscomp started through MCP
# `run` (fbdev + evdev back end), two wyclient windows, two tiles in `bitscomp ctl list`; keys and pointer go
# through the kernel's evdev path (MCP send_keys/send_mouse) and the clients print what they received; MCP
# `screen` == QEMU screendump == a reference drawn by headless bitscomp on the host with the same clients
# (tolerance, the pointer's box left out); `bitscomp ctl quit` gives the console back; a second bitscomp killed
# with SIGKILL gives it back too; started from the BusyBox shell it switches its terminal to KD_GRAPHICS and
# Ctrl+Alt+Q (typed by MCP, read by bitscomp from evdev) ends it with KD_TEXT. Timing is never judged here
# (Mini-PC list in plan/notes/wy-WY-1a.md).
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT=$PWD
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wy-40-v2}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30500}"
FWD=$((LO + 40))
TOKEN="wy1a-scenario-token-4c1d"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=wy-v2
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}

echo "== host reference: headless bitscomp with the same two clients at 1280x800"
(cd user/services/bitscomp && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)
BIN=$CARGO_TARGET_DIR/release/bitscomp
REF_DIR="$RUN_DIR/ref"
rm -rf "$REF_DIR"
mkdir -m 700 -p "$REF_DIR"
python3 -I host/tests/wy/v2_reference.py "$BIN" "$CARGO_TARGET_DIR/release/wyclient" "$REF_DIR" > "$RUN_DIR/ref.out"
cat "$RUN_DIR/ref.out"

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;screen=1280x800"
rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 420 --serial-log "$LOG" \
    -- -smp 4 -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
stop() {
    kill "$QPID" 2>/dev/null || true
    pf="target/xtask/qemu-$NAME.pid"
    if [ -f "$pf" ]; then
        pid="$(head -1 "$pf")"
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then kill "$pid" 2>/dev/null || true; fi
    fi
    wait "$QPID" 2>/dev/null || true
}
trap stop EXIT
python3 -I host/tests/wy/v2_check.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR" "$REF_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: bitscomp on V2"
