#!/bin/bash
# slow
# EINS-C (K5, K6): the assistant tree's trash end to end on /data (ext4 in the stick's data.img, data_img_mb=64).
# Through MCP `run` (uid 1000) busybox's shell enters a tree itself (`echo enter > /proc/self/bitsos_tree`) and runs
# `rm -rf` on a folder of files (a symlink, an empty folder, 300 KiB of random bytes): everything is in one entry
# of /data/.papierkorb and comes back byte for byte through /dev/bitsos/papierkorb; the tree may not empty the
# trash nor remove from it; on tmpfs it removes for real; the same `rm -rf` outside the tree deletes for real and
# leaves the trash as it was; Georg's `empty` removes the entry. Then the ktest fs.trash.scenario (over MCP).
# papierkorb_check.py does the steps. Ports: lane range + 60 (MCP forward).
set -eu
export LC_ALL=C
for t in python3 mcopy mkfs.fat; do command -v $t >/dev/null || { echo "RED: $t missing"; exit 1; }; done
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/fs-60-papierkorb}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-24000}"
FWD=$((LO + 60))
TOKEN="eins-c-scenario-token-61d2"
LOG="$RUN_DIR/serial.log"
NAME=fs-papierkorb
export BITSOS_KTEST_RUN="${BITSOS_KTEST_RUN:-fs/60-papierkorb}"

QPID=""
stop() {
    [ -n "$QPID" ] && kill "$QPID" 2>/dev/null || true
    pf="target/xtask/qemu-$NAME.pid"
    if [ -f "$pf" ]; then
        pid="$(head -1 "$pf")"
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then kill "$pid" 2>/dev/null || true; fi
    fi
    [ -n "$QPID" ] && wait "$QPID" 2>/dev/null || true
}
trap stop EXIT

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;nvme_write=off;data_img_mb=64"
IMG="target/xtask/$NAME.img"
[ -f "$IMG" ] || { echo "RED: no image $IMG"; exit 1; }
# Room for data.img (64 MiB) and the 512 MiB V2 keeps free: the FAT image made anew from xtask's ESP tree, larger.
kib=$(( $(stat -c %s "$IMG") / 1024 + 700 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 900 --smp 4 --mem 4G --serial-log "$LOG" \
    > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
python3 -I host/tests/fs/papierkorb_check.py "$FWD" "$TOKEN" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "KTEST ok fs.trash.scenario (checked by this script)" >> "$RUN_DIR/ledger.txt"
echo "GREEN: the tree's rm -rf went to the trash and came back whole; outside the tree it deleted for real"
