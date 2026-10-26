#!/bin/bash
# slow
# NW-5: program sockets on the kernel stack. Boot 1 (virtio-net on QEMU's user network + a second virtio card on
# the fake LAN k3_lan.py): every nw.sock ktest, none may skip: the socket programs against Linux's transcripts,
# the Wayland-like pair, an HTTP fetch (length and chunked, SHA-256), a UDP echo and 3 x 4 MiB through a TCP echo
# against stack_server.py on the host (10.0.2.2), and V1's k3a/k3b fakes (refused, silent, hold-until-resend,
# late ARP, lossy web server). Boot 2 (igb): the same against the host server (the fake LAN needs virtio).
# Boot 3 (normal boot, MCP): k3b in QEMU: 3 connections stream 64 MiB each through the echo while MCP is asked
# every 200 ms; every MCP call must be answered (timing is judged on the Mini-PC only). NW-7, same boot on igb:
# MCP's `ktest` runs nw.stack.fetch_parallel (64 downloads at once against stack_server.py, every SHA-256 checked)
# while MCP is asked every 200 ms.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/nw-60-sockets}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/nw"
HTTP_PORT=24560     # stack_server.py (UDP echo +1, TCP echo +2), the guest's 10.0.2.2
LAN_PORT=24570      # k3_lan.py
QEMU_PORT=24571     # QEMU's end of the dgram cable
FWD=24572           # host -> guest MCP
TOKEN="nw5-sockets-token-7a2d"
BOOT_LINE="BdsDxe: loading"

SPID=""; LPID=""; QPID=""; NAME=""
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
    for p in $SPID $LPID; do kill "$p" 2>/dev/null || true; wait "$p" 2>/dev/null || true; done
}
trap stop_all EXIT
red() { echo "RED: $*"; exit 1; }
wait_line() { # $1 = pattern, $2 = file, $3 = seconds
    local n=$(( $3 * 10 ))
    for _ in $(seq 1 "$n"); do grep -aq "$1" "$2" 2>/dev/null && return 0; sleep 0.1; done
    return 1
}

head -c $((16 * 1024 * 1024)) /dev/urandom > "$RUN_DIR/file.bin"
rm -f "$RUN_DIR/http.log" "$RUN_DIR/lan.log"
python3 -I "$HERE/stack_server.py" "$HTTP_PORT" "$RUN_DIR/file.bin" 24579 "$RUN_DIR" &
SPID=$!
python3 -I "$HERE/k3_lan.py" "$LAN_PORT" "$QEMU_PORT" "$RUN_DIR/lan.log" &
LPID=$!
for _ in $(seq 1 50); do
    python3 -I -c 'import socket,sys; socket.create_connection(("127.0.0.1", int(sys.argv[1])), 1).close()' "$HTTP_PORT" 2>/dev/null && break
    sleep 0.1
done

ktest_boot() { # $1 = card, $2 = with the fake LAN (yes|no)
    local card="$1" lan="$2" log="$RUN_DIR/ktest-$1.log" extra=()
    NAME="nw-sock-$card"
    local cfg="ktest=nw.sock;loglevel=info;net_test_url=http://10.0.2.2:$HTTP_PORT"
    if [ "$lan" = yes ]; then
        cfg="$cfg;nw_k3_peer=192.168.77.1"
        extra=(-netdev "dgram,id=lan,local.type=inet,local.host=127.0.0.1,local.port=$QEMU_PORT,remote.type=inet,remote.host=127.0.0.1,remote.port=$LAN_PORT"
               -device "virtio-net-pci,netdev=lan,mac=52:54:00:4e:57:35")
    fi
    cargo xtask image --name "$NAME" --ktest --config-extra "$cfg"
    for attempt in 1 2; do
        rm -f "$log"
        cargo xtask qemu --name "$NAME" --net "$card" --smp 4 --timeout 400 --serial-log "$log" -- "${extra[@]}" > "$RUN_DIR/$NAME.qemu.out" 2>&1 &
        QPID=$!
        if wait_line "$BOOT_LINE" "$log" 60; then break; fi
        echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
        stop_qemu
        [ "$attempt" = 2 ] && red "$card: never booted"
    done
    wait "$QPID" || true
    QPID=""
    echo "== $card"
    grep -a "KTEST\|sock\.\|unix.gc" "$log" || true
    local want="abi_fdpass abi_inet abi_unix fetch wayland_pair"
    [ "$lan" = yes ] && want="$want k3_lan"
    for t in $want; do grep -aq "KTEST ok nw.sock.$t " "$log" || red "$card: nw.sock.$t did not pass"; done
    if grep -a "KTEST \(FAIL\|PANIC\)" "$log"; then red "$card: a test failed"; fi
    if [ "$lan" = no ]; then
        grep -a "KTEST skip" "$log" | grep -v "nw.sock.k3_lan" && red "$card: a test skipped"
    elif grep -aq "KTEST skip" "$log"; then
        red "$card: a test skipped"
    fi
    true
}

ktest_boot virtio yes
echo "== fake LAN"
cat "$RUN_DIR/lan.log"
grep -q "hold: first data segment" "$RUN_DIR/lan.log" || red "fake LAN: the hold peer never ignored a segment"
grep -q "hold: the resend came" "$RUN_DIR/lan.log" || red "fake LAN: no resend to the hold peer"
grep -q "silent: SYN dropped" "$RUN_DIR/lan.log" || red "fake LAN: the silent peer saw no SYN"
grep -q "reset: SYN to 7003" "$RUN_DIR/lan.log" || red "fake LAN: the reset peer saw no SYN"
grep -q "arp: 192.168.77.50 asked" "$RUN_DIR/lan.log" || red "fake LAN: no ARP for .50"
grep -q "lossy: segment .* lost" "$RUN_DIR/lan.log" || red "fake LAN: no lost segment"
grep -q "lossy: segment after its successor" "$RUN_DIR/lan.log" || red "fake LAN: no reordering"
grep -q "7080: \(three duplicate ACKs\|timeout\)" "$RUN_DIR/lan.log" || red "fake LAN: lost segments never sent again"
ktest_boot igb no

echo "== k3b: 3 x 64 MiB through the echo while MCP is asked every 200 ms"
NAME="nw-sock-k3b"
LOG="$RUN_DIR/k3b.log"
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
for attempt in 1 2; do
    rm -f "$LOG"
    cargo xtask qemu --name "$NAME" --net virtio --smp 4 --hostfwd "$FWD:8765" --timeout 600 --serial-log "$LOG" > "$RUN_DIR/$NAME.qemu.out" 2>&1 &
    QPID=$!
    if wait_line "$BOOT_LINE" "$LOG" 60; then break; fi
    echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
    stop_qemu
    [ "$attempt" = 2 ] && red "k3b: never booted"
done
python3 -I "$HERE/sockets.py" k3b "127.0.0.1:$FWD" "$TOKEN" 10.0.2.2 "$((HTTP_PORT + 2))" 64
stop_qemu

echo "== NW-7: 64 parallel downloads (igb, 16 CPUs) while MCP is asked every 200 ms"
NAME="nw-sock-par64"
LOG="$RUN_DIR/par64.log"
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;net_test_url=http://10.0.2.2:$HTTP_PORT"
for attempt in 1 2; do
    rm -f "$LOG"
    cargo xtask qemu --name "$NAME" --net igb --smp 16 --hostfwd "$FWD:8765" --timeout 600 --serial-log "$LOG" > "$RUN_DIR/$NAME.qemu.out" 2>&1 &
    QPID=$!
    if wait_line "$BOOT_LINE" "$LOG" 60; then break; fi
    echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
    stop_qemu
    [ "$attempt" = 2 ] && red "par64: never booted"
done
python3 -I "$HERE/sockets.py" par64 "127.0.0.1:$FWD" "$TOKEN"
grep -a "ktest.fetch_parallel" "$LOG" || true
stop_qemu
echo "nw sockets: green (programs as Linux, Wayland pair, fetch/UDP/echo on virtio and igb, k3a/k3b fakes, lossy web server, MCP during 3 x 64 MiB and during 64 parallel downloads)"
