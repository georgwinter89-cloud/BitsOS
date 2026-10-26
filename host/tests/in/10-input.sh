#!/bin/bash
# slow
# IN-2a: a normal boot with MCP reached from the host and four HID devices on a second qemu-xhci (two usb-kbd,
# usb-mouse, usb-tablet; no PS/2, i8042=off). QMP types keys (send-key, AltGr), moves the mouse and the tablet,
# checks every button and wheel on the tablet alone and on a mouse plugged in again (device_del/device_add:
# QEMU sends buttons to the pointer it polled last), turns Caps Lock on and off, holds a key and unplugs
# keyboards (device_del), then plugs a new keyboard in and types on it: input_check.py reads every step back
# through the MCP tool `input` (the evdev events and the held-key state) and fails on anything missing or a key
# that stays held.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/in-10-input}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/in"
LO="${BITSOS_PORT_LO:-26000}"
FWD=$((LO + 101))
TOKEN="in2a-scenario-token-7d3b"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=in-input

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

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
stop_qemu
rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 400 --serial-log "$LOG" -- \
    -machine i8042=off -device qemu-xhci,id=xb \
    -device usb-kbd,bus=xb.0,port=1,id=kbd1 -device usb-kbd,bus=xb.0,port=2,id=kbd2 \
    -device usb-mouse,bus=xb.0,port=3,id=mouse1 -device usb-tablet,bus=xb.0,port=4,id=tab1 \
    -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!

python3 -I "$HERE/input_check.py" "$FWD" "$TOKEN" "$QMP"
grep -a "input \|usb driver.bound\|usb device.detached" "$LOG" | grep -v "loader.line" | tail -40 || true
if grep -aq "lock.violation" "$LOG"; then echo "RED: lock order broken"; grep -a "lock.violation" "$LOG" | head; exit 1; fi
if grep -aq "hid.leds_failed\|hid.pipe_stopped\|PANIC" "$LOG"; then echo "RED: a failure line in the log"; exit 1; fi
if grep -aq "$TOKEN" "$LOG"; then echo "RED: the token appears in the log (R8i)"; exit 1; fi
# IN-3: the VT turns Num Lock on at start, so Caps Lock on is 0x3 (0x2 without the VT).
grep -aq "input hid.leds .*leds=0x[23]" "$LOG" || { echo "RED: Caps Lock LED never sent"; exit 1; }
echo "in input: green (keys with AltGr, mouse buttons and wheels, tablet with buttons, Caps Lock LEDs, two keyboards, unplug while held, hot plug)"
