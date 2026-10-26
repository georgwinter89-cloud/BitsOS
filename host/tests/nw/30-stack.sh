#!/bin/bash
# slow
# NW-3: the smoltcp stack on a QEMU card (virtio-net, then igb) against host partners in the lane's port range:
# a Python HTTP server on 127.0.0.1:24510 (the guest reaches it as 10.0.2.2:24510; a UDP echo on 24511) serving a 16 MiB random file
# by length and chunked plus its SHA-256, and a host client that connects into the guest through
# hostfwd 127.0.0.1:24521 -> guest 24520. All nw.stack ktests must pass (none may skip), the downloads must
# match the hash, and the host must get the guest's answer line. NW-7: nw.stack.fetch_parallel (64 downloads at once,
# each checked against its SHA-256, while a fresh connection keeps being answered).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/nw-30-stack}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/nw"
HTTP_PORT=24510
FWD_HOST=24521
FWD_GUEST=24520
TESTS="nw.stack"
WANT="dhcp ephemeral_random fetch fetch_parallel inbound listen_private loopback_full_pipe loopback_listener_drop loopback_shutdown no_memory ping_gateway quiet_memory tcp_host udp_host"

head -c $((16 * 1024 * 1024)) /dev/urandom > "$RUN_DIR/file.bin"
SPID=""
stop_server() {
    if [ -n "$SPID" ] && kill -0 "$SPID" 2>/dev/null; then kill "$SPID" 2>/dev/null || true; wait "$SPID" 2>/dev/null || true; fi
    SPID=""
}
trap stop_server EXIT
rm -f "$RUN_DIR/http.log"
python3 -I "$HERE/stack_server.py" "$HTTP_PORT" "$RUN_DIR/file.bin" "$FWD_HOST" "$RUN_DIR" &
SPID=$!
for _ in $(seq 1 50); do
    python3 -I -c 'import socket,sys; socket.create_connection(("127.0.0.1", int(sys.argv[1])), 1).close()' "$HTTP_PORT" 2>/dev/null && break
    sleep 0.1
done

check() { # $1 = serial log
    local log="$1" t
    grep -a "KTEST\|ktest\.\|dhcp\.\|http\.get" "$log" || true
    for t in $WANT; do
        grep -aq "KTEST ok nw.stack.$t " "$log" || { echo "RED: nw.stack.$t did not pass"; return 1; }
    done
    if grep -aq "KTEST \(FAIL\|skip\)" "$log"; then echo "RED: a test failed or skipped"; return 1; fi
    grep -q "hello from BitsOS" "$RUN_DIR/inbound.txt" || { echo "RED: host did not get the guest's answer"; cat "$RUN_DIR/inbound.txt"; return 1; }
}

for card in virtio igb; do
    rm -f "$RUN_DIR/inbound.txt"
    cargo xtask image --name "nw-stack-$card" --ktest --config-extra "ktest=$TESTS;loglevel=info;net_test_host=1"
    cargo xtask qemu --name "nw-stack-$card" --net "$card" --hostfwd "$FWD_HOST:$FWD_GUEST" --timeout 240 \
        --serial-log "$RUN_DIR/$card.log" || true
    echo "== $card"
    check "$RUN_DIR/$card.log"
done
echo "nw stack: green (virtio + igb: DHCP, ping, TCP out and in, 16 MiB by length and chunked with the right SHA-256)"
