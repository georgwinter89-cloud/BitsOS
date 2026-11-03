#!/bin/bash
# slow
# GM-1a: the staged Mesa tree (user/ports/mesa/scripts/stage-v2.sh) on V2 in QEMU (-smp 4, 4 GiB, 1280x800 GOP),
# software drivers only (no /dev/dri): `run --check`, `vulkaninfo --summary` (lavapipe listed), `eglinfo -B`
# (llvmpipe on the surfaceless and device platforms), then vkcube --wsi wayland, eglgears_wayland and
# es2gears_wayland in today's bitscomp over wl_shm, 20 s each, with screendumps; nothing may crash and no window may
# stay black. Every program runs with host/tests/gm/errlog.c preloaded (failed calls with their errno) and the
# kernel's ENOSYS/ENOTTY events are collected; the same programs run on the host first (headless bitscomp, no
# /dev/dri in a user namespace) so the report names only what V2 adds (v2_mesa.py). No xtask change: the tree and
# the logger are put onto the stick image here, the FAT image made again larger from xtask's ESP tree (as fs/50-data.sh).
# Needs the stage: `bash user/ports/mesa/scripts/stage-v2.sh` (7 s from pacman's cache).
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT=$PWD
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/gm-10-mesa}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-32000}"
FWD=$((LO + 60))
TOKEN="gm1a-scenario-token-3c9d"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=gm-mesa
STAGE=${MESA_STAGE:-$ROOT/user/ports/mesa/out/mesa}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
[ -x "$STAGE/run" ] && [ -f "$STAGE/lib/ld-linux-x86-64.so.2" ] \
  || { echo "RED: no staged Mesa in $STAGE (bash user/ports/mesa/scripts/stage-v2.sh)"; exit 1; }
cmp -s "$STAGE/run" user/ports/mesa/bitsos/run || { echo "RED: $STAGE/run is older than user/ports/mesa/bitsos/run: stage again"; exit 1; }

gcc -O2 -Wall -Wextra -Werror -shared -fPIC -o "$RUN_DIR/errlog.so" host/tests/gm/errlog.c

echo "== host reference: headless bitscomp, the staged tree, no /dev/dri"
(cd user/services/bitscomp && cargo build --release -q --offline)
bash host/tests/gm/tools/host-ref.sh "$RUN_DIR/host" "$STAGE" "$RUN_DIR/errlog.so"

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn,syscall=info;screen=1280x800"
# The tree at \BITSOS2\mesa\ and the logger at \BITSOS2\gmtest\, then the FAT image again with room for them.
ESP=$CARGO_TARGET_DIR/xtask/esp-$NAME
IMG=$CARGO_TARGET_DIR/xtask/$NAME.img
rm -rf "$ESP/BITSOS2/mesa" "$ESP/BITSOS2/gmtest"
cp -r "$STAGE" "$ESP/BITSOS2/mesa"
mkdir "$ESP/BITSOS2/gmtest"
cp "$RUN_DIR/errlog.so" "$ESP/BITSOS2/gmtest/errlog.so"
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
python3 -I host/tests/gm/v2_mesa.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR" "$RUN_DIR/host"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: Mesa (llvmpipe, lavapipe) on V2"
