#!/bin/bash
# slow
# NW-10: downloads from node.js (FD-1: 3-5 MB/s on the Mini-PC, python 116 MB/s) and from python's http.server
# with the host kernel's TCP as the guest's partner. QEMU's user network would put slirp's TCP in between, so the
# guest's second card is cabled (-netdev dgram + tap_cable.py) to a tap in a private user + network namespace
# (unshare -rn): the tap, its address 10.77.0.1, the cable's DHCP answer and both servers exist only there; the host's
# network is never touched. MCP comes in over the first card (user network, hostfwd inside the namespace).
# Over MCP: fetch and BusyBox wget from both servers, length and chunked, every download checked against the
# file's FNV-1a 64. The tap side is captured (filter-dump, headers only) and summarised by pcap_tcp.py: pauses of
# the sender, ACK rate, the guest's window. Rates are printed, never judged (QEMU); the Mini-PC check compares them.
# Without node or user namespaces on this host the scenario says so and passes.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/nw-70-node-tap}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/nw"
NODE_PORT=24530
PY_PORT=24531       # stack_server.py (UDP echo +1, TCP echo +2)
CABLE_PORT=24581    # tap_cable.py's end of the dgram cable
QEMU_PORT=24582     # QEMU's end
FWD=24580           # MCP, inside the namespace
TOKEN="nw10-tap-token-5c1e"
NAME="nw-node-tap"
TAP_IP=10.77.0.1
GUEST_IP=10.77.0.21
MIB="${NW10_MIB:-100}"
# The tap side's card model and the wire towards the guest (tc netem on the tap: delay, rate, a switch's queue).
# Default: igb (the Mini-PC's driver family) behind 0.1 ms each way at 1 Gbit/s, roughly the home LAN.
CARD="${NW10_CARD:-igb}"
NETEM="${NW10_NETEM:-delay 100us rate 1gbit limit 2000}"

if [ "${1:-}" != "--inside" ]; then
    for t in node ip tc unshare; do
        command -v "$t" > /dev/null || { echo "nw node-tap: skipped ($t is not installed)"; exit 0; }
    done
    unshare -rn true 2> /dev/null || { echo "nw node-tap: skipped (no user namespaces here)"; exit 0; }
    [ -n "${NW10_NO_IMAGE:-}" ] || cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
    head -c $((MIB * 1024 * 1024)) /dev/urandom > "$RUN_DIR/file.bin"
    exec unshare -rn "$0" --inside
fi

PIDS=()
QPID=""
stop_all() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2> /dev/null; then
        q=$(cat "target/xtask/qemu-$NAME.pid" 2> /dev/null || true)
        if [ -n "$q" ] && tr '\0' ' ' < "/proc/$q/cmdline" 2> /dev/null | grep -q -- "-name v2-[A-Za-z0-9]*-$NAME"; then kill "$q" 2> /dev/null || true; fi
        kill "$QPID" 2> /dev/null || true
        wait "$QPID" 2> /dev/null || true
    fi
    for p in "${PIDS[@]}"; do kill "$p" 2> /dev/null || true; wait "$p" 2> /dev/null || true; done
}
trap stop_all EXIT
red() { echo "RED: $*"; exit 1; }

ip link set lo up
python3 -I "$HERE/tap_cable.py" tap0 "$CABLE_PORT" "$QEMU_PORT" "$TAP_IP" "$GUEST_IP" > "$RUN_DIR/cable.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 50); do ip link show tap0 > /dev/null 2>&1 && break; sleep 0.1; done
ip link show tap0 > /dev/null 2>&1 || red "no tap0 (see $RUN_DIR/cable.log)"
ip addr add "$TAP_IP/24" dev tap0
ip link set tap0 up
[ "$NETEM" = none ] || tc qdisc add dev tap0 root netem $NETEM
rm -f "$RUN_DIR/http.log" "$RUN_DIR/lan.pcap"
node "$HERE/node_server.mjs" "$NODE_PORT" "$RUN_DIR/file.bin" "$TAP_IP" > "$RUN_DIR/node.log" 2>&1 &
PIDS+=($!)
python3 -I "$HERE/stack_server.py" "$PY_PORT" "$RUN_DIR/file.bin" 24579 "$RUN_DIR" "$TAP_IP" &
PIDS+=($!)
for port in $NODE_PORT $PY_PORT; do
    for _ in $(seq 1 100); do
        python3 -I -c 'import socket,sys; socket.create_connection((sys.argv[1], int(sys.argv[2])), 1).close()' "$TAP_IP" "$port" 2> /dev/null && break
        sleep 0.1
    done
done

cargo xtask qemu --name "$NAME" --net virtio --smp 4 --mem 4G --hostfwd "$FWD:8765" --timeout 900 --serial-log "$RUN_DIR/serial.log" -- \
    -netdev "dgram,id=lan,local.type=inet,local.host=127.0.0.1,local.port=$QEMU_PORT,remote.type=inet,remote.host=127.0.0.1,remote.port=$CABLE_PORT" \
    -device "$CARD,netdev=lan,mac=52:54:00:4e:57:3a" \
    -object "filter-dump,id=cap,netdev=lan,file=$RUN_DIR/lan.pcap,maxlen=128" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
python3 -I "$HERE/node_tap.py" "127.0.0.1:$FWD" "$TOKEN" "$RUN_DIR/file.bin" "http://$TAP_IP:$NODE_PORT" "http://$TAP_IP:$PY_PORT" \
    | tee "$RUN_DIR/drive.log"
[ "${PIPESTATUS[0]}" = 0 ] || red "downloads (see $RUN_DIR/drive.log, $RUN_DIR/serial.log)"
# The desk's LAN tool (lan_rate.py, for the Mini-PC check) once here, small: every line answered, none FAILED.
echo "$TOKEN" > "$RUN_DIR/token"
python3 -I "$HERE/lan_rate.py" --mcp "127.0.0.1:$FWD" --token-file "$RUN_DIR/token" --bind "$TAP_IP" --mib 16 \
    --node-port 24534 --py-port 24535 --dir "$RUN_DIR/lan-rate" | tee "$RUN_DIR/lan-rate.log"
[ "${PIPESTATUS[0]}" = 0 ] || red "lan_rate.py failed"
[ "$(grep -c ' sender: ' "$RUN_DIR/lan-rate.log")" = 7 ] || red "lan_rate.py: not seven downloads"
grep -q "FAILED" "$RUN_DIR/lan-rate.log" && red "lan_rate.py: a download failed"
stop_all
trap - EXIT
echo "== capture (tap side, headers only)"
for port in $NODE_PORT $PY_PORT; do python3 -I "$HERE/pcap_tcp.py" "$RUN_DIR/lan.pcap" "$port"; done | tee "$RUN_DIR/capture.txt"
# The downloads must have crossed the tap (not the user network): bytes from both servers in the capture.
for port in $NODE_PORT $PY_PORT; do
    grep -q "== server :$port -> guest" "$RUN_DIR/capture.txt" || red "no download from :$port crossed the tap"
done
echo "nw node-tap: green (fetch and BusyBox wget from node and python over the host's own TCP, every download intact)"
