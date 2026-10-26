#!/bin/bash
# US-1: xHCI bring-up with several controllers. Besides the qemu-xhci every QEMU run has (the boot stick hangs on
# it), a second qemu-xhci with a USB keyboard and an nec-usb-xhci are added, so the driver must take three
# controllers, run the US ktests on all of them and record the keyboard's port at takeover.
# Fails if a controller did not come up, failed, reported HSE, or fewer than the five us.xhci tests passed.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/us-10-xhci}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LOG="$RUN_DIR/serial.log"

cargo xtask image --name us-xhci3 --ktest --config-extra "ktest=us.xhci,contract.quiesce;loglevel=debug"
cargo xtask qemu --name us-xhci3 --timeout 120 --serial-log "$LOG" -- \
    -device qemu-xhci,id=x2 -device usb-kbd,bus=x2.0 -device nec-usb-xhci,id=x3 || true

grep -a "KTEST" "$LOG" || true
fail() { echo "RED: $*"; exit 1; }
grep -aq "KTEST done passed=.* failed=0" "$LOG" || fail "ktests not green"
ok=$(grep -ac "KTEST ok us.xhci." "$LOG" || true)
[ "$ok" -ge 5 ] || fail "only $ok us.xhci tests passed (want 5)"
up=$(grep -a "xhci.up ctrl=" "$LOG" | grep -ao "ctrl=xhci[0-9]*" | sort -u | wc -l)
[ "$up" -ge 3 ] || fail "$up controllers came up (want 3)"
grep -aq "ktest.controllers running=3" "$LOG" || fail "the running test did not see three controllers"
grep -aq "device=0x194" "$LOG" || fail "the nec-usb-xhci (1033:0194) was not probed"
if grep -aq "xhci.failed\|xhci.noop_failed\|xhci.probe_failed" "$LOG"; then fail "a controller failed"; fi
# QEMU raises no port change event for a device present before RS (US-2 scans the ports); the takeover record
# must show the keyboard on the second controller (xhci1, PCI order) and the stick on the first.
grep -aq "ports_at_takeover ctrl=xhci1 ports=\[[0-9]" "$LOG" || fail "keyboard on x2 not seen at takeover"
grep -aq "ports_at_takeover ctrl=xhci0 ports=\[[0-9]" "$LOG" || fail "boot stick not seen at takeover"
grep -a "ktest.wrap" "$LOG"
echo "us-10-xhci: green"
