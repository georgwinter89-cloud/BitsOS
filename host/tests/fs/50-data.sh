#!/bin/bash
# slow
# FS-8c: /data from \BITSOS2\data.img on the stick, two ktest boots on one stick image of this run only (64 MiB
# image: CONFIG data_img_mb=64; the stick is made larger than xtask makes it so there is room).
#  1. First boot: no data.img yet; V2 makes it (FAT zero-fill, bitsvol pool, volume "data", V2's mkfs), mounts
#     it at /data with /data/appdata and /data/home/1000 for uid 1000; fs.ext4.data checks type/owners/
#     /proc/mounts, fs.ext4.data_write writes Brave-like files and counts the boots (1). The power-off's stop
#     pass leaves ext4 clean: the volume taken out of the pool on the host is e2fsck -fn clean.
#  2. Second boot of the same image: data.img is found and mounted (created=false), the boot counter reads 2;
#     e2fsck -fn clean again, the files of boot 1 are there (debugfs).
set -eu
export LC_ALL=C
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/fs-50-data}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
BOOT_LINE="BdsDxe: loading"
NAME=fs-data
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
trap stop_own EXIT

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

# The volume "data" out of the pool in the stick's data.img, then e2fsck -fn.
check_volume() {
    local tag="$1" out="$RUN_DIR/$1"
    mkdir -p "$out"
    rm -f "$out/data.img" "$out/vol.img"
    mcopy -n -i "$IMG" ::BITSOS2/data.img "$out/data.img" || fail "$tag: no data.img on the stick"
    BITSVOL_POOL_IMG="$out/data.img" BITSVOL_OUT="$out/vol.img" \
        cargo test -q -p fs-ext4 --lib tests::data::extract_volume -- --ignored --exact > "$out/extract.log" 2>&1 \
        || { cat "$out/extract.log"; fail "$tag: the pool in data.img does not open on the host"; }
    if ! e2fsck -fn "$out/vol.img" > "$out/fsck.log" 2>&1; then cat "$out/fsck.log"; fail "$tag: e2fsck -fn is not clean"; fi
    echo "ok: $tag: e2fsck -fn clean ($(stat -c %s "$out/data.img") bytes data.img, volume $(stat -c %s "$out/vol.img") bytes)"
}

cargo xtask image --name "$NAME" --ktest --config-extra "ktest=fs.ext4.data;data_img_mb=64;loglevel=info"
# Room for data.img (64 MiB) and the 512 MiB V2 keeps free on the stick: rebuild the FAT image larger from xtask's
# ESP tree, the way xtask makes it (mkfs.fat -F 32 -n BITSOS, mcopy).
kib=$(( $(stat -c %s "$IMG") / 1024 + 700 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
mdir -i "$IMG" ::BITSOS2/data.img > /dev/null 2>&1 && fail "the fresh stick already holds data.img"
# A creation cut half-way before (200 MiB of data.img.new): counted as used, the stick would lack the room; V2
# removes it before the room check.
truncate -s 200M "$RUN_DIR/leftover"
mcopy -i "$IMG" "$RUN_DIR/leftover" ::BITSOS2/data.img.new
rm -f "$RUN_DIR/leftover"

# ---- boot 1: data.img made ----
LOG="$RUN_DIR/serial-1.log"
boot "$LOG"
grep -a "KTEST\|ext4 data\.\|ext4 ktest\.\|ext4 mkfs\.\|fat stick\.decided" "$LOG" || true
grep -aq "fat stick.decided .*rw=true" "$LOG" || fail "boot 1: the stick was not opened for writing"
grep -aq "ext4 data.leftover_removed path=/boot/BITSOS2/data.img.new" "$LOG" || fail "boot 1: the leftover data.img.new was not removed"
grep -aq "ext4 data.created path=/boot/BITSOS2/data.img bytes=67108864" "$LOG" || fail "boot 1: data.img not made"
grep -aq "ext4 data.mounted at=/data pool=true writable=true created=true" "$LOG" || fail "boot 1: /data not mounted from the new pool"
grep -aq "KTEST ok fs.ext4.data " "$LOG" || fail "boot 1: fs.ext4.data"
grep -aq "ext4 ktest.data_write boots=1 created=true" "$LOG" || fail "boot 1: fs.ext4.data_write did not write (boots=1)"
grep -aq "KTEST done passed=3 failed=0" "$LOG" || fail "boot 1: ktests not green"
mdir -i "$IMG" ::BITSOS2/data.img.new > /dev/null 2>&1 && fail "boot 1: data.img.new left over"
FSCK=$(fsck.fat -n "$IMG" 2>&1) || fail "boot 1: fsck.fat -n not clean on the stick: $FSCK"
check_volume boot1

# ---- boot 2: data.img found ----
LOG="$RUN_DIR/serial-2.log"
boot "$LOG"
grep -a "KTEST\|ext4 data\.\|ext4 ktest\.\|ext4 journal\." "$LOG" || true
grep -aq "ext4 data.creating path=/boot/" "$LOG" && fail "boot 2: data.img made again"
grep -aq "ext4 data.mounted at=/data pool=true writable=true created=false" "$LOG" || fail "boot 2: /data not mounted from the image of boot 1"
grep -aq "ext4 journal.replayed" "$LOG" && fail "boot 2: a journal replay after a clean power-off"
grep -aq "ext4 ktest.data_write boots=2 created=false" "$LOG" || fail "boot 2: the boot counter of boot 1 was not read back (boots=2)"
grep -aq "KTEST done passed=3 failed=0" "$LOG" || fail "boot 2: ktests not green"
check_volume boot2
B=$(debugfs -R "cat /ktest-fs8c/boots" "$RUN_DIR/boot2/vol.img" 2>/dev/null)
[ "$B" = "2" ] || fail "boot 2: /ktest-fs8c/boots on the host reads '$B'"
L=$(debugfs -R "stat /ktest-fs8c/x/w/SingletonLock" "$RUN_DIR/boot2/vol.img" 2>/dev/null | grep -c 'Fast link dest: "bitsos-1"' || true)
[ "$L" = "1" ] || fail "boot 2: the SingletonLock symlink is not on the volume"
O=$(debugfs -R "stat /home/1000" "$RUN_DIR/boot2/vol.img" 2>/dev/null | grep -o "User: *1000" || true)
[ -n "$O" ] || fail "boot 2: /home/1000 is not uid 1000 on the volume"
echo "GREEN: data.img made, mounted twice, e2fsck clean after each boot"
