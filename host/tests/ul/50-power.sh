#!/bin/bash
# slow
# UL-7: reboot and poweroff from the console shell in QEMU (V2-LANES UL-7 row), two normal boots:
#  1. reboot: injected `poweroff` (MCP send_keys) refused by init, MCP run's poweroff/poweroff -f/reboot EPERM,
#     a shell program gets SIGTERM before the reset, `reboot` typed on the (QEMU USB) keyboard ends QEMU.
#  2. poweroff: `poweroff` typed on the keyboard: init ends the programs, ACPI S5, QEMU ends.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/ul-50-power}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/ul"
LO="${BITSOS_PORT_LO:-28500}"
FWD=$((LO + 70))
TOKEN="ul7-power-token-3d81"
QMP="$RUN_DIR/qmp.sock"
fail() { echo "RED: $*"; exit 1; }

QPID=""
cleanup() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then kill "$QPID" 2>/dev/null || true; fi
    local vm
    vm=$(grep -aoE 'qemu pid [0-9]+' "$RUN_DIR"/qemu-*.out 2>/dev/null | tail -1 | awk '{print $3}' || true)
    if [ -n "$vm" ] && kill -0 "$vm" 2>/dev/null; then kill "$vm" 2>/dev/null || true; fi
    if [ -n "$QPID" ]; then wait "$QPID" 2>/dev/null || true; fi
}
trap cleanup EXIT

boot() {
    local mode="$1" name="$BITSOS_LANE-power-$1"
    LOG="$RUN_DIR/serial-$mode.log"
    cargo xtask image --name "$name" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info" > "$RUN_DIR/image-$mode.txt" 2>&1
    rm -f "$LOG" "$QMP"
    cargo xtask qemu --name "$name" --net virtio --hostfwd "$FWD:8765" --timeout 400 --serial-log "$LOG" -- \
        -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 \
        -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu-$mode.out" 2>&1 &
    QPID=$!
    python3 -I "$HERE/power_check.py" "$mode" "$FWD" "$TOKEN" "$QMP" || { cleanup; fail "$mode: the check failed"; }
    for _ in $(seq 1 60); do kill -0 "$QPID" 2>/dev/null || break; sleep 1; done
    cleanup
    QPID=""
    grep -aq "PANIC" "$LOG" && fail "a panic ($mode)"
    grep -aq "$TOKEN" "$LOG" && fail "the token appears in the log (R8i, $mode)"
    return 0
}

# Line number of the first match of $2 in $1 (empty when none).
at() { grep -anm1 -- "$2" "$1" | cut -d: -f1; }

boot reboot
L="$RUN_DIR/serial-reboot.log"
grep -aq "init: power off refused" "$L" || fail "init did not log the refused injected poweroff"
grep -aq "init: the system is going down for restart" "$L" || fail "init did not start the restart"
term=$(at "$L" 'kmsg text="\?ul7-got-term')
sent=$(at "$L" "init: sent SIGTERM to all processes")
reset=$(at "$L" "power reboot.requested")
[ -n "$term" ] || fail "the shell's program did not get SIGTERM"
[ -n "$sent" ] || fail "init did not report the SIGTERM round"
[ -n "$reset" ] || fail "no power reboot.requested (the clean reset path)"
[ "$term" -lt "$reset" ] || fail "SIGTERM came after the reset request"
grep -aq "ul reboot.restart" "$L" || fail "no ul reboot.restart event from reboot(2)"
grep -aq "power off.requested" "$L" && fail "a power-off was asked for in the reboot boot"

boot poweroff
L="$RUN_DIR/serial-poweroff.log"
grep -aq "init: the system is going down for power off" "$L" || fail "init did not start the power-off"
grep -aq "ul reboot.poweroff" "$L" || fail "no ul reboot.poweroff event from reboot(2)"
grep -aq "power off.requested" "$L" || fail "no power off.requested (the clean power-off path)"
grep -aq "power-off did not happen" "$L" && fail "S5 did not switch QEMU off"
echo "ul power: green (injected poweroff refused, MCP run EPERM, SIGTERM first, reboot and poweroff typed on the keyboard end QEMU)"
