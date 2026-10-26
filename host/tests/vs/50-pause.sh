#!/bin/bash
# slow
# VS-2b: the freeze pause of a snapshot under a load like Brave's (snap.fsyncs: 4 writers appending 4 KiB records
# with an fsync after each, rename-over saves; 30 automatic snapshots meanwhile) on a real /data (QEMU, 4 CPUs,
# data.img on the emulated stick as on the Mini-PC). Prints the pause, its parts and the device barriers inside it
# (QEMU timing is shown, not judged: the < 50 ms check is on the Mini-PC list). Afterwards the pool and the volume
# are checked on the host (e2fsck -fn).
set -eu
export LC_ALL=C
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/vs-50-pause}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
BOOT_LINE="BdsDxe: loading"
NAME=vs-pause
IMG="target/xtask/$NAME.img"
fail() { echo "RED: $*"; exit 1; }
for t in mcopy mkfs.fat e2fsck; do command -v $t > /dev/null || { echo "SKIP: $t missing"; exit 0; }; done

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

cargo build -q -p bitsvol --example export
EXPORT="${CARGO_TARGET_DIR:-target}/debug/examples/export"
[ -x "$EXPORT" ] || fail "no export example at $EXPORT"

cargo xtask image --name "$NAME" --ktest --config-extra "ktest=snap.fsyncs;data_img_mb=256;loglevel=info"
kib=$(( $(stat -c %s "$IMG") / 1024 + 900 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/

LOG="$RUN_DIR/serial.log"
for attempt in 1 2; do
    rm -f "$LOG"
    cargo xtask qemu --name "$NAME" --timeout 600 --serial-log "$LOG" -- -smp 4 &
    QPID=$!
    for _ in $(seq 1 600); do grep -aq "$BOOT_LINE" "$LOG" 2>/dev/null && break; sleep 0.1; done
    if grep -aq "$BOOT_LINE" "$LOG" 2>/dev/null; then break; fi
    echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
    stop_own
    [ "$attempt" = 2 ] && fail "$NAME never booted"
done
wait "$QPID" || true
QPID=""

grep -a "KTEST\|snap fsyncs\.\|snap take_failed\|snap freeze_failed\|commit.slow" "$LOG" || true
grep -aq "ext4 data.mounted at=/data pool=true writable=true created=true" "$LOG" || fail "/data not mounted from a new pool"
grep -aq "KTEST ok snap.fsyncs " "$LOG" || fail "snap.fsyncs"
grep -aq "KTEST done passed=1 failed=0" "$LOG" || fail "ktests not green"
line=$(grep -a "snap fsyncs.pauses" "$LOG" | tail -1)
[ -n "$line" ] || fail "no fsyncs.pauses line"
# Every pause line of the run, then the summary.
grep -a "snap taken pool=data .*label=vsfsync" "$LOG" | sed -E 's/.*(preflush_us=[0-9]+ freeze_us=[0-9]+ snap_us=[0-9]+ pause_us=[0-9]+ commit_us=[0-9]+ barriers=[0-9]+ barrier_us=[0-9]+).*/  \1/' || true
echo "pauses (QEMU, not judged): ${line#*snap fsyncs.pauses }"
# Counts, not timing: no device barrier inside a pause (the ktest asserts the sum, this names the line).
if grep -a "snap taken pool=data .*label=vsfsync" "$LOG" | grep -aqv " barriers=0 "; then
    grep -a "snap taken pool=data .*label=vsfsync" "$LOG" | grep -av " barriers=0 " | head -3
    fail "a device barrier inside a pause"
fi

out="$RUN_DIR/pool"
mkdir -p "$out"
rm -f "$out/data.img"
mcopy -n -i "$IMG" ::BITSOS2/data.img "$out/data.img" || fail "no data.img on the stick"
"$EXPORT" "$out/data.img" list > "$out/list.txt" || fail "the pool does not open on the host"
"$EXPORT" "$out/data.img" 1 "$out/vol.img" > /dev/null || fail "volume 1 not exported"
e2fsck -fn "$out/vol.img" > "$out/vol-fsck.txt" 2>&1 || { cat "$out/vol-fsck.txt"; fail "the volume is not e2fsck clean"; }
rm -f "$out/vol.img" "$out/data.img"
echo "GREEN: snap.fsyncs ran, the pool opens and the volume is e2fsck -fn clean"
