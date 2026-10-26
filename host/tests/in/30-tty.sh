#!/bin/bash
# slow
# IN-4: the console terminal with physical keys (QEMU's USB keyboard through QMP): a raw-mode program started
# with the monitor's `run` reads the keys as a Linux console sends them, VTIME times out and the settings come
# back; Ctrl+C ends a program blocked in a canonical read with SIGINT (exit code 130 on the console); the evdev
# nodes exist for MCP's devices and the USB keyboard and mouse.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/in-30-tty}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/in"
LO="${BITSOS_PORT_LO:-26000}"
FWD=$((LO + 103))
TOKEN="in4-scenario-token-7d02"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=in-tty

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

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
stop_qemu
rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 300 --serial-log "$LOG" -- \
    -machine i8042=off -device qemu-xhci,id=xb \
    -device usb-kbd,bus=xb.0,port=1,id=kbd1 -device usb-mouse,bus=xb.0,port=2,id=mouse1 \
    -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!

python3 -I "$HERE/tty_check.py" "$FWD" "$TOKEN" "$QMP" "$RUN_DIR"
if grep -aq "lock.violation" "$LOG"; then echo "RED: lock order broken"; grep -a "lock.violation" "$LOG" | head; exit 1; fi
if grep -aq "PANIC" "$LOG"; then echo "RED: a panic"; exit 1; fi
grep -aq "proc signal.fatal .*signal=SIGINT" "$LOG" || { echo "RED: no SIGINT end for the program Ctrl+C was typed at"; exit 1; }
grep -aq "input evdev.node node=/dev/input/event0 name=\"BitsOS MCP keyboard\"" "$LOG" || { echo "RED: no evdev node for MCP's keyboard"; exit 1; }
if grep -aq "$TOKEN" "$LOG"; then echo "RED: the token appears in the log (R8i)"; exit 1; fi
echo "in tty: green (raw program read physical keys, VTIME, settings restored, Ctrl+C = SIGINT for the foreground program, evdev nodes)"
