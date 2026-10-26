#!/bin/bash
# slow
# RB-8: the journal (bitsjournal, B6) under QEMU on one stick with a 64 MiB data.img, in three QEMU runs
# (host/tests/rb/journal_scenario.py has the checks of each phase):
#   1. a program's connection found through `events` since/until (from the journal), by time window and field;
#      a second one right before a Reload is there after it (the Reload waited: `klog journal.drained`);
#      metrics from the journal; `reboot` (QEMU ends, -no-reboot);
#   2. the same stick booted again: both still there, three boots told apart;
#   3. booted with the clock 70 days on (-rtc base=...): retention (journal.keep_days=60) deleted the old part.
# Needs init to start /bin/bitsjournal (change request 1 of plan/notes/rb-RB-8.md): without it SKIP, or RED with
# RB8_REQUIRE=1 (the desk's merge gate: a missing CR 1/CR 2 must not pass silently).
# Ports: RB's range (BITSOS_PORT_LO, default 23000): MCP LO+140, HTTP LO+150.
set -eu
export LC_ALL=C
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
for t in python3 mcopy mkfs.fat; do command -v $t >/dev/null || { echo "SKIP: $t missing"; exit 0; }; done
if ! grep -q "bitsjournal" user/init/src/main.rs; then
    if [ "${RB8_REQUIRE:-0}" = 1 ]; then echo "RED: init does not start bitsjournal (CR 1 of plan/notes/rb-RB-8.md missing)"; exit 1; fi
    echo "SKIP: init does not start bitsjournal yet (change request 1 of plan/notes/rb-RB-8.md)"
    exit 0
fi
# MCP's reboot drains the journal once CR 2 is applied: then no wait before it and the drain line is required.
export RB8_REBOOT_DRAIN=0
if grep -q "journal::wait_drained" crates/mcp/src/tools/reboot.rs; then RB8_REBOOT_DRAIN=1; fi
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/rb-70-journal}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/rb"
LO="${BITSOS_PORT_LO:-23000}"
FWD=$((LO + 140))
HTTP=$((LO + 150))
TOKEN="rb8-scenario-token-3d1f"
NAME=rb-journal
IMG="target/xtask/$NAME.img"

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;wlan=off;nvme_write=off;data_img_mb=64" > "$RUN_DIR/image.out" 2>&1 \
    || { tail -20 "$RUN_DIR/image.out"; exit 1; }
kib=$(( $(stat -c %s "$IMG") / 1024 + 700 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
WEB="$RUN_DIR/web"
rm -rf "$WEB"
mkdir -p "$WEB/base"
cp "target/xtask/esp-$NAME/BITSOS2/SYS/1/KERNEL" "target/xtask/esp-$NAME/BITSOS2/SYS/1/INITRD" "$WEB/base/"
for m in marker-a marker-b; do echo "journal marker $m" > "$WEB/$m"; done
python3 -I -m http.server --bind 127.0.0.1 --directory "$WEB" "$HTTP" > "$RUN_DIR/http.log" 2>&1 &
HPID=$!
QPID=""
stop_qemu() {
    local qemu
    qemu=$(sed -n 's/^xtask: qemu pid \([0-9]*\).*/\1/p' "$RUN_DIR/qemu-$1.out" 2>/dev/null | head -1)
    for p in $QPID $qemu; do
        if kill -0 "$p" 2>/dev/null; then kill "$p" 2>/dev/null || true; wait "$p" 2>/dev/null || true; fi
    done
    QPID=""
}
cleanup() {
    for n in 1 2 3; do stop_qemu "$n"; done
    kill "$HPID" 2>/dev/null || true
    wait "$HPID" 2>/dev/null || true
}
trap cleanup EXIT

run() { # phase, extra qemu args...
    local n="$1"
    shift
    local log="$RUN_DIR/serial-$n.log"
    rm -f "$log"
    cargo xtask qemu --name "$NAME" --net igb --hostfwd "$FWD:8765" --timeout 420 --serial-log "$log" "$@" > "$RUN_DIR/qemu-$n.out" 2>&1 &
    QPID=$!
    if ! python3 -I "$HERE/journal_scenario.py" "$n" "$FWD" "$TOKEN" "$HTTP"; then
        grep -a "bitsjournal\|journal\.\|PANIC" "$log" | tail -30
        exit 1
    fi
    if [ "$n" != 3 ]; then
        # reboot: QEMU ends by itself (-no-reboot).
        for _ in $(seq 1 60); do kill -0 "$QPID" 2>/dev/null || break; sleep 1; done
    fi
    stop_qemu "$n"
    if grep -aq "PANIC" "$log"; then echo "RED: the kernel panicked in run $n"; tail -20 "$log"; exit 1; fi
}

run 1
grep -aq "klog journal.drained what=reload" "$RUN_DIR/serial-1.log" || { echo "RED: the Reload did not wait for the journal"; grep -a "journal\." "$RUN_DIR/serial-1.log" | tail; exit 1; }
grep -a "klog journal.drained" "$RUN_DIR/serial-1.log" | head -3
if [ "$RB8_REBOOT_DRAIN" = 1 ]; then
    grep -aq "klog journal.drained what=mcp-reboot" "$RUN_DIR/serial-1.log" || { echo "RED: MCP's reboot did not wait for the journal"; exit 1; }
else
    if [ "${RB8_REQUIRE:-0}" = 1 ]; then echo "RED: MCP's reboot does not drain the journal (CR 2 of plan/notes/rb-RB-8.md missing)"; exit 1; fi
    echo "SKIP: MCP's reboot does not drain the journal yet (CR 2 of plan/notes/rb-RB-8.md); waited for the 5 s flush instead"
fi
run 2
run 3 -- -rtc "base=$(date -d '+70 days' +%Y-%m-%dT%H:%M:%S)"
grep -aq "bitsjournal: retention: deleted" "$RUN_DIR/serial-3.log" || { echo "RED: no retention line in run 3"; exit 1; }
grep -a "bitsjournal:" "$RUN_DIR/serial-3.log" | head -10
echo "journal on V2 (QEMU): green"
