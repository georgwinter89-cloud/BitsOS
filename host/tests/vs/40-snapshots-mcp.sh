#!/bin/bash
# slow
# VS-2 on V2 (QEMU) through MCP: bitssnap's tools snapshots, snapshot_take, snapshot_delete and restore (host/tests/vs/snap_mcp_check.py),
# on a stick of this run with a 64 MiB data.img. Ports: lane range + 30.
set -eu
export LC_ALL=C
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
for t in python3 mcopy mkfs.fat; do command -v $t >/dev/null || { echo "SKIP: $t missing"; exit 0; }; done
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/vs-40-snapshots-mcp}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-18000}"
FWD=$((LO + 30))
TOKEN="vs2-scenario-token-5e1a"
LOG="$RUN_DIR/serial.log"
NAME=vs-snap-mcp
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
python3 -I host/tests/vs/snap_mcp_check.py "$FWD" "$TOKEN"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
grep -aq "snap taken pool=data " "$LOG" || { echo "RED: no snap taken event in the kernel log"; exit 1; }
grep -a "bitssnap:" "$LOG" | head -20 || true
echo "bitssnap MCP tools on V2 (QEMU): green"
