#!/bin/bash
# NW-2: the igc driver on QEMU's igb (82576, the I226-V's stand-in). Three runs:
# 1. one igb on QEMU user networking: all nw.igc ktests (probe, ARP, DHCP, poll_tx, burst, robust, quiesce,
#    firmware left the card running, the PHY semaphore on QEMU's registers, the whole bring-up as an I226,
#    a restart run by the igc.restart worker thread, BARs lost and written back by restore_config)
#    plus the NetDevice/Quiesce contract tests;
# 2. cable pull: QMP set_link off/on while nw.igc.link_qmp waits (link.down, no frames, link.up, ARP again);
# 3. two cards as on the Mini-PC, the first without link (QMP set_link off before the kernel runs), the
#    second behind a PCIe root port: both run, the second carries traffic, the first gets its link later.
# Fails if a run is red, if a test skipped because its card was missing, or if a scenario never reached QMP.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/nw-20-igb}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
BOOT_LINE="BdsDxe: loading"

# 1. all nw.igc ktests on one igb
cargo xtask image --name nw-igb-1 --ktest --config-extra "ktest=nw.igc,contract.netdevice,contract.quiesce;loglevel=info"
cargo xtask qemu --name nw-igb-1 --net igb --timeout 180 --serial-log "$RUN_DIR/one.log" || true
grep -a "KTEST\|igc\.\|link\." "$RUN_DIR/one.log" || true
grep -aq "KTEST done" "$RUN_DIR/one.log" || { echo "RED: run 1 did not finish"; exit 1; }
grep -aq "KTEST FAIL" "$RUN_DIR/one.log" && { echo "RED: run 1 has failures"; exit 1; }
ok=$(grep -ac "KTEST ok nw.igc." "$RUN_DIR/one.log" || true)
[ "$ok" -ge 13 ] || { echo "RED: only $ok nw.igc tests passed (want 13)"; exit 1; }
grep -aq "KTEST ok nw.igc.bars_lost" "$RUN_DIR/one.log" || { echo "RED: nw.igc.bars_lost (NW-3b restore_config) did not pass"; exit 1; }
grep -aq "KTEST skip nw.igc.*no igb" "$RUN_DIR/one.log" && { echo "RED: a test found no igb card"; exit 1; }
grep -aq "igc.up ifname=igc0 model=82576" "$RUN_DIR/one.log" || { echo "RED: no igc.up"; exit 1; }
grep -aq "ktest.dhcp_offer" "$RUN_DIR/one.log" || { echo "RED: no DHCP offer"; exit 1; }

# QMP helpers (runs 2 and 3)
qmp() { # $1 = netdev id, $2 = up (true|false)
    python3 -I -c '
import json, socket, sys
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); f = s.makefile("rw")
f.readline()
for c in ({"execute": "qmp_capabilities"}, {"execute": "set_link", "arguments": {"name": sys.argv[2], "up": sys.argv[3] == "true"}}):
    f.write(json.dumps(c) + "\n"); f.flush(); r = json.loads(f.readline())
    if "error" in r: sys.exit(r)
' "$RUN_DIR/qmp.sock" "$1" "$2"
}
wait_line() { # $1 = pattern, $2 = log, $3 = seconds
    local n=$(( $3 * 10 ))
    for _ in $(seq 1 "$n"); do grep -aq "$1" "$2" 2>/dev/null && return 0; sleep 0.1; done
    return 1
}
wait_sock() {
    for _ in $(seq 1 100); do [ -S "$RUN_DIR/qmp.sock" ] && return 0; sleep 0.1; done
    return 1
}
QPID=""
QNAME=""
# Only our own processes: the QEMU this xtask started (its pid file) and the xtask itself.
stop_own() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
        local q; q=$(cat "target/xtask/qemu-$QNAME.pid" 2>/dev/null || true)
        if [ -n "$q" ]; then kill "$q" 2>/dev/null || true; fi
        kill "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    QPID=""
}
trap stop_own EXIT

start_qmp_run() { # $1 = name, $2 = log, $3 = netdev to switch off before boot (or ""), rest = extra QEMU args
    local name="$1" log="$2" off="$3"; shift 3
    QNAME="$name"
    for attempt in 1 2; do
        rm -f "$RUN_DIR/qmp.sock" "$log"
        cargo xtask qemu --name "$name" --net none --timeout 180 --serial-log "$log" -- \
            "$@" -qmp "unix:$RUN_DIR/qmp.sock,server=on,wait=off" &
        QPID=$!
        wait_sock || { echo "RED: no QMP socket"; exit 1; }
        if [ -n "$off" ]; then qmp "$off" false; fi
        if wait_line "$BOOT_LINE" "$log" 60; then return 0; fi
        echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
        stop_own
    done
    echo "RED: $name never booted"; exit 1
}

# 2. cable pull
cargo xtask image --name nw-igb-l --ktest --config-extra "ktest=nw.igc.link_qmp;loglevel=info"
start_qmp_run nw-igb-l "$RUN_DIR/link.log" "" -netdev user,id=n1 -device igb,netdev=n1,mac=52:54:00:4e:57:02
wait_line "ktest.link_wait" "$RUN_DIR/link.log" 90 || { echo "RED: no ktest.link_wait"; exit 1; }
qmp n1 false
wait_line "ktest.link_down_seen" "$RUN_DIR/link.log" 60 || { echo "RED: link down not seen"; exit 1; }
qmp n1 true
wait "$QPID" || true
QPID=""
grep -a "KTEST\|link\." "$RUN_DIR/link.log"
grep -aq "KTEST ok nw.igc.link_qmp" "$RUN_DIR/link.log" || { echo "RED: nw.igc.link_qmp"; exit 1; }

# 3. two cards, the first without link, the second behind a PCIe root port
cargo xtask image --name nw-igb-2 --ktest --config-extra "ktest=nw.igc.two_cards;loglevel=info"
start_qmp_run nw-igb-2 "$RUN_DIR/two.log" n1 \
    -netdev user,id=n1 -device igb,netdev=n1,mac=52:54:00:4e:57:10 \
    -device pcie-root-port,id=rp1,chassis=1 -netdev user,id=n2 -device igb,netdev=n2,bus=rp1,mac=52:54:00:4e:57:11
wait_line "ktest.two_wait" "$RUN_DIR/two.log" 90 || { echo "RED: no ktest.two_wait"; exit 1; }
qmp n1 true
wait "$QPID" || true
QPID=""
grep -a "KTEST\|igc\.\|link\." "$RUN_DIR/two.log"
grep -aq "KTEST ok nw.igc.two_cards" "$RUN_DIR/two.log" || { echo "RED: nw.igc.two_cards"; exit 1; }
[ "$(grep -ac "igc.up ifname=igc" "$RUN_DIR/two.log")" -ge 2 ] || { echo "RED: not both cards came up"; exit 1; }
echo "nw igb: green (all ktests + cable pull + two cards)"
