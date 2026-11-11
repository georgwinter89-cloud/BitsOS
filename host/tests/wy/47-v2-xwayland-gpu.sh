#!/bin/bash
# slow
# time-limit: 1200
# WY-3b on V2 in QEMU (-smp 4, 4 GiB, 1280x800): Xwayland with glamor and DRI3 on the fake 780M's render node. CONFIG.TXT
# drm=fake, bitscomp=gles, bitscomp_gl=hardware: bitscomp draws with radeonsi on the node and names it as main device, as
# on the chip, and the fake runs no shader, so the picture stays black: what counts is who was asked and what arrived.
# The gpu build is started by name: the hand-over's check-build reads its frame back and goes red on the fake (right).
# Checked (v2_xwayland_gpu.py): xdpyinfo lists DRI3 and GLX, vkcube --wsi xcb (RADV) and
# glxgears (radeonsi) get X11 windows whose frames reach bitscomp as dma-bufs, nothing refused; a node lost while Xwayland
# runs (a watched file removed) ends it, and the next client is served in software without DRI3; under Pixman Xwayland
# stays in software, with the node named too. Last, host/tests/minipc/x11gpu.sh against this boot: only the steps that need
# the real chip may fail and must, and on a Pixman desktop it must fail at DRI3. The software half, no node at all, is
# 94-xwayland.sh. Timing is never judged here.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wy-47-xwayland-gpu}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30500}"
FWD=$((LO + 47))
TOKEN="wy3b-scenario-token-4e7a"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=wy-xwayland-gpu
export WY_GPU=gles WY_XWAYLAND=1
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;screen=1280x800;drm=fake;bitscomp=gles;bitscomp_gl=hardware" > "$RUN_DIR/image.txt" 2>&1 || { tail -20 "$RUN_DIR/image.txt"; exit 1; }
bash host/tests/wy/tools/gpu-image.sh image "$NAME"
rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 600 --mem 4G --serial-log "$LOG" \
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
python3 -I host/tests/wy/v2_xwayland_gpu.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: X11 programs on the render node through Xwayland's glamor and DRI3 on V2, software when the node is gone"
