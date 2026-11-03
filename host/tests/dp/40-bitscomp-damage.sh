#!/bin/bash
# slow
# DP-6: bitscomp on V2 in QEMU (-smp 4, 1280x800), WY-1a's whole check (host/tests/wy/v2_check.py: two windows
# with --fb-pan first, then one window; MCP `screen` == QEMU screendump == headless bitscomp's reference on the
# host), so the picture is still complete; then MCP `metrics`: with bitscomp's damage reports (change request
# plan/notes/dp-DP-6-bitscomp.patch applied) the kernel took them and copied by damage, else bitscomp ran on the
# tracked path and reported nothing. Timing is never judged here (Mini-PC list in plan/notes/dp-DP-6.md).
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT=$PWD
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/dp-40-bitscomp-damage}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-26500}"
FWD=$((LO + 40))
TOKEN="dp6-scenario-token-91e3"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=dp-bcdamage
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
if grep -q "FBIO_BITSOS_DAMAGE" user/services/bitscomp/src/backend/fbdev.rs; then WANT=damage; else WANT=tracked; fi
echo "== bitscomp reports damage: $WANT"

echo "== host reference: headless bitscomp with the same two clients at 1280x800"
(cd user/services/bitscomp && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)
REF_DIR="$RUN_DIR/ref"
rm -rf "$REF_DIR"
mkdir -m 700 -p "$REF_DIR"
python3 -I host/tests/wy/v2_reference.py "$CARGO_TARGET_DIR/release/bitscomp" "$CARGO_TARGET_DIR/release/wyclient" "$REF_DIR" > "$RUN_DIR/ref.out"

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;screen=1280x800"
rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 480 --serial-log "$LOG" \
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
python3 -I host/tests/dp/damage_check.py "$FWD" "$TOKEN" "$WANT"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: bitscomp's picture complete on V2 ($WANT)"
