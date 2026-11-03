#!/bin/bash
# slow
# DR-2: the fake GPU's render node met by real libdrm and real RADV on V2 in QEMU (-smp 4, 4 GiB, drm=fake).
# drmprobe.c (libdrm's drmGetDevices2/drmGetDevice2/drmGetVersion, then libdrm_amdgpu's device set-up) runs on
# CachyOS first (the host's real amdgpu: the reference for the "drm:" lines), then on V2 through the staged Mesa
# tree's loader; then `vulkaninfo --summary` and `vulkaninfo` with RADV only must end with an exit code (no
# crash), and the kernel's `drm ioctl.unsupported` events name the first ioctl the fake does not answer yet.
# DR-7: vulkaninfo gets past memory allocation (GEM_VA into the per-fd VM); drm events at debug to see `vm.created`.
# Needs GM-1a's Mesa stage (user/ports/mesa, lane GM): MESA_STAGE=<tree> or the tree at user/ports/mesa/out/mesa.
# No xtask change: the tree and the probe go onto the stick image here, the FAT image is made again larger from
# xtask's ESP tree (as host/tests/gm/10-mesa.sh).
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT=$PWD
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/dr-20-radv}"
mkdir -p "$RUN_DIR/host"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-16000}"
FWD=$((LO + 20))
TOKEN="dr2-scenario-token-7e41"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=dr-radv
STAGE=${MESA_STAGE:-$ROOT/user/ports/mesa/out/mesa}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
[ -x "$STAGE/run" ] && [ -f "$STAGE/lib/libdrm.so.2" ] && [ -f "$STAGE/lib/libvulkan_radeon.so" ] \
  || { echo "RED: no staged Mesa tree in $STAGE (lane GM: bash user/ports/mesa/scripts/stage-v2.sh, or MESA_STAGE=)"; exit 1; }

gcc -O2 -Wall -Wextra -Werror -I/usr/include/libdrm -o "$RUN_DIR/drmprobe" host/tests/dr/drmprobe.c -ldrm -ldrm_amdgpu

echo "== host reference: drmprobe against CachyOS's amdgpu"
"$RUN_DIR/drmprobe" > "$RUN_DIR/host/drmprobe.txt"
cat "$RUN_DIR/host/drmprobe.txt"
grep -q "^drm: an AMD PCI device with a render node: yes" "$RUN_DIR/host/drmprobe.txt" \
  || { echo "RED: this host has no amdgpu render node to compare with"; exit 1; }

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn,syscall=info,drm=debug;screen=1280x800;drm=fake"
ESP=$CARGO_TARGET_DIR/xtask/esp-$NAME
IMG=$CARGO_TARGET_DIR/xtask/$NAME.img
rm -rf "$ESP/BITSOS2/mesa" "$ESP/BITSOS2/drtest"
cp -r "$STAGE" "$ESP/BITSOS2/mesa"
mkdir "$ESP/BITSOS2/drtest"
cp "$RUN_DIR/drmprobe" "$ESP/BITSOS2/drtest/drmprobe"
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
python3 -I host/tests/dr/v2_radv.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR" "$RUN_DIR/host"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: libdrm finds the fake's render node as CachyOS's; RADV maps its memory (GEM_VA) and vulkaninfo ends cleanly"
