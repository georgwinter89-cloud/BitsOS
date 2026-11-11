#!/bin/bash
# slow
# WY-19 on V2 in QEMU (-smp 4, 1280x800 GOP): the effect level of theme.effects on llvmpipe, as a screendump per level.
# One boot, drm=fake, bitscomp=auto, so the gpu build hands over: with `--scanout readback` and on the direct path
# (WY-17: frames straight into fbdev's two windows) the gpu level at frozen clocks, a wipe standing still and the way
# back to the cheap level must be the same pictures, MCP screen == QEMU's screendump each time; the cheap level is
# Pixman's picture within the screenshot tolerance; a still desktop at the cheap level and with motion off draws no
# frame, ambient motion draws frames. Timing is never judged here (frame times per level: plan/notes/wy-WY-19.md).
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wy-46-effects}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30500}"
FWD=$((LO + 46))
TOKEN="wy19-scenario-token-4c2a"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}
export WY_GPU=auto
NAME=wy-effects
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;screen=1280x800;drm=fake;bitscomp=auto" > "$RUN_DIR/image.txt" 2>&1 || { tail -20 "$RUN_DIR/image.txt"; exit 1; }
bash host/tests/wy/tools/gpu-image.sh image "$NAME"
rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 900 --serial-log "$LOG" \
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
python3 -I host/tests/wy/v2_effects.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR" -
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: the effect level on V2, both ways to the screen"
