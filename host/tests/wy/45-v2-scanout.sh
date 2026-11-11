#!/bin/bash
# slow
# WY-17 on V2 in QEMU (-smp 4, 1280x800 GOP): the GLES renderer (llvmpipe) draws straight into fbdev's two windows,
# exported as dma-bufs by FBIO_BITSOS_SCANOUT (without a display engine: fb0's own RAM), and FBIOPAN_DISPLAY brings the
# frame on the screen. First boot, drm=fake, bitscomp=auto: `auto` hands over since a render node is up; MCP screen
# and QEMU's screendump of the direct path are the readback path's picture, also after maximise toggles, a pointer
# crossing the screen and a floated window; `--scanout readback` and `--renderer pixman` are said in the log; a killed
# compositor gives the console its picture back and the next one exports again; the kernel's counters saw it all.
# Second boot, drm=off: no render node, so `auto` stays with the rescue build's Pixman, and `--renderer gles` reads
# back. The scanout buffers of a real display engine exist only on the Mini-PC (plan/notes/wy-WY-17.md's check list;
# the flip itself against gx's fake engine in `cargo test -p amdgpu`). Timing is never judged here.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wy-45-scanout}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30500}"
FWD=$((LO + 45))
TOKEN="wy17-scenario-token-9d1e"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}
export WY_GPU=auto

boot() { # name drm mode
    local name=$1 drm=$2 mode=$3
    local log="$RUN_DIR/$mode-serial.log" qmp="$RUN_DIR/$mode-qmp.sock"
    cargo xtask image --name "$name" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;screen=1280x800;drm=$drm;bitscomp=auto" > "$RUN_DIR/$mode-image.txt" 2>&1 || { tail -20 "$RUN_DIR/$mode-image.txt"; exit 1; }
    bash host/tests/wy/tools/gpu-image.sh image "$name"
    rm -f "$log" "$qmp"
    mkdir -p "$RUN_DIR/$mode"
    cargo xtask qemu --name "$name" --net virtio --hostfwd "$FWD:8765" --timeout 600 --serial-log "$log" \
        -- -smp 4 -qmp "unix:$qmp,server=on,wait=off" > "$RUN_DIR/$mode-qemu.out" 2>&1 &
    QPID=$!
    NAME=$name
    trap stop EXIT
    python3 -I host/tests/wy/v2_scanout.py "$FWD" "$TOKEN" "$qmp" "$RUN_DIR/$mode" "$mode"
    if grep -aq "PANIC" "$log"; then echo "RED: the kernel panicked"; tail -20 "$log"; exit 1; fi
    stop
    trap - EXIT
}
stop() {
    kill "$QPID" 2>/dev/null || true
    pf="target/xtask/qemu-$NAME.pid"
    if [ -f "$pf" ]; then
        pid="$(head -1 "$pf")"
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then kill "$pid" 2>/dev/null || true; fi
    fi
    wait "$QPID" 2>/dev/null || true
}
boot wy-scanout fake fake
boot wy-scanout-off off off
echo "GREEN: frames straight into fbdev's windows on V2, the readback when no node is up"
