#!/bin/bash
# slow
# FS-8c: /data across Reloads and a power cut, on a stick image of this run only (data_img_mb=64, the stick made
# larger than xtask makes it). Boot 1 (MCP over igb, 4 CPUs) makes data.img and mounts /data; data_scenario.py
# runs host/tests/fs/progs/fs-data in /data over MCP against Linux's transcript (data-linux.txt), checks the type
# and /proc/mounts, then three Reloads while a shell writes into /data (each new kernel mounts /data without a
# journal replay), then the writer runs again and QEMU is killed (the cut). The volume taken out of the pool on the
# host: e2fsck -fy replays the journal and must fix nothing else. Boot 2 (ktest kernel and CONFIG swapped in on the
# same stick): V2 replays the journal at mount, fs.ext4.data passes, the power-off leaves it clean: e2fsck -fn.
set -eu
export LC_ALL=C
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/fs-51-data-reload}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/fs"
LO="${BITSOS_PORT_LO:-24000}"
MCP_FWD=$((LO + 130))
HTTP=$((LO + 140))
TOKEN="fs8c-data-token-5e1d"
NAME=fs-data-rl
IMG="target/xtask/$NAME.img"
BOOT_LINE="BdsDxe: loading"
fail() { echo "RED: $*"; exit 1; }

QPID=""
HPID=""
cleanup() {
    local qemu
    qemu=$(sed -n 's/^xtask: qemu pid \([0-9]*\).*/\1/p' "$RUN_DIR/qemu.out" 2>/dev/null | head -1)
    for p in "$QPID" "$HPID" $qemu; do
        if [ -n "$p" ] && kill -0 "$p" 2>/dev/null; then kill "$p" 2>/dev/null || true; wait "$p" 2>/dev/null || true; fi
    done
}
trap cleanup EXIT

check_volume() {
    local tag="$1" out="$RUN_DIR/$1"
    mkdir -p "$out"
    rm -f "$out/data.img" "$out/vol.img"
    mcopy -n -i "$IMG" ::BITSOS2/data.img "$out/data.img" || fail "$tag: no data.img on the stick"
    BITSVOL_POOL_IMG="$out/data.img" BITSVOL_OUT="$out/vol.img" \
        cargo test -q -p fs-ext4 --lib tests::data::extract_volume -- --ignored --exact > "$out/extract.log" 2>&1 \
        || { cat "$out/extract.log"; fail "$tag: the pool in data.img does not open on the host"; }
}

# The MCP kernel (no ktests) and, for boot 2, a ktest kernel whose KERNEL, INITRD and CONFIG.TXT replace it on the
# same stick (data.img stays).
cargo xtask image --name "$NAME-kt" --ktest --config-extra "ktest=fs.ext4.data;data_img_mb=64;loglevel=info"
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;data_img_mb=64"
kib=$(( $(stat -c %s "$IMG") / 1024 + 700 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
BUNDLES="$RUN_DIR/bundles"
rm -rf "$BUNDLES"
mkdir -p "$BUNDLES/base"
cp "target/xtask/esp-$NAME/BITSOS2/SYS/1/KERNEL" "target/xtask/esp-$NAME/BITSOS2/SYS/1/INITRD" "$BUNDLES/base/"
python3 -I -m http.server --bind 127.0.0.1 --directory "$BUNDLES" "$HTTP" > "$RUN_DIR/http.log" 2>&1 &
HPID=$!

# ---- boot 1: make, probe, Reload x3 with writes in flight, cut ----
LOG="$RUN_DIR/serial-1.log"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --smp 4 --net igb --hostfwd "$MCP_FWD:8765" --timeout 540 --serial-log "$LOG" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
python3 -I "$HERE/data_scenario.py" "$MCP_FWD" "$TOKEN" "$LOG" "$HTTP" "$HERE/data-linux.txt"
qemu=$(sed -n 's/^xtask: qemu pid \([0-9]*\).*/\1/p' "$RUN_DIR/qemu.out" | head -1)
[ -n "$qemu" ] || fail "no QEMU pid from xtask"
kill -9 "$qemu"
wait "$QPID" 2>/dev/null || true
QPID=""
echo "ok: power cut (QEMU $qemu killed) while the writer ran"
grep -a "ext4 data\.\|kexec reload\.synced\|ext4 journal\." "$LOG" | tail -20 || true

check_volume cut
cp "$RUN_DIR/cut/vol.img" "$RUN_DIR/cut/repair.img"
set +e
e2fsck -fy "$RUN_DIR/cut/repair.img" > "$RUN_DIR/cut/repair.log" 2>&1
rc=$?
set -e
cat "$RUN_DIR/cut/repair.log"
[ "$rc" -le 1 ] || fail "after the cut: e2fsck -fy exit $rc"
grep -v "^Pass \|^e2fsck \|^$\|recovering journal\|: [0-9]*/[0-9]* files\|^Clearing orphaned\|^Journal \|^Backing up journal\|^Superblock needs_recovery\|^Run journal anyway\|: \*\*\*\*\* FILE SYSTEM WAS MODIFIED \*\*\*\*\*$" "$RUN_DIR/cut/repair.log" \
    | grep -v "^$(basename "$RUN_DIR/cut/repair.img")" > "$RUN_DIR/cut/problems.txt" || true
if [ -s "$RUN_DIR/cut/problems.txt" ]; then cat "$RUN_DIR/cut/problems.txt"; fail "after the cut: e2fsck found more than a journal to replay"; fi
e2fsck -fn "$RUN_DIR/cut/repair.img" > /dev/null 2>&1 || fail "after the cut: not clean after the host's replay"
echo "ok: after the cut the host's replay needs nothing else (e2fsck -fy exit $rc)"

# ---- boot 2: V2 replays at mount, the power-off leaves it clean ----
for f in KERNEL INITRD; do mcopy -o -i "$IMG" "target/xtask/esp-$NAME-kt/BITSOS2/SYS/1/$f" "::BITSOS2/SYS/1/$f"; done
mcopy -o -i "$IMG" "target/xtask/esp-$NAME-kt/BITSOS2/CONFIG.TXT" ::BITSOS2/CONFIG.TXT
LOG="$RUN_DIR/serial-2.log"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --timeout 400 --serial-log "$LOG" -- -smp 4 > "$RUN_DIR/qemu.out" 2>&1 || true
grep -a "KTEST\|ext4 data\.\|ext4 journal\.\|ext4 mounted" "$LOG" || true
grep -aq "ext4 data.mounted at=/data pool=true writable=true created=false" "$LOG" || fail "boot 2: /data not mounted"
grep -aq "KTEST ok fs.ext4.data " "$LOG" || fail "boot 2: fs.ext4.data"
grep -aq "KTEST done passed=3 failed=0" "$LOG" || fail "boot 2: ktests not green"
grep -aq "ext4 journal.replayed" "$LOG" && echo "ok: boot 2 replayed the journal of the cut" || echo "note: boot 2 found nothing to replay (no commit after the last checkpoint)"
check_volume boot2
e2fsck -fn "$RUN_DIR/boot2/vol.img" > "$RUN_DIR/boot2/fsck.log" 2>&1 || { cat "$RUN_DIR/boot2/fsck.log"; fail "boot 2: e2fsck -fn not clean"; }
echo "GREEN: /data through 3 Reloads with writes in flight and a power cut; e2fsck clean"
