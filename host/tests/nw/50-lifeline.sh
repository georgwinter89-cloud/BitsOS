#!/bin/bash
# slow
# NW-4: the MCP lifeline on the real kernel stack, on both QEMU cards (virtio-net, then igb), each driven by MSI-X.
# Boot A (normal boot, token, -smp 4, QMP): the MCP reserve under pressure (the guest's ktest nw.reserve.pressure
# holds 1,000 connections to a host sink and refuses all socket memory; MCP still takes 3 side by side and answers
# `log`, three rounds), a CPU busy 20 s without yielding (ktest nw.busy.spin) while `log` keeps being answered,
# then the cable out 10 s (QMP set_link off/on): DHCP asks again at link-up and MCP comes back by itself.
# Boot B (mcp_conns=3): V1's mcp-http-test.mjs transport cases (status codes, 1-byte writes, LF, keep-alive,
# pipelining, Connection: close, HTTP/1.0, side by side, knock/evict, 408, idle close).
# Every boot must show the card on MSI-X and the stack in interrupt mode, and no Error line from net.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/nw-50-lifeline}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/nw"
FWD=24540        # host port -> guest 8765 (MCP)
SINK=24541       # the reserve test's sink (guest: 10.0.2.2:24541)
TOKEN="nw4-lifeline-token-5c1e"
BOOT_LINE="BdsDxe: loading"
CARDS="${NW4_CARDS:-virtio igb}"

QPID=""; NAME=""
stop_own() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
        local q; q=$(cat "target/xtask/qemu-$NAME.pid" 2>/dev/null || true)
        if [ -n "$q" ] && tr '\0' ' ' < "/proc/$q/cmdline" 2>/dev/null | grep -q -- "-name v2-[A-Za-z0-9]*-$NAME"; then kill "$q" 2>/dev/null || true; fi
        kill "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    QPID=""
}
trap stop_own EXIT
wait_line() { # $1 = pattern, $2 = file, $3 = seconds
    local n=$(( $3 * 10 ))
    for _ in $(seq 1 "$n"); do grep -aq "$1" "$2" 2>/dev/null && return 0; sleep 0.1; done
    return 1
}
boot() { # $1 = name, $2 = card, $3 = log, $4 = extra config; one retry after a firmware stall
    NAME="$1"
    cargo xtask image --name "$1" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info$4"
    for attempt in 1 2; do
        rm -f "$RUN_DIR/qmp.sock" "$3"
        cargo xtask qemu --name "$1" --net "$2" --smp 4 --hostfwd "$FWD:8765" --timeout 600 --serial-log "$3" -- \
            -qmp "unix:$RUN_DIR/qmp.sock,server=on,wait=off" > "$RUN_DIR/$1.qemu.out" 2>&1 &
        QPID=$!
        if wait_line "$BOOT_LINE" "$3" 60; then return 0; fi
        echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
        stop_own
    done
    echo "RED: $1 never booted"; exit 1
}
red() { echo "RED: $*"; exit 1; }
common_checks() { # $1 = log, $2 = card
    local log="$1" card="$2" drv
    drv=$([ "$card" = igb ] && echo igc || echo virtio-net)
    grep -aq "pci msi.alloc function=.* driver=$drv mode=msix" "$log" || red "$card: no MSI-X set for $drv"
    grep -aq "net stack.poll_mode mode=irq" "$log" || red "$card: the stack never went to interrupt mode"
    grep -aq "net card.irq_mode ifname=.* irq=true" "$log" || red "$card: the card never said it is interrupt-driven"
    if grep -a "^\[[ 0-9.]*\] E net " "$log"; then red "$card: an Error line from net"; fi
    if grep -aq "msi.storm " "$log"; then red "$card: an interrupt storm"; fi
    if grep -aq "$TOKEN" "$log"; then red "$card: the token is in the log (R8i)"; fi
}

for card in $CARDS; do
    nd=n0
    echo "== $card: reserve, busy CPU, cable"
    LOG="$RUN_DIR/$card-a.log"
    boot "nw-life-$card" "$card" "$LOG" ""
    python3 -I "$HERE/lifeline.py" reserve "$FWD" "$TOKEN" "$LOG" "$SINK"
    python3 -I "$HERE/lifeline.py" busy "$FWD" "$TOKEN"
    python3 -I "$HERE/lifeline.py" link "$FWD" "$TOKEN" "$RUN_DIR/qmp.sock" "$nd"
    stop_own
    grep -a "ktest\.\(reserve\|busy\)\|iface.link\|dhcp\.\(restart\|lease\)\|card.irq_mode\|poll_mode\|msi.alloc\|tcp.listen.*8765" "$LOG" || true
    common_checks "$LOG" "$card"
    grep -aq "ktest.reserve_pressure_ready conns=1000" "$LOG" || red "$card: no reserve pressure"
    grep -aq "net iface.link_down" "$LOG" || red "$card: link down not seen"
    grep -aq "net dhcp.restart" "$LOG" || red "$card: DHCP did not ask again at link-up"
    # Same network behind the cable: the same address comes back as a renewal of the kept lease.
    sed -n '/net dhcp.restart/,$p' "$LOG" | grep -aq "net dhcp.\(lease\|renewed\)" || red "$card: no lease after the cable came back"
    grep -aq "tcp.listen ip=0.0.0.0 port=8765 .*private_only=true reserve=true" "$LOG" || red "$card: MCP listener not private with reserve"

    echo "== $card: transport cases"
    LOG="$RUN_DIR/$card-b.log"
    boot "nw-trans-$card" "$card" "$LOG" ";mcp_conns=3"
    python3 -I "$HERE/lifeline.py" transport "$FWD" "$TOKEN"
    stop_own
    common_checks "$LOG" "$card"
done
echo "nw lifeline: green ($CARDS: MSI-X, reserve under pressure, busy CPU, cable 10 s, transport cases)"
