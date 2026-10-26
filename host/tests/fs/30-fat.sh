#!/bin/bash
# slow
# FS-7: FAT32 on the boot stick, two boots on image files of this run only.
#  1. ktest=fs.fat. with a second USB disk "other.img" (FAT32, label BITSOS, the same STICK.ID text, but its own
#     boot sector): the boot stick is mounted at /boot read-write (identity confirmed), other.img is not the
#     stick and stays byte for byte as it was. fs.fat.write leaves \BITSOS2\FS7CHECK.TXT: afterwards fsck.fat -n
#     is clean on the boot image, mtype reads the file, SYS\1\ and \EFI\ are unchanged.
#  2. ktest=fs.fat.doubt with a clone of the boot image on a second port: two disks match bitsboot's block 0,
#     so /boot is read-only and neither image changes.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/fs-30-fat}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
BOOT_LINE="BdsDxe: loading"
SUM='
import hashlib, sys
print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())
'
sum() { python3 -I -c "$SUM" "$1"; }
fail() { echo "RED: $*"; exit 1; }

QPID=""
NAME=""
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

# boot NAME LOG extra-qemu-args...
boot() {
    local log="$2"; shift 2
    for attempt in 1 2; do
        rm -f "$log"
        cargo xtask qemu --name "$NAME" --timeout 300 --serial-log "$log" -- "$@" &
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

# ---- boot 1: the stick read-write, another BITSOS-labelled FAT disk next to it ----
NAME=fs-fat
LOG="$RUN_DIR/serial-1.log"
cargo xtask image --name "$NAME" --ktest --config-extra "ktest=fs.fat.;loglevel=info"
IMG="target/xtask/$NAME.img"
mtype -i "$IMG" ::/BITSOS2/STICK.ID > "$RUN_DIR/stick.id"
mtype -i "$IMG" ::/BITSOS2/SYS/1/KERNEL > "$RUN_DIR/kernel.before"
mtype -i "$IMG" ::/EFI/BOOT/BOOTX64.EFI > "$RUN_DIR/loader.before"
rm -f "$RUN_DIR/other.img"
mkfs.fat -F 32 -n BITSOS -C "$RUN_DIR/other.img" 70000 >/dev/null
mmd -i "$RUN_DIR/other.img" ::/BITSOS2
mcopy -i "$RUN_DIR/other.img" "$RUN_DIR/stick.id" ::/BITSOS2/STICK.ID
OTHER_SUM=$(sum "$RUN_DIR/other.img")
boot "$NAME" "$LOG" \
    -drive "if=none,id=dother,format=raw,file=$RUN_DIR/other.img" \
    -device qemu-xhci,id=xf \
    -device usb-storage,bus=xf.0,port=1,drive=dother
grep -a "KTEST\|fat stick\.\|fat ktest\.\|block disk\.registered" "$LOG" || true
grep -aq "KTEST done passed=.* failed=0" "$LOG" || fail "ktests not green"
for t in mounted tree refusals write sysapi gate; do
    grep -aq "KTEST ok fs.fat.$t " "$LOG" || fail "fs.fat.$t did not pass"
done
grep -aq "fat stick.decided .*rw=true" "$LOG" || fail "the stick was not opened for writing"
grep -aq "block disk.registered disk=msd[0-9]* kind=usb .*stick=no" "$LOG" || fail "other.img was not told apart from the stick"
FSCK=$(fsck.fat -n "$IMG" 2>&1) || fail "fsck.fat -n is not clean on the boot image: $FSCK"
echo "$FSCK"
echo "$FSCK" | grep -q "Free cluster summary" && fail "FSInfo left unknown or wrong: $FSCK"
mtype -i "$IMG" ::/BITSOS2/FS7CHECK.TXT | grep -q "written by BitsOS V2" || fail "FS7CHECK.TXT missing or wrong"
mtype -i "$IMG" ::/BITSOS2/SYS/1/KERNEL | cmp -s - "$RUN_DIR/kernel.before" || fail "SYS\\1\\KERNEL changed"
mtype -i "$IMG" ::/EFI/BOOT/BOOTX64.EFI | cmp -s - "$RUN_DIR/loader.before" || fail "\\EFI\\BOOT\\BOOTX64.EFI changed"
mdir -i "$IMG" ::/BITSOS2 | grep -qi "ktest-fs7" && fail "the test folder was left behind"
[ "$(sum "$RUN_DIR/other.img")" = "$OTHER_SUM" ] || fail "other.img changed"

# ---- boot 2: a clone of the boot stick next to it: read-only ----
NAME=fs-fat2
LOG="$RUN_DIR/serial-2.log"
cargo xtask image --name "$NAME" --ktest --config-extra "ktest=fs.fat.doubt;loglevel=info"
IMG2="target/xtask/$NAME.img"
cp "$IMG2" "$RUN_DIR/clone.img"
SUM_BOOT=$(sum "$IMG2")
SUM_CLONE=$(sum "$RUN_DIR/clone.img")
boot "$NAME" "$LOG" \
    -drive "if=none,id=dclone,format=raw,file=$RUN_DIR/clone.img" \
    -device qemu-xhci,id=xf \
    -device usb-storage,bus=xf.0,port=1,drive=dclone
grep -a "KTEST\|fat stick\.\|fat ktest\." "$LOG" || true
grep -aq "KTEST ok fs.fat.doubt " "$LOG" || fail "fs.fat.doubt did not pass"
grep -aq "fat stick.decided .*rw=false" "$LOG" || fail "two sticks: writing was not refused at the decision"
[ "$(sum "$IMG2")" = "$SUM_BOOT" ] || fail "the boot image changed with two sticks"
[ "$(sum "$RUN_DIR/clone.img")" = "$SUM_CLONE" ] || fail "the clone changed"
if grep -aq "PANIC\|lock.violation" "$RUN_DIR"/serial-*.log; then
    fail "a failure line in a log"
fi
echo "fs-30-fat: green"
