#!/bin/bash
# slow
# DS-5 in QEMU (2560x1440 GOP screen, desktop=on: QEMU's auto means off): the kernel's boot screen hands over to
# /bin/bitscomp, which shows the same picture and continues the list (ds_boot_qemu.py).
#  1. first: the last kernel frame and bitscomp's first frame compared from QMP screendumps (the ring and the title
#     pixel for pixel, the rest within tolerance outside the step list and its labels); the desktop shows after a
#     fade of at most 400 ms; the times bitscomp logged (start -> desktop) are printed (numbers only).
#  2. key: a key while bitscomp's scene is up shows the desktop without the fade.
# A boot whose screendumps missed bitscomp's short scene (ds_boot_qemu.py exit 75, MISSED) is booted again, up to 3
# tries; a wrong picture is red at once.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/ds-61-boot-qemu}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/ds"
QMP="$RUN_DIR/qmp.sock"
NAME=ds-boot

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

cargo xtask image --name "$NAME" --config-extra "loglevel=info;desktop=on"
boot() {
    local mode="$1" try="${2:-1}" rc=0
    LOG="$RUN_DIR/serial-$mode.log"
    stop_qemu
    rm -f "$LOG" "$QMP"
    cargo xtask qemu --name "$NAME" --timeout 400 --serial-log "$LOG" -- \
        -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 \
        -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu-$mode.out" 2>&1 &
    QPID=$!
    python3 -I "$HERE/ds_boot_qemu.py" "$mode" "$QMP" "$RUN_DIR" "$LOG" || rc=$?
    cleanup
    QPID=""
    if grep -aq "PANIC" "$LOG"; then echo "RED: a panic ($mode)"; exit 1; fi
    if [ "$rc" = 75 ] && [ "$try" -lt 3 ]; then
        echo "  the screendumps missed the scene ($mode, try $try of 3): booting again"
        boot "$mode" $((try + 1))
        return
    fi
    [ "$rc" = 0 ] || { echo "RED ($mode): ds_boot_qemu.py exit $rc (try $try)"; exit 1; }
    grep -aq "console bootscreen.end reason=handover" "$LOG" || { echo "RED ($mode): the kernel did not hand its boot screen over"; exit 1; }
}
boot first
boot key
echo "ds boot scene in QEMU: green (bitscomp's first frame = the kernel's last, desktop after a short fade, a key skips it)"
