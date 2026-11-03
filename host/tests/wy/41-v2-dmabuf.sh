#!/bin/bash
# slow
# WY-16 on V2 in QEMU (-smp 4, 1280x800 GOP, drm=fake: the fake GPU's render node 226:128 with DR-3's sync objects):
# bitscomp through MCP `run` (fbdev/evdev) advertises zwp_linux_dmabuf_v1 (main device 226:128, XRGB8888 LINEAR
# first) and wp_linux_drm_syncobj_manager_v1 on /dev/dri/renderD128; wyclient --dmabuf makes its buffers with V2's
# /dev/udmabuf (DR-5) and its window looks like a wl_shm window's (MCP screen of the two runs within tolerance);
# a non-linear modifier is answered `failed`; with explicit sync an animating client gets its buffers back through
# release points and a commit with a late acquire point is held until it signals (bitscomp counts the wait).
# With WY_GPU=gles the same with the GLES renderer (llvmpipe imports through EGL). Timing is never judged here.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wy-41-dmabuf}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30500}"
FWD=$((LO + 41))
TOKEN="wy16-scenario-token-5b7c"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=wy-dmabuf
WY_GPU_CFG=$(bash host/tests/wy/tools/gpu-image.sh config)
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;screen=1280x800;drm=fake$WY_GPU_CFG" > "$RUN_DIR/image.txt" 2>&1 || { tail -20 "$RUN_DIR/image.txt"; exit 1; }
bash host/tests/wy/tools/gpu-image.sh image "$NAME"
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
python3 -I host/tests/wy/v2_dmabuf.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: dma-buf windows and explicit sync on V2"
