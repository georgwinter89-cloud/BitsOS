#!/bin/bash
# slow
# WY-15: bitscomp's CPU cost per frame in QEMU, Pixman against GLES on llvmpipe (numbers only: QEMU timing judges
# nothing, and llvmpipe says nothing about the 780M). One image with the gpu build and the Mesa tree
# (tools/gpu-image.sh, CONFIG.TXT bitscomp=gpu); in one boot bitscomp runs twice through MCP `run`: `--renderer
# pixman` (the gpu build on Pixman), then `--renderer gles`, each with one animated wyclient (a new picture every
# frame) for COST_SECS (default 15) seconds. Printed per renderer: frames, bitscomp's CPU time (all its threads, from
# /proc/PID/task/*/stat) per frame, render_avg_ms (GLES: draw + readback), present_avg_ms (the fb0 copy), damaged
# pixels per frame, threads. Checked, timing-free: both pictures come up, frames are drawn, the start line names
# the renderer. COST_SCREEN=2560x1440 draws at the Mini-PC's size (default 1280x800), COST_SMP the CPUs (default 4).
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wy-99-gles-cost}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30500}"
FWD=$((LO + 99))
TOKEN="gles-cost-scenario-token-9b21"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=wy-gles-cost
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}
export WY_GPU=1
if ss -Htln "sport = :$FWD" 2>/dev/null | grep -q .; then
    echo "RED: port $FWD is in use already (a leftover QEMU?); stop it by its pid first"
    exit 1
fi
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;screen=${COST_SCREEN:-1280x800}$(bash host/tests/wy/tools/gpu-image.sh config)"
bash host/tests/wy/tools/gpu-image.sh image "$NAME"
rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 900 --mem "${COST_MEM:-4G}" --serial-log "$LOG" \
    -- -smp "${COST_SMP:-4}" -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu.out" 2>&1 &
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
python3 -I host/tests/wy/v2_cost.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: Pixman and GLES on llvmpipe both drew; cost figures above (QEMU, -smp ${COST_SMP:-4}, ${COST_SCREEN:-1280x800})"
