#!/bin/bash
# slow
# VS-6 on V2 (QEMU): the snapshot space warning. A 64 MiB pool is filled past 80 % with manual snapshots of a changing
# file; MCP `snapshots` warns and proposes the oldest for deletion, nothing is deleted; below 80 % no warning
# (host/tests/vs/snap_space_check.py). Ports: lane range + 31.
set -eu
export LC_ALL=C
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
for t in python3 mcopy mkfs.fat; do command -v $t >/dev/null || { echo "SKIP: $t missing"; exit 0; }; done
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/vs-60-snapshots-space}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-18000}"
FWD=$((LO + 31))
TOKEN="vs6-scenario-token-81c4"
LOG="$RUN_DIR/serial.log"
NAME=vs-snap-space
IMG="target/xtask/$NAME.img"

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;wlan=off;nvme_write=off;data_img_mb=64"
kib=$(( $(stat -c %s "$IMG") / 1024 + 700 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 600 --serial-log "$LOG" \
    > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
stop() {
    kill "$QPID" 2>/dev/null || true
    pf="target/xtask/qemu-$NAME.pid"
    if [ -f "$pf" ]; then
        pid="$(head -1 "$pf")"
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then kill "$pid" 2>/dev/null || true; fi
    fi
    wait "$QPID" 2>/dev/null || true
}
trap stop EXIT
python3 -I host/tests/vs/snap_space_check.py "$FWD" "$TOKEN"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
n=$(grep -ac " W user kmsg text=\"bitssnap: the data pool is [0-9.]* % full (warning above 80 %)" "$LOG" || true)
[ "$n" = 1 ] || { echo "RED: want one space warning at level Warn in the kernel log, found $n"; grep -a "bitssnap:" "$LOG" | tail -20; exit 1; }
grep -aq "below 75 % full again" "$LOG" || echo "note: the pool did not drop below 75 % again after the clean-up (see snap_space_check.py)"
grep -a "bitssnap:" "$LOG" | head -20 || true
echo "bitssnap space warning on V2 (QEMU): green"
