#!/bin/bash
# slow
# IN-4: the console terminal with physical keys (QEMU's USB keyboard through QMP): a raw-mode program reads the
# keys as a Linux console sends them, VTIME times out and the settings come back; Ctrl+C ends a program blocked
# in a canonical read with SIGINT; the evdev nodes exist for MCP's devices and the USB keyboard and mouse.
# Since UL-3a init keeps BusyBox ash on the console (`/ # `) and the kernel monitor (`bits>`) reads only while no
# session holds it, so two boots (as 20-console.sh):
#  1. shell (a normal boot): the programs typed at ash; `echo $?` says 130 after the Ctrl+C.
#  2. monitor (CONFIG init=off): the programs started with the monitor's `run`; it shows `exit code 130`.
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

boot() {
    local mode="$1" extra="$2"
    LOG="$RUN_DIR/serial-$mode.log"
    cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info$extra"
    stop_qemu
    rm -f "$LOG" "$QMP"
    cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 300 --serial-log "$LOG" -- \
        -machine i8042=off -device qemu-xhci,id=xb \
        -device usb-kbd,bus=xb.0,port=1,id=kbd1 -device usb-mouse,bus=xb.0,port=2,id=mouse1 \
        -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu-$mode.out" 2>&1 &
    QPID=$!
    python3 -I "$HERE/tty_check.py" "$mode" "$FWD" "$TOKEN" "$QMP" "$RUN_DIR"
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then kill "$QPID" 2>/dev/null || true; fi
    stop_qemu
    wait "$QPID" 2>/dev/null || true
    QPID=""
    if grep -aq "lock.violation" "$LOG"; then echo "RED: lock order broken ($mode)"; grep -a "lock.violation" "$LOG" | head; exit 1; fi
    if grep -aq "PANIC" "$LOG"; then echo "RED: a panic ($mode)"; exit 1; fi
    grep -aq "proc signal.fatal .*signal=SIGINT" "$LOG" || { echo "RED: no SIGINT end for the program Ctrl+C was typed at ($mode)"; exit 1; }
    grep -aq "input evdev.node node=/dev/input/event0 name=\"BitsOS MCP keyboard\"" "$LOG" || { echo "RED: no evdev node for MCP's keyboard ($mode)"; exit 1; }
    if grep -aq "$TOKEN" "$LOG"; then echo "RED: the token appears in the log (R8i, $mode)"; exit 1; fi
}

boot shell ""
grep -aq "init: started the console shell" "$RUN_DIR/serial-shell.log" || { echo "RED: init did not start the console shell"; exit 1; }
boot monitor ";init=off"
grep -aq "ul init.off" "$RUN_DIR/serial-monitor.log" || { echo "RED: no ul init.off event (CONFIG init=off)"; exit 1; }
echo "in tty: green (shell and monitor: raw program read physical keys, VTIME, settings restored, Ctrl+C = SIGINT for the foreground program, evdev nodes)"
