#!/bin/bash
# slow
# WY-5 on V2 in QEMU (-smp 4, 1280x800 GOP): bitscomp started through MCP `run` (fbdev/evdev, fixed status values)
# with two wyclients reads its theme from V2's settingsd (started by init). Through CL-2's MCP tools:
# settings_set theme.mode light changes the screen within 1 s, settings_set theme.accent #ffb020 too, settings_undo
# twice restores the first picture. Each picture is compared (MCP screen and QEMU screendump) with a reference drawn by
# headless bitscomp on the host with the same clients and theme (tolerance, the pointer's box left out). Timing in
# QEMU is recorded, only the 1 s bound of the brief is checked.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wy-80-theme}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30500}"
FWD=$((LO + 80))
TOKEN="wy5-scenario-token-8e2a"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=wy-theme
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}

echo "== host references: dark, light, light with the amber accent"
(cd user/services/bitscomp && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)
REF_DIR="$RUN_DIR/ref"
rm -rf "$REF_DIR"
mkdir -m 700 -p "$REF_DIR"
python3 -I host/tests/wy/v2_reference.py "$CARGO_TARGET_DIR/release/bitscomp" "$CARGO_TARGET_DIR/release/wyclient" "$REF_DIR" themes \
    > "$RUN_DIR/ref.out"
tail -3 "$RUN_DIR/ref.out"

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;screen=1280x800" > "$RUN_DIR/image.txt" 2>&1 || { tail -20 "$RUN_DIR/image.txt"; exit 1; }
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
python3 -I host/tests/wy/v2_theme.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR" "$REF_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: the theme applies live on V2"
