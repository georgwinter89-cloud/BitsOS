#!/bin/bash
# slow
# VS-3 on V2 (QEMU, KVM, igb) through MCP: every restore kind of bitssnap's tool `restore`, each undone again, and
# nothing outside its target changed (host/tests/vs/restore_check.py lists every check): a folder; a program
# (package version + data folder, with a version bitspkg removed since); the whole /data volume (scheduled,
# cancelled, carried out at a reboot, undone at the next); "everything as at a time" (/data + system_pin) with a
# Reload into the generation of then and back. The stick: generations 1 and 2 confirmed (OK), 1 installed two days
# ago and 2 in two days (KERNEL times), so generation 1 is the one that "ran" at the time of a snapshot taken now;
# a 64 MiB data.img made at the first boot. Ports: lane range + 60. Afterwards on the host: fsck.fat -n clean,
# no PIN left, every snapshot of /data (the final one taken at the end included) e2fsck -fn clean.
set -eu
export LC_ALL=C
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/vs-70-restore}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/vs"
LO="${BITSOS_PORT_LO:-18000}"
FWD=$((LO + 60))
TOKEN="vs3-restore-token-7c2e"
NAME=vs-restore
IMG="target/xtask/$NAME.img"
fail() { echo "RED: $*"; exit 1; }
for t in python3 mcopy mdir mkfs.fat fsck.fat objcopy e2fsck; do command -v $t >/dev/null || { echo "SKIP: $t missing"; exit 0; }; done

QPID=""
stop_qemu() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
        local q; q=$(head -1 "target/xtask/qemu-$NAME.pid" 2>/dev/null || true)
        # The QEMU of this run only: its pid file, checked against its -name (v2-<lane>-<name>, any lane).
        if [ -n "$q" ] && tr '\0' ' ' < "/proc/$q/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then
            kill "$q" 2>/dev/null || true
        fi
        kill "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    QPID=""
}
trap stop_qemu EXIT

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;wlan=off;nvme_write=off;data_img_mb=64" | tail -1
ESP="target/xtask/esp-$NAME"
objcopy --strip-debug "$ESP/BITSOS2/SYS/1/KERNEL" "$RUN_DIR/KERNEL"
cp "$RUN_DIR/KERNEL" "$ESP/BITSOS2/SYS/1/KERNEL"
rm -rf "$ESP/BITSOS2/SYS/2"
cp -r "$ESP/BITSOS2/SYS/1" "$ESP/BITSOS2/SYS/2"
touch "$ESP/BITSOS2/SYS/1/OK" "$ESP/BITSOS2/SYS/2/OK"
rm -f "$ESP/BITSOS2/SYS/PIN"
touch -d "-2 days" "$ESP/BITSOS2/SYS/1/KERNEL"
touch -d "+2 days" "$ESP/BITSOS2/SYS/2/KERNEL"
CONTENT=$(du -sb "$ESP" | cut -f1)
# data.img (64 MiB) is made only when 512 MiB of the stick stay free (FS-8c KEEP_FREE).
KIB=$(( (CONTENT + (700 << 20)) / 1024 / 1024 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$KIB" > /dev/null
mcopy -m -s -i "$IMG" "$ESP/EFI" "$ESP/BITSOS2" ::/

LOG="$RUN_DIR/serial.log"
SOCK="$RUN_DIR/qmp.sock"
rm -f "$LOG" "$SOCK"
cargo xtask qemu --name "$NAME" --net igb --mem 4096 --hostfwd "$FWD:8765" --timeout 1200 --serial-log "$LOG" \
    -- -action reboot=reset -qmp "unix:$SOCK,server,nowait" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
for _ in $(seq 100); do [ -S "$SOCK" ] && break; sleep 0.1; done
python3 -I "$HERE/restore_check.py" "$FWD" "$TOKEN" "$LOG" "$SOCK"
stop_qemu
if grep -a "PANIC\|lock.violation" "$LOG"; then fail "a failure line in the log"; fi
grep -a "volume\.rollback_applied\|snap restore_at\|snap rollback_\|generation\.chosen\|kexec reload\.generation_switched" "$LOG" | cut -c1-200 || true
FSCK=$(fsck.fat -n "$IMG" 2>&1) || fail "fsck.fat -n is not clean: $FSCK"
mdir -b -i "$IMG" ::/BITSOS2/SYS/PIN > /dev/null 2>&1 && fail "a PIN is left on the stick"
OUT="$RUN_DIR/host"
rm -rf "$OUT"
mkdir -p "$OUT"
mcopy -n -i "$IMG" ::BITSOS2/data.img "$OUT/data.img" || fail "no data.img on the stick"
cargo build -q -p bitsvol --example export
EXPORT="${CARGO_TARGET_DIR:-target}/debug/examples/export"
# Every snapshot, the before-restore and before-rollback ones and the final one of the check included.
"$EXPORT" "$OUT/data.img" fsck > "$OUT/fsck.log" 2>&1 || { tail -20 "$OUT/fsck.log"; fail "a snapshot of /data is not e2fsck -fn clean"; }
grep -q "before-rollback" "$OUT/fsck.log" || { tail -20 "$OUT/fsck.log"; fail "no before-rollback snapshot checked"; }
echo "ok: on the host: fsck.fat clean, no PIN, every snapshot of /data e2fsck -fn clean ($(tail -1 "$OUT/fsck.log"))"
echo "vs-70-restore: green"
