#!/bin/bash
# slow
# GM-1c: the staged Mesa tree's hardware drivers (RADV, radeonsi) against the fake GPU's render node (drm=fake, the
# 780M profile) on V2 in QEMU (-smp 4, 4 GiB, 1280x800): T0 vulkaninfo, T1 vkcube in bitscomp up to its first CS,
# T2 eglinfo/eglgears_wayland with radeonsi up to its first CS, then drmreplay plays vkcube's trace again, and two
# damaged copies of it; tools/drmhostile.c makes bad calls under the tracer (v2_radv_fake.py says what each step checks). Every Mesa program runs under host/tests/gm/errlog.c and
# tools/drmtrace.c (every DRM ioctl with its payloads, tools/drmtrace.md). The fake has no drawing engine yet
# (GX-8): the tests stop at the first CS and say how far they got. No host reference run: the host's GPU drives the
# desk's display and gets no jobs from a test. T0 compares with a real 780M: host/tests/gm/ref-780m/ (committed), or
# GM_RADV_REF=<dir> with a real 780M's vulkaninfo.out/.info of a Linux run. No xtask change: tree and tools go onto the stick image
# here, the FAT image made again larger from xtask's ESP tree (as 10-mesa.sh).
# Needs the stage: `bash user/ports/mesa/scripts/stage-v2.sh`.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT=$PWD
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/gm-20-radv-fake}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-32000}"
FWD=$((LO + 70))
TOKEN="gm1c-scenario-token-81fa"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=gm-radv
STAGE=${MESA_STAGE:-$ROOT/user/ports/mesa/out/mesa}
REF=${GM_RADV_REF:--}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
[ -x "$STAGE/run" ] && [ -f "$STAGE/lib/libvulkan_radeon.so" ] && [ -f "$STAGE/share/libdrm/amdgpu.ids" ] \
  || { echo "RED: no staged Mesa (with share/libdrm) in $STAGE (bash user/ports/mesa/scripts/stage-v2.sh)"; exit 1; }
cmp -s "$STAGE/run" user/ports/mesa/bitsos/run || { echo "RED: $STAGE/run is older than user/ports/mesa/bitsos/run: stage again"; exit 1; }

W="-O2 -Wall -Wextra -Werror"
gcc $W -shared -fPIC -o "$RUN_DIR/errlog.so" host/tests/gm/errlog.c
gcc $W -shared -fPIC -I/usr/include/libdrm -o "$RUN_DIR/drmtrace.so" host/tests/gm/tools/drmtrace.c -ldl -lpthread
gcc $W -I/usr/include/libdrm -o "$RUN_DIR/drmreplay" host/tests/gm/tools/drmreplay.c
gcc $W -I/usr/include/libdrm -o "$RUN_DIR/drmhostile" host/tests/gm/tools/drmhostile.c -lpthread

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn,syscall=info,drm=debug;screen=1280x800;drm=fake" \
  > "$RUN_DIR/image.txt" 2>&1 || { tail -20 "$RUN_DIR/image.txt"; exit 1; }
ESP=$CARGO_TARGET_DIR/xtask/esp-$NAME
IMG=$CARGO_TARGET_DIR/xtask/$NAME.img
rm -rf "$ESP/BITSOS2/mesa" "$ESP/BITSOS2/gmtest"
cp -r "$STAGE" "$ESP/BITSOS2/mesa"
mkdir "$ESP/BITSOS2/gmtest"
cp "$RUN_DIR/errlog.so" "$RUN_DIR/drmtrace.so" "$RUN_DIR/drmreplay" "$RUN_DIR/drmhostile" "$ESP/BITSOS2/gmtest/"
bytes=$(du -sb "$ESP" | cut -f1)
kib=$(( (bytes + bytes / 4 + (64 << 20)) / 1048576 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "$ESP/EFI" "$ESP/BITSOS2" ::/

rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --mem 4G --net virtio --hostfwd "$FWD:8765" --timeout 1500 --serial-log "$LOG" \
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
python3 -I host/tests/gm/v2_radv_fake.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR" "$REF"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: RADV and radeonsi on the fake 780M up to their first CS; the trace replays"
