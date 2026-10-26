#!/bin/bash
# slow
# IN-2b: gamepads over QEMU usb-redir (usbredir_pad.mjs, V1's pad model): Georg's DragonRise plugged in, pressed,
# unplugged with buttons held and plugged in again (hot plug, no string descriptor asked for), a DualShock 3 that
# reports only after GET_REPORT of feature report 0xF2, a high speed pad whose 8 ms bInterval gets the 1 ms wish.
# pad_check.py reads every step back through the MCP tool `input`. Rates are shown, never judged (QEMU).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/in-40-pad}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/in"
LO="${BITSOS_PORT_LO:-26000}"
FWD=$((LO + 104))
P1=$((LO + 105)); P2=$((LO + 106)); P3=$((LO + 107))
TOKEN="in2b-scenario-token-5e1c"
LOG="$RUN_DIR/serial.log"
NAME=in-pad

command -v node > /dev/null || { echo "SKIP: node not installed (usbredir pad model)"; exit 0; }

# PID of the QEMU this script started (checked against its -name, never another process).
qemu_pid() {
    local pf="target/xtask/qemu-$NAME.pid" pid
    [ -f "$pf" ] || return 0
    pid="$(head -1 "$pf" 2>/dev/null || true)"
    case "$pid" in ''|*[!0-9]*) return 0 ;; esac
    if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then echo "$pid"; fi
}
stop_qemu() {
    local pid
    pid="$(qemu_pid)"
    if [ -n "$pid" ]; then
        kill "$pid" 2>/dev/null || true
        for _ in $(seq 1 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
        if kill -0 "$pid" 2>/dev/null; then kill -9 "$pid" 2>/dev/null || true; fi
    fi
    rm -f "target/xtask/qemu-$NAME.pid"
}
QPID=""
cleanup() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then kill "$QPID" 2>/dev/null || true; fi
    stop_qemu
    if [ -n "$QPID" ]; then wait "$QPID" 2>/dev/null || true; fi
}
trap cleanup EXIT

pad() { echo "-chardev socket,id=pad$1,host=127.0.0.1,port=$2,server=on,wait=off,nodelay=on -device usb-redir,chardev=pad$1,bus=xb.0"; }
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
stop_qemu
rm -f "$LOG"
# shellcheck disable=SC2046
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 400 --serial-log "$LOG" -- \
    -machine i8042=off -device qemu-xhci,id=xb $(pad 1 "$P1") $(pad 2 "$P2") $(pad 3 "$P3") > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!

python3 -I "$HERE/pad_check.py" "$FWD" "$TOKEN" "$P1" "$P2" "$P3"
grep -a "input hid\.\|usb driver.bound\|usb device.detached" "$LOG" | grep -v "loader.line" | tail -30 || true
if grep -aq "lock.violation" "$LOG"; then echo "RED: lock order broken"; grep -a "lock.violation" "$LOG" | head; exit 1; fi
if grep -aq "hid.pipe_stopped\|PANIC" "$LOG"; then echo "RED: a failure line in the log"; exit 1; fi
if grep -aq "$TOKEN" "$LOG"; then echo "RED: the token appears in the log (R8i)"; exit 1; fi
grep -aq "input hid.sixaxis_wake .*report=0xf2" "$LOG" || { echo "RED: no hid.sixaxis_wake event for 0xF2"; exit 1; }
echo "in pad: green (DragonRise twice with hot plug and no strings, DualShock 3 woken by 0xF2, 1 ms wish at low and high speed)"
