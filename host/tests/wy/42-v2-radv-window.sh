#!/bin/bash
# slow
# WY-17b (CR-GM1d-2 way b) on V2 in QEMU (-smp 4, 4 GiB, 1280x800, drm=fake: the fake 780M's render node): under Pixman
# RADV's swapchain images reach bitscomp, none refused. bitscomp names no main device (0:0), so RADV's WSI hands over a
# linear copy in GTT Pixman can map; explicit sync stays on the node. With `--main-device render` (the old way) the
# same vkcube's VRAM images are refused with EPERM, as on Linux: the difference is this one line of the feedback.
# Makes its own stage when there is none (user/ports/mesa/scripts/stage-v2.sh into the target dir, as 98-gles.sh).
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT=$PWD
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wy-42-radv-window}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30500}"
FWD=$((LO + 42))
TOKEN="wy17b-scenario-token-4a7d"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=wy-radv
whole() { [ -x "$1/run" ] && [ -f "$1/lib/libvulkan_radeon.so" ] && [ -f "$1/share/libdrm/amdgpu.ids" ]; }
STAGE=${MESA_STAGE:-$ROOT/user/ports/mesa/out/mesa}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
if ! whole "$STAGE"; then
    STAGE=$CARGO_TARGET_DIR/wy-mesa-out/mesa
    if ! whole "$STAGE"; then
        echo "== stage the Mesa tree into $STAGE"
        MESA_OUT=$CARGO_TARGET_DIR/wy-mesa-out bash user/ports/mesa/scripts/stage-v2.sh >"$RUN_DIR/stage.out" 2>&1 \
            || { echo "RED: no Mesa stage ($(tail -1 "$RUN_DIR/stage.out"))"; exit 1; }
    fi
    whole "$STAGE" || { echo "RED: the stage in $STAGE lacks RADV or share/libdrm"; exit 1; }
fi

# WY-21 made bitscomp=auto the default, and the fake node opens: auto would hand over to GLES. This scenario is Pixman's.
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;screen=1280x800;drm=fake;bitscomp=rescue" \
  > "$RUN_DIR/image.txt" 2>&1 || { tail -20 "$RUN_DIR/image.txt"; exit 1; }
ESP=$CARGO_TARGET_DIR/xtask/esp-$NAME
IMG=$CARGO_TARGET_DIR/xtask/$NAME.img
rm -rf "$ESP/BITSOS2/mesa"
cp -r "$STAGE" "$ESP/BITSOS2/mesa"
bytes=$(du -sb "$ESP" | cut -f1)
kib=$(( (bytes + bytes / 4 + (64 << 20)) / 1048576 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "$ESP/EFI" "$ESP/BITSOS2" ::/

rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --mem 4G --net virtio --hostfwd "$FWD:8765" --timeout 900 --serial-log "$LOG" \
    -- -smp 4 -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
stop() {
    kill "$QPID" 2>/dev/null || true
    pf="$CARGO_TARGET_DIR/xtask/qemu-$NAME.pid"
    if [ -f "$pf" ]; then
        pid="$(head -1 "$pf")"
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then kill "$pid" 2>/dev/null || true; fi
    fi
    wait "$QPID" 2>/dev/null || true
}
trap stop EXIT
python3 -I host/tests/wy/v2_radv_window.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: RADV's window buffers reach a Pixman bitscomp"
