#!/bin/bash
# CP-5: power-off and reboot in QEMU (V2-LANES CP-5 row: "QMP: poweroff -> SHUTDOWN guest-shutdown, reboot
# returns"; functional only). The QemuOnly ktest cp.power.qemu_action does what CONFIG cp_power_test= asks after
# quiescing the drivers:
#  1. poweroff: GPEs off, SLP_TYP from \_S5 with SLP_EN -> QEMU reports SHUTDOWN reason guest-shutdown (with
#     -action shutdown=pause QEMU stays to deliver the event; without it, it may exit before the event is out).
#  2. reboot: the reset order (q35: FADT 0xCF9 first) -> QEMU reports RESET guest-reset and, with the reboot
#     action set to reset (xtask always passes -no-reboot; -action reboot=reset comes after it), the machine
#     starts again: a second 'kernel start'.
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
fail=0
check() { if grep -aEq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3 (no '$2' in $(basename "$1"))"; fail=1; fi; }

run() { # tag, action, qmp helper args, extra qemu args...
    local tag="$1" action="$2" helper="$3"
    shift 3
    local name="$BITSOS_LANE-$tag" sock="$RUN/$tag.qmp" log="$RUN/$tag.log"
    rm -f "$sock" "$log"
    cargo xtask image --name "$name" --ktest --config-extra "ktest=cp.power.qemu_action;cp_power_test=$action" > "$RUN/$tag-image.txt" 2>&1
    cargo xtask qemu --name "$name" --timeout 180 --boot-retries 4 --serial-log "$log" -- -qmp "unix:$sock,server,wait=off" "$@" > "$RUN/$tag-qemu.txt" 2>&1 &
    local qpid=$!
    # shellcheck disable=SC2086
    python3 -I host/tests/cp/qmp_events.py "$sock" 150 $helper --serial "$log" --quit > "$RUN/$tag-events.txt" 2>&1 || true
    for _ in $(seq 1 30); do kill -0 "$qpid" 2>/dev/null || break; sleep 1; done
    # Stop our own QEMU by the PID xtask printed (killing cargo alone would leave it running: the reboot run
    # keeps rebooting and the paused power-off waits).
    local vm
    vm=$(grep -aoE 'qemu pid [0-9]+' "$RUN/$tag-qemu.txt" | tail -1 | awk '{print $3}' || true)
    if [ -n "$vm" ] && kill -0 "$vm" 2>/dev/null; then kill "$vm" 2>/dev/null || true; fi
    if kill -0 "$qpid" 2>/dev/null; then kill "$qpid" 2>/dev/null || true; fi
    wait "$qpid" 2>/dev/null || true
    touch "$log"
    echo "--- $tag events:"; cat "$RUN/$tag-events.txt"
}

run off poweroff "--until-shutdown" -action shutdown=pause
check "$RUN/off-events.txt" '"event": "SHUTDOWN".*"reason": "guest-shutdown"' "power-off: QEMU saw the guest switch itself off (SHUTDOWN guest-shutdown)"
check "$RUN/off.log" 'power off.gpes ' "power-off: GPEs disabled and cleared first"
check "$RUN/off.log" 'power off.slp_typ ' "power-off: SLP_TYP written"
if grep -aq 'KTEST FAIL cp.power.qemu_action' "$RUN/off.log"; then echo "FAIL power-off came back"; fail=1; fi

run reboot reboot "--resets 1 --boots 2" -action reboot=reset
check "$RUN/reboot-events.txt" '"event": "RESET".*"reason": "guest-reset"' "reboot: QEMU saw a guest reset"
check "$RUN/reboot.log" 'power reboot.step n=1 method="FADT reset register I/O 0xcf9' "reboot: the FADT reset register first"
n=$(grep -ac 'kernel start' "$RUN/reboot.log" || true)
if [ "${n:-0}" -ge 2 ]; then echo "ok   reboot: the machine came back ($n kernel starts)"; else echo "FAIL reboot: $n kernel start(s), wanted 2"; fail=1; fi
exit $fail
