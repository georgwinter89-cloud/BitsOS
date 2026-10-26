#!/bin/bash
# slow
# FS-14b: a cold start reads each byte from the stick once (plan/notes/desk-minipc-w11.md "Cold vs warm start":
# Brave's first start read 4.5 GB for its 575 MB folder). The stick image is rebuilt like Georg's stick: FAT32
# with 32 KiB clusters (2100 MiB sparse: FAT32 needs >= 65525 clusters), xtask's ESP tree, plus a Brave-like
# folder \BITSOS2\brave\ with a 330 MiB "binary" and a 40 MiB .pak. One ktest boot (fs.fat.cold): the binary is
# faulted by two mappings from two threads at once, the next cold file read with read(); the stick's read bytes
# stay <= 1.2 x the cold bytes, a warm pass reads nothing, Kind::PageCache (mem.used.pagecache) grows.
set -eu
export LC_ALL=C
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/fs-31-fat-cold}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
BOOT_LINE="BdsDxe: loading"
NAME=fs-cold
IMG="target/xtask/$NAME.img"
fail() { echo "RED: $*"; exit 1; }

QPID=""
stop_own() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
        local q; q=$(cat "target/xtask/qemu-$NAME.pid" 2>/dev/null || true)
        if [ -n "$q" ]; then kill "$q" 2>/dev/null || true; fi
        kill "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    QPID=""
}
cleanup() { stop_own; rm -f "$IMG"; }
trap cleanup EXIT

boot() {
    local log="$1"
    for attempt in 1 2; do
        rm -f "$log"
        cargo xtask qemu --name "$NAME" --timeout 400 --serial-log "$log" -- -smp 4 &
        QPID=$!
        for _ in $(seq 1 600); do grep -aq "$BOOT_LINE" "$log" 2>/dev/null && break; sleep 0.1; done
        if grep -aq "$BOOT_LINE" "$log" 2>/dev/null; then break; fi
        echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
        stop_own
        [ "$attempt" = 2 ] && fail "$NAME never booted"
    done
    wait "$QPID" || true
    QPID=""
}

cargo xtask image --name "$NAME" --ktest --config-extra "ktest=fs.fat.cold;loglevel=info"
rm -f "$IMG"
mkfs.fat -F 32 -s 64 -n BITSOS -C "$IMG" $((2100 * 1024)) > /dev/null
mcopy -s -i "$IMG" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
mmd -i "$IMG" ::BITSOS2/brave
head -c $((330 << 20)) /dev/urandom > "$RUN_DIR/brave"
head -c $((40 << 20)) /dev/urandom > "$RUN_DIR/resources.pak"
mcopy -i "$IMG" "$RUN_DIR/brave" "$RUN_DIR/resources.pak" ::BITSOS2/brave/
rm -f "$RUN_DIR/brave" "$RUN_DIR/resources.pak"
CB=$(fsck.fat -n -v "$IMG" 2>/dev/null | grep -o "^ *[0-9]* bytes per cluster" | grep -o "[0-9]*" || true)
[ "$CB" = "32768" ] || fail "the stick image has $CB bytes per cluster, not 32768"

LOG="$RUN_DIR/serial.log"
boot "$LOG"
grep -a "KTEST\|fat ktest\.cold\|fat stick\.decided" "$LOG" || true
grep -aq "KTEST ok fs.fat.cold " "$LOG" || fail "fs.fat.cold did not pass"
EV=$(grep -a "fat ktest.cold " "$LOG" | tail -1)
echo "$EV" | grep -q "exe=/boot/BITSOS2/brave/brave " || fail "the 330 MiB binary was not the file measured: $EV"
num() { echo "$EV" | grep -o " $1=[0-9]*" | cut -d= -f2; }
echo "GREEN: binary $(num exe_bytes) cold bytes -> $(num exe_device) from the stick; read() $(num pak_bytes) -> $(num pak_device); warm $(num warm_device); page cache +$(num pagecache_grew) bytes"
