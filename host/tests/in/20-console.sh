#!/bin/bash
# slow
# IN-3: the kernel console on QEMU's 2560x1440 GOP screen. Since UL-3a init keeps BusyBox ash on the console
# (`/ # `) and the kernel monitor (`bits>`) reads only while no session holds it, so two boots:
#  1. shell (a normal boot): a USB keyboard (QMP send-key: physical keys) types umlauts and AltGr characters into
#     the shell; QEMU's screendump must equal the Rust model's picture pixel for pixel; MCP send_keys (injected)
#     cannot reboot; a mouse move draws the pointer. IN-quiet: the console's default level keeps MCP's Info request
#     lines off the screen; console_log info (/sys/bitsos) shows them and a command typed halfway stays whole below.
#  2. monitor (CONFIG init=off): `log level off` and clear at `bits>`; an injected reboot is refused (R8c).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/in-20-console}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/in"
LO="${BITSOS_PORT_LO:-26000}"
FWD=$((LO + 102))
TOKEN="in3-scenario-token-51c9"
QMP="$RUN_DIR/qmp.sock"
NAME=in-console

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
    cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 400 --serial-log "$LOG" -- \
        -machine i8042=off -device qemu-xhci,id=xb \
        -device usb-kbd,bus=xb.0,port=1,id=kbd1 -device usb-mouse,bus=xb.0,port=2,id=mouse1 \
        -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu-$mode.out" 2>&1 &
    QPID=$!
    python3 -I "$HERE/console_check.py" "$mode" "$FWD" "$TOKEN" "$QMP" "$RUN_DIR" "$(pwd)"
    cleanup
    QPID=""
    if grep -aq "lock.violation" "$LOG"; then echo "RED: lock order broken ($mode)"; grep -a "lock.violation" "$LOG" | head; exit 1; fi
    if grep -aq "PANIC" "$LOG"; then echo "RED: a panic ($mode)"; exit 1; fi
    if grep -aq "$TOKEN" "$LOG"; then echo "RED: the token appears in the log (R8i, $mode)"; exit 1; fi
}

boot shell ""
grep -aq "init: started the console shell" "$RUN_DIR/serial-shell.log" || { echo "RED: init did not start the console shell"; exit 1; }
grep -aq 'console log.level level=off was=warn source=sysfs' "$RUN_DIR/serial-shell.log" || { echo "RED: no log.level event for console_log off (default warn)"; exit 1; }
if grep -aq "power reboot.requested\|mcp reboot.requested" "$RUN_DIR/serial-shell.log"; then echo "RED: a reboot was asked for in the shell boot"; exit 1; fi

NAME=in-console-mon
boot monitor ";init=off"
grep -aq "ul init.off" "$RUN_DIR/serial-monitor.log" || { echo "RED: no ul init.off event (CONFIG init=off)"; exit 1; }
grep -aq "console monitor.refused .*reboot" "$RUN_DIR/serial-monitor.log" || { echo "RED: no monitor.refused event for the injected reboot"; exit 1; }
grep -aq 'console log.level level=off was=warn source=monitor' "$RUN_DIR/serial-monitor.log" || { echo "RED: no log.level event for 'log level off' (default warn)"; exit 1; }
echo "in console: green (shell: quiet by default, umlauts and AltGr typed, screendump = model, injected reboot not found, pointer, typed line whole under log lines; monitor with init=off: quiet, clear, injected reboot refused)"
