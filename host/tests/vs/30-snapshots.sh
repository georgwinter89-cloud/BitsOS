#!/bin/bash
# slow
# VS-2: snapshots of /data on a real boot (QEMU, 4 CPUs), one stick image of this run only (256 MiB data.img:
# CONFIG data_img_mb=256; the FAT image is made larger than xtask makes it so there is room).
#  1. Boot 1, ktest=snap.load alone: 100 snapshots while a writer rewrites, renames over and fsyncs; every
#     snapshot mounted read-only in V2 with every file whole; the snapshots stay (only for this exact filter).
#     On the host: each of the 100 taken out of data.img (bitsvol's export example) is e2fsck -fn clean and
#     needs no journal replay; the volume itself too; the freeze pauses are printed (judged on the Mini-PC).
#  2. Boot 2 of the same stick, ktest=snap.: the 100 snapshots of boot 1 are still listed after the reboot
#     (snap.data), snap.private, snap.rename_over, snap.load (deletes its own), snap.fsyncs (VS-2b), snap.user (/bin/bitssnap: take, restore of a file
#     and a folder, undo, refusals, uid 1000, serve --once); afterwards e2fsck again on every snapshot.
set -eu
export LC_ALL=C
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/vs-30-snapshots}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
BOOT_LINE="BdsDxe: loading"
NAME=vs-snap
IMG="target/xtask/$NAME.img"
fail() { echo "RED: $*"; exit 1; }
for t in mcopy mtype mkfs.fat e2fsck; do command -v $t > /dev/null || { echo "SKIP: $t missing"; exit 0; }; done

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
        cargo xtask qemu --name "$NAME" --timeout 900 --serial-log "$log" -- -smp 4 &
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

cargo build -q -p bitsvol --example export
EXPORT="${CARGO_TARGET_DIR:-target}/debug/examples/export"
[ -x "$EXPORT" ] || fail "no export example at $EXPORT"

# Every snapshot whose label starts with $2, then the volume "data" itself (id 1), e2fsck -fn on the host.
check_pool() {
    local tag="$1" prefix="$2" want="$3" out="$RUN_DIR/$1"
    mkdir -p "$out"
    rm -f "$out/data.img"
    mcopy -n -i "$IMG" ::BITSOS2/data.img "$out/data.img" || fail "$tag: no data.img on the stick"
    "$EXPORT" "$out/data.img" list > "$out/list.txt" || fail "$tag: the pool does not open on the host"
    local n; n=$(awk -v p="$prefix" '$3 == "snapshot" && index($6, p) == 1' "$out/list.txt" | wc -l)
    [ "$n" = "$want" ] || { cat "$out/list.txt"; fail "$tag: $n snapshots labelled $prefix* in the pool, want $want"; }
    "$EXPORT" "$out/data.img" fsck "$prefix" > "$out/fsck.txt" 2>&1 || { tail -40 "$out/fsck.txt"; fail "$tag: a snapshot is not clean"; }
    tail -1 "$out/fsck.txt"
    "$EXPORT" "$out/data.img" 1 "$out/vol.img" > /dev/null || fail "$tag: volume 1 not exported"
    if ! e2fsck -fn "$out/vol.img" > "$out/vol-fsck.txt" 2>&1; then
        # INT-W13: the ktest run ends like a power cut, and since RB-8 bitsjournal writes /data every few seconds
        # (its state file replaced by rename: an orphan in the running transaction). That is a legal state after a
        # cut: a copy may need only the journal replay and the orphan list a mount runs (as fs/51-data-reload.sh).
        grep -q "skipping journal recovery" "$out/vol-fsck.txt" || { cat "$out/vol-fsck.txt"; fail "$tag: the volume is not e2fsck clean"; }
        cp "$out/vol.img" "$out/vol-replay.img"
        local rc=0
        LC_ALL=C e2fsck -fy "$out/vol-replay.img" > "$out/vol-replay.txt" 2>&1 || rc=$?
        [ "$rc" -le 1 ] || { cat "$out/vol-replay.txt"; fail "$tag: the volume's replay failed"; }
        if grep -v "^Pass \|^e2fsck \|^$\|recovering journal\|: [0-9]*/[0-9]* files\|^Clearing orphaned\|^Journal \|^Backing up journal\|: \*\*\*\*\* FILE SYSTEM WAS MODIFIED \*\*\*\*\*$" "$out/vol-replay.txt" | grep -q .; then
            cat "$out/vol-replay.txt"; fail "$tag: the volume needs more than a journal replay"
        fi
        e2fsck -fn "$out/vol-replay.img" > /dev/null 2>&1 || fail "$tag: the volume is not clean after the replay"
        rm -f "$out/vol-replay.img"
        echo "ok: $tag: the volume needed only the journal replay of a cut"
    fi
    rm -f "$out/vol.img" "$out/data.img"
    echo "ok: $tag: $want snapshots and the volume e2fsck -fn clean"
}

cargo xtask image --name "$NAME" --ktest --config-extra "ktest=snap.load;data_img_mb=256;loglevel=info"
kib=$(( $(stat -c %s "$IMG") / 1024 + 900 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/

# ---- boot 1: 100 snapshots under load, kept ----
LOG="$RUN_DIR/serial-1.log"
boot "$LOG"
grep -a "KTEST\|snap load\.\|ext4 data\.mounted\|snap take_failed\|snap freeze_failed" "$LOG" || true
grep -aq "ext4 data.mounted at=/data pool=true writable=true created=true" "$LOG" || fail "boot 1: /data not mounted from a new pool"
grep -aq "KTEST ok snap.load " "$LOG" || fail "boot 1: snap.load"
grep -aq "snap load.pauses snapshots=100 .*kept=true" "$LOG" || fail "boot 1: no load.pauses line with 100 kept snapshots"
grep -aq "KTEST done passed=1 failed=0" "$LOG" || fail "boot 1: ktests not green"
taken=$(grep -ac "snap taken pool=data " "$LOG" || true)
[ "$taken" -ge 100 ] || fail "boot 1: only $taken snap taken events"
check_pool boot1 vsload 100

# ---- boot 2: all snap. tests on the same stick ----
mtype -i "$IMG" ::BITSOS2/CONFIG.TXT > "$RUN_DIR/config.txt"
# INT-W11: ab.fsfreeze (FIFREEZE/FITHAW, VS-2 CR5) runs here too, the one boot with an ext4 /data.
sed -i 's/^ktest=snap\.load$/ktest=snap.,ab.fsfreeze/' "$RUN_DIR/config.txt"
grep -q '^ktest=snap\.,ab\.fsfreeze$' "$RUN_DIR/config.txt" || fail "CONFIG.TXT has no ktest=snap.load line to change"
mcopy -o -i "$IMG" "$RUN_DIR/config.txt" ::BITSOS2/CONFIG.TXT
LOG="$RUN_DIR/serial-2.log"
boot "$LOG"
grep -a "KTEST\|snap load\.\|snap take_failed\|snap freeze_failed\|bitssnap:" "$LOG" || true
grep -aq "ext4 data.mounted at=/data pool=true writable=true created=false" "$LOG" || fail "boot 2: /data not mounted from boot 1's pool"
for t in snap.data snap.load snap.private snap.rename_over snap.rollback snap.user ab.fsfreeze; do
    grep -aq "KTEST ok $t " "$LOG" || fail "boot 2: $t"
done
# INT-W13: VS-3 added snap.rollback to the snap. filter (8 tests).
grep -aq "KTEST done passed=8 failed=0" "$LOG" || fail "boot 2: ktests not green"
# Boot 1's 100 snapshots hold space now: snap.load sizes its run by the free space (about 100 again).
grep -aEq "snap load.pauses snapshots=[0-9]{2,3} .*kept=false" "$LOG" || fail "boot 2: snap.load did not run (deleting its own)"
check_pool boot2 vsload 100
grep -a "snap load.pauses" "$RUN_DIR"/serial-*.log | sed 's/^/pauses (QEMU, not judged): /'
echo "GREEN: 100 snapshots under load e2fsck clean, kept across a reboot; bitssnap take/restore/undo on V2"
