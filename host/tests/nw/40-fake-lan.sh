#!/bin/bash
# slow
# NW-3: two cards and a scripted DHCP life (the idea of V1's net-lan.mjs). Card 1 is QEMU's user network
# (leases at once: it owns the default route), card 2 sits on `-netdev dgram` to fake_lan.py, the router
# 192.168.77.1: three DISCOVERs unanswered, a 30 s lease with T1 3 s / T2 8 s, a renew ACKed, a renew ignored
# (rebind by broadcast at T2, ACKed), a renew NAKed, a new address .21. Then QMP pulls card 2's cable and plugs
# it back: the address stays, DHCP asks again and gets the same address. ktests nw.lan.dhcp and nw.lan.link check
# the guest side; this script checks the events and the router's log (repeated DISCOVER, renew, rebind, NAK,
# pings answered both ways).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/nw-40-fake-lan}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/nw"
LAN_PORT=24530   # fake_lan.py listens here
QEMU_PORT=24531  # QEMU's end of the dgram cable
NAME=nw-fake-lan
LOG="$RUN_DIR/serial.log"
LANLOG="$RUN_DIR/fake-lan.log"
BOOT_LINE="BdsDxe: loading"

FPID=""; QPID=""
stop_own() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
        local q; q=$(cat "target/xtask/qemu-$NAME.pid" 2>/dev/null || true)
        if [ -n "$q" ]; then kill "$q" 2>/dev/null || true; fi
        kill "$QPID" 2>/dev/null || true; wait "$QPID" 2>/dev/null || true
    fi
    if [ -n "$FPID" ] && kill -0 "$FPID" 2>/dev/null; then kill "$FPID" 2>/dev/null || true; wait "$FPID" 2>/dev/null || true; fi
    QPID=""; FPID=""
}
trap stop_own EXIT
qmp() { # $1 = up (true|false)
    python3 -I -c '
import json, socket, sys
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); f = s.makefile("rw")
f.readline()
for c in ({"execute": "qmp_capabilities"}, {"execute": "set_link", "arguments": {"name": "lan", "up": sys.argv[2] == "true"}}):
    f.write(json.dumps(c) + "\n"); f.flush(); r = json.loads(f.readline())
    if "error" in r: sys.exit(r)
' "$RUN_DIR/qmp.sock" "$1"
}
wait_line() { # $1 = pattern, $2 = file, $3 = seconds
    local n=$(( $3 * 10 ))
    for _ in $(seq 1 "$n"); do grep -aq "$1" "$2" 2>/dev/null && return 0; sleep 0.1; done
    return 1
}

cargo xtask image --name "$NAME" --ktest --config-extra "ktest=nw.lan;loglevel=info"
for attempt in 1 2; do
    rm -f "$RUN_DIR/qmp.sock" "$LOG" "$LANLOG"
    python3 -I "$HERE/fake_lan.py" "$LAN_PORT" "$QEMU_PORT" "$LANLOG" &
    FPID=$!
    cargo xtask qemu --name "$NAME" --net virtio --timeout 240 --serial-log "$LOG" -- \
        -netdev "dgram,id=lan,local.type=inet,local.host=127.0.0.1,local.port=$QEMU_PORT,remote.type=inet,remote.host=127.0.0.1,remote.port=$LAN_PORT" \
        -device virtio-net-pci,netdev=lan,mac=52:54:00:4e:57:30 -qmp "unix:$RUN_DIR/qmp.sock,server=on,wait=off" &
    QPID=$!
    if wait_line "$BOOT_LINE" "$LOG" 60; then break; fi
    echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
    stop_own
    [ "$attempt" = 2 ] && { echo "RED: never booted"; exit 1; }
done
wait_line "ktest.lan_link_wait" "$LOG" 150 || { echo "RED: no ktest.lan_link_wait"; grep -a "KTEST\|net " "$LOG" || true; exit 1; }
qmp false
wait_line "ktest.lan_link_down_seen" "$LOG" 60 || { echo "RED: link down not seen"; exit 1; }
sleep 2
qmp true
wait "$QPID" || true
QPID=""
grep -a "KTEST\|ktest\.\|dhcp\.\|route\.\|iface\." "$LOG" || true
echo "== fake LAN"
cat "$LANLOG"

red() { echo "RED: $*"; exit 1; }
for t in dhcp link; do grep -aq "KTEST ok nw.lan.$t " "$LOG" || red "nw.lan.$t did not pass"; done
grep -aq "dhcp.retry ifname=virtio-net1 attempt=4" "$LOG" || red "no 4th DISCOVER (repeated DISCOVER)"
grep -aq "dhcp.lease ifname=virtio-net1 ip=192.168.77.20 .*lease_s=30" "$LOG" || red "no first lease .20"
grep -aq "route.default ifname=virtio-net0" "$LOG" || red "default route not on the first leased card"
[ "$(grep -ac "dhcp.renewed ifname=virtio-net1 ip=192.168.77.20" "$LOG")" -ge 2 ] || red "renew and rebind not both seen"
grep -aq "dhcp.lost ifname=virtio-net1" "$LOG" || red "NAK not seen as a loss"
grep -aq "dhcp.lease ifname=virtio-net1 ip=192.168.77.21" "$LOG" || red "no new lease .21"
grep -aq "iface.link_down ifname=virtio-net1" "$LOG" || red "no link_down event"
grep -aq "dhcp.restart ifname=virtio-net1 keep_ip=192.168.77.21" "$LOG" || red "no DHCP restart keeping .21"
grep -q "DISCOVER 3 dropped" "$LANLOG" || red "router log: drops"
grep -q "renew ack" "$LANLOG" || red "router log: renew"
grep -q "rebind ack" "$LANLOG" || red "router log: rebind"
grep -q "NAK" "$LANLOG" || red "router log: NAK"
grep -q "echo request from 192.168.77.2" "$LANLOG" || red "router log: the guest's pings"
grep -q "guest answered ping" "$LANLOG" || red "router log: the guest answers pings"
echo "nw fake LAN: green (repeated DISCOVER, lease, renew, rebind, NAK, new lease, two cards, link flap)"
