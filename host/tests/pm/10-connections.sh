#!/bin/bash
# slow
# PM-2: the connection view on the fake LAN. Card 1 is QEMU's user network (MCP through hostfwd), card 2 sits on
# `-netdev dgram` to conn_lan.py (router and DNS 192.168.77.1, web hosts .50-.53). conn_client.rs (static musl,
# sent with MCP `run`) resolves three names with its own DNS queries, makes four TCP connections (one to an
# address it never resolved) and holds the last one open. conn_check.py checks names and byte counts: live in
# `connections` while held, then the `pm conn.closed` events and `connections` after the end, against what the
# fake LAN counted. Ports from the PM range (17000-17499).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/pm-10-connections}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/pm"
LAN_PORT=${BITSOS_PORT_LO:-17000}
QEMU_PORT=$((LAN_PORT + 1))
FWD=$((LAN_PORT + 2))
TOKEN="pm2-connections-token-5c1e"
NAME=pm-connections
LOG="$RUN_DIR/serial.log"
LANLOG="$RUN_DIR/conn-lan.log"
BOOT_LINE="BdsDxe: loading"
HOLD_S=12

LPID=""; QPID=""
stop_qemu() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
        local q; q=$(cat "target/xtask/qemu-$NAME.pid" 2>/dev/null || true)
        if [ -n "$q" ] && tr '\0' ' ' < "/proc/$q/cmdline" 2>/dev/null | grep -q -- "-name v2-[A-Za-z0-9]*-$NAME"; then kill "$q" 2>/dev/null || true; fi
        kill "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    QPID=""
}
stop_all() {
    stop_qemu
    if [ -n "$LPID" ]; then kill "$LPID" 2>/dev/null || true; wait "$LPID" 2>/dev/null || true; fi
}
trap stop_all EXIT
red() { echo "RED: $*"; exit 1; }
wait_line() { # $1 = pattern, $2 = file, $3 = seconds
    local n=$(( $3 * 10 ))
    for _ in $(seq 1 "$n"); do grep -aq "$1" "$2" 2>/dev/null && return 0; sleep 0.1; done
    return 1
}

rustc --edition 2021 --target x86_64-unknown-linux-musl -C target-cpu=x86-64-v3 -C opt-level=2 -C strip=symbols \
    -o "$RUN_DIR/conn_client" "$HERE/conn_client.rs"
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
rm -f "$LANLOG"
python3 -I "$HERE/conn_lan.py" "$LAN_PORT" "$QEMU_PORT" "$LANLOG" &
LPID=$!
for attempt in 1 2; do
    rm -f "$LOG"
    cargo xtask qemu --name "$NAME" --net virtio --smp 4 --hostfwd "$FWD:8765" --timeout 400 --serial-log "$LOG" -- \
        -netdev "dgram,id=lan,local.type=inet,local.host=127.0.0.1,local.port=$QEMU_PORT,remote.type=inet,remote.host=127.0.0.1,remote.port=$LAN_PORT" \
        -device virtio-net-pci,netdev=lan,mac=52:54:00:50:4d:02 > "$RUN_DIR/qemu.out" 2>&1 &
    QPID=$!
    if wait_line "$BOOT_LINE" "$LOG" 60; then break; fi
    echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
    stop_qemu
    [ "$attempt" = 2 ] && red "never booted"
done
if ! python3 -I "$HERE/conn_check.py" "127.0.0.1:$FWD" "$TOKEN" "$RUN_DIR/conn_client" "$LANLOG" "$HOLD_S"; then
    echo "== fake LAN"; cat "$LANLOG" || true
    echo "== serial (pm, dhcp)"; grep -a "pm \|dhcp\.\|PANIC" "$LOG" | tail -40 || true
    red "connection view checks"
fi
stop_qemu
grep -a "PANIC\|KTEST FAIL" "$LOG" && red "a panic or failed test in the serial log"
echo "pm connections: green"
