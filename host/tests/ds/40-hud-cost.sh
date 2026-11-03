#!/bin/bash
# slow
# DS-2 on V2 in QEMU (-smp 4, 2560x1440 GOP): bitscomp started through MCP `run` (fbdev, fixed status values) with two
# wyclients. Records the cost of full redraws with the wallpaper and the HUD kit (theme change, workspace switch to
# the start page and back) and of the busy ring (numbers only: timing is never judged in QEMU); checks that a busy
# ring asks for at most 10 frames a second and that the still desktop with an idle ring draws 0 frames in 10 s.
# Without CR DS-2-1 (render.rs draws the wallpaper) the frame checks still run; the costs then lack the wallpaper.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
if ! grep -q "wallpaper::elements" user/services/bitscomp/src/render.rs; then
    echo "NOTE: render.rs does not draw the wallpaper yet (CR DS-2-1): the costs below are without it; the frame checks run"
fi
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/ds-40-hud-cost}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-15000}"
FWD=$((LO + 40))
TOKEN="ds2-scenario-token-8e1f"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=ds-hud-cost
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;screen=2560x1440" > "$RUN_DIR/image.txt" 2>&1 || { tail -20 "$RUN_DIR/image.txt"; exit 1; }
rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 420 --mem 4G --serial-log "$LOG" \
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
python3 -I host/tests/ds/ds_hud_cost.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: HUD kit cost recorded on V2 (QEMU, -smp 4, 2560x1440)"
