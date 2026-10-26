#!/bin/bash
# slow
# FS-13: start-up preload across a Reload, on a stick image of this run only (data_img_mb=64 for /data, a 48 MiB
# random file on the stick, preload_window_ms=5000). Boot 1 (MCP over igb, 4 CPUs): a program reads the file cold
# from the stick (usb.msd.read_bytes grows by the file), its session ends, the record is saved to /data. Reload:
# the new kernel's preload reads the recorded ranges (vfs preload.done) before anything starts; the same program
# then reads (almost) nothing from the stick. The fs.preload.* ktests run in that kernel too (an Interactive read
# during a preload pass is not delayed beyond the FS-12 limit), then MCP `preload clear` empties the record.
set -eu
export LC_ALL=C
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
if ! grep -q "vfs::preload::exec_started" crates/proc/src/exec.rs; then
    echo "SKIP: proc's execve does not call vfs::preload::exec_started yet (change request FS-13p/1, plan/notes/fs-FS-13-preload-integration.patch)"
    exit 0
fi
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/fs-52-preload}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/fs"
LO="${BITSOS_PORT_LO:-24000}"
MCP_FWD=$((LO + 150))
HTTP=$((LO + 160))
TOKEN="fs13-preload-token-81c4"
NAME=fs-preload
IMG="target/xtask/$NAME.img"
BIG_MB=48
fail() { echo "RED: $*"; exit 1; }

QPID=""
HPID=""
cleanup() {
    local qemu
    qemu=$(sed -n 's/^xtask: qemu pid \([0-9]*\).*/\1/p' "$RUN_DIR/qemu.out" 2>/dev/null | head -1)
    for p in "$QPID" "$HPID" $qemu; do
        if [ -n "$p" ] && kill -0 "$p" 2>/dev/null; then kill "$p" 2>/dev/null || true; wait "$p" 2>/dev/null || true; fi
    done
    rm -f "$IMG"
}
trap cleanup EXIT

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;data_img_mb=64;preload_window_ms=5000"
kib=$(( $(stat -c %s "$IMG") / 1024 + 700 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
head -c $((BIG_MB << 20)) /dev/urandom > "$RUN_DIR/big.bin"
mcopy -i "$IMG" "$RUN_DIR/big.bin" ::BITSOS2/preload-big.bin
rm -f "$RUN_DIR/big.bin"
# FS-15: the reader is a program on the stick (a copy of BusyBox named busybox, so it takes the applet from argv[1]),
# the exec hook; that head must be in the record and preloaded too.
BB=user/ports/busybox/out/busybox
[ -x "$BB" ] || { echo "SKIP: $BB missing (cargo xtask image builds it)"; exit 0; }
mmd -i "$IMG" ::BITSOS2/fs15
mcopy -i "$IMG" "$BB" ::BITSOS2/fs15/busybox
# Fixer F4: a launcher script that exec's the reader, the way Brave's launcher does.
printf 'exec /boot/BITSOS2/fs15/busybox cat /boot/BITSOS2/preload-big.bin > /dev/null\n' > "$RUN_DIR/launch"
mcopy -i "$IMG" "$RUN_DIR/launch" ::BITSOS2/fs15/launch
rm -f "$RUN_DIR/launch"
BUNDLES="$RUN_DIR/bundles"
rm -rf "$BUNDLES"
mkdir -p "$BUNDLES/base"
cp "target/xtask/esp-$NAME/BITSOS2/SYS/1/KERNEL" "target/xtask/esp-$NAME/BITSOS2/SYS/1/INITRD" "$BUNDLES/base/"
python3 -I -m http.server --bind 127.0.0.1 --directory "$BUNDLES" "$HTTP" > "$RUN_DIR/http.log" 2>&1 &
HPID=$!

LOG="$RUN_DIR/serial.log"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --smp 4 --net igb --hostfwd "$MCP_FWD:8765" --timeout 600 --serial-log "$LOG" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
python3 -I "$HERE/preload_scenario.py" "$MCP_FWD" "$TOKEN" "$LOG" "$HTTP" "$((BIG_MB << 20))" "$(stat -c %s "$BB")"
grep -a "vfs preload\." "$LOG" | tail -20 || true
echo "GREEN: the preload read the recorded file after the Reload; the program's next start read (almost) nothing from the stick"
