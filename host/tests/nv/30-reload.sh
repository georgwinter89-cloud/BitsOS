#!/bin/bash
# slow
# NV-1: RB's Reload scenario (host/tests/rb/reload_scenario.py: ten reloads in a row over MCP, 16 CPUs, igb,
# qemu-xhci) with an active NVMe controller: NVQ0 with the fake Windows GPT image (ns1) and a 4 KiB pattern
# image (ns2). Each kernel brings the controller up and registers its disks; each Reload quiesces it first
# (shutdown, CC.EN=0, RDY=0, bus master off). CONFIG nvme_busy=on keeps a reader on ns1 going without pause in
# every kernel, so each Reload quiesces the controller with a read in flight. Afterwards: every boot logged `nvme ctrl.ready`, no
# `nvme ctrl.failed`, and both images are byte for byte what they were.
# Ports: NV's range (BITSOS_PORT_LO, default 19000).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/nv-30-reload}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
NV="$(pwd)/host/tests/nv"
RB="$(pwd)/host/tests/rb"
LO="${BITSOS_PORT_LO:-19000}"
MCP_FWD=$((LO + 100))
HTTP=$((LO + 110))
TOKEN="nv1-reload-token-5e1d"
LOG="$RUN_DIR/serial.log"
NAME=nv-reload
SMP="${NV_SMP:-16}"
sum() { sha256sum "$1" | cut -d' ' -f1; }
fail() { echo "RED: $*"; exit 1; }

if ! grep -q "kexec::set_boot_info" kernel/src/main.rs || ! grep -q "net::init" kernel/src/main.rs; then
    echo "SKIP: kernel/src/main.rs lacks kexec::set_boot_info or net::init"
    exit 0
fi
python3 -I "$NV/mkimg.py" gpt "$RUN_DIR/win.img" 65536 512 3 efi:2048:4095 msr:4096:8191 msdata:8192:65000
python3 -I "$NV/mkimg.py" pattern "$RUN_DIR/k4.img" $((8 << 20)) 2
WIN=$(sum "$RUN_DIR/win.img")
K4=$(sum "$RUN_DIR/k4.img")

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;rb_squeeze_mb=192;rb_choke_mb=10;nvme_busy=on"
BUNDLES="$RUN_DIR/bundles"
rm -rf "$BUNDLES"
mkdir -p "$BUNDLES/base"
cp "target/xtask/esp-$NAME/BITSOS2/SYS/1/KERNEL" "target/xtask/esp-$NAME/BITSOS2/SYS/1/INITRD" "$BUNDLES/base/"

python3 -I -m http.server --bind 127.0.0.1 --directory "$BUNDLES" "$HTTP" > "$RUN_DIR/http.log" 2>&1 &
HPID=$!
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --smp "$SMP" --net igb --hostfwd "$MCP_FWD:8765" --timeout 540 --serial-log "$LOG" -- \
    -drive "if=none,id=n1,format=raw,file=$RUN_DIR/win.img" \
    -drive "if=none,id=n2,format=raw,file=$RUN_DIR/k4.img" \
    -device nvme,id=nvq0,serial=NVQ0 \
    -device nvme-ns,drive=n1,bus=nvq0,nsid=1 \
    -device nvme-ns,drive=n2,bus=nvq0,nsid=2,logical_block_size=4096,physical_block_size=4096 \
    > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
cleanup() {
    local qemu
    qemu=$(sed -n 's/^xtask: qemu pid \([0-9]*\).*/\1/p' "$RUN_DIR/qemu.out" 2>/dev/null | head -1)
    for p in "$QPID" "$HPID" $qemu; do
        if kill -0 "$p" 2>/dev/null; then kill "$p" 2>/dev/null || true; wait "$p" 2>/dev/null || true; fi
    done
}
trap cleanup EXIT

python3 -I "$RB/reload_scenario.py" "$MCP_FWD" "$TOKEN" "$LOG" "$BUNDLES" "$HTTP" "$SMP"
grep -a "nvme ctrl\.\(ready\|failed\|recover\|start_aborted\)\|nvme busy\.\|nvme cmd\.\|kexec \(handover\|reload\.jump\)" "$LOG" | tail -40
READY=$(grep -ac "nvme ctrl.ready ctrl=nvme0" "$LOG" || true)
[ "$READY" -ge 11 ] || fail "nvme came up $READY times, want 11 (boot + 10 reloads)"
BUSY=$(grep -ac "nvme busy.started ctrl=nvme0" "$LOG" || true)
[ "$BUSY" -ge 11 ] || fail "the reader ran in $BUSY kernels, want 11"
if grep -aq "nvme ctrl.failed\|nvme ctrl.recover\|nvme busy.failed\|PANIC" "$LOG"; then fail "a failure line in the log"; fi
[ "$(sum "$RUN_DIR/win.img")" = "$WIN" ] || fail "win.img changed"
[ "$(sum "$RUN_DIR/k4.img")" = "$K4" ] || fail "k4.img changed"
echo "GREEN: $READY bring-ups over Reloads, a reader running at each jump ($BUSY), images unchanged"
