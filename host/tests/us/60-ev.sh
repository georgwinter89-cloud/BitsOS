#!/bin/bash
# US-ev (B4): HID reports stamped with the controller interrupt's time and handed on by the KernelRt event thread,
# stick reads (transfer ends on the Normal completion thread) going on while the HID pipes flood. One boot with
# usb-kbd, usb-tablet and usb-mouse next to the boot stick on xhci.0; usbhid_raw=on leaves them to the test's own
# timed pipes (idle 4 ms: a report every 4 ms from each). Timing is only logged (Mini-PC check list).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/us-60-ev}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LOG="$RUN_DIR/serial.log"
NAME=us-ev

cargo xtask image --name "$NAME" --ktest --config-extra "ktest=us.ev,us.xhci.running;usbhid_raw=on;loglevel=info"
cargo xtask qemu --name "$NAME" --smp 4 --timeout 240 --serial-log "$LOG" -- \
    -device usb-kbd,bus=xhci.0 -device usb-tablet,bus=xhci.0 -device usb-mouse,bus=xhci.0 || true

grep -a "KTEST\|ktest.ev_" "$LOG" || true
fail() { echo "RED: $*"; exit 1; }
grep -aq "KTEST done passed=.* failed=0" "$LOG" || fail "ktests not green"
for t in us.ev.stamps us.ev.flood_read us.xhci.running; do
    grep -aq "KTEST ok $t " "$LOG" || fail "$t did not pass (skipped?)"
done
n=$(grep -ac "ktest.ev_stamps" "$LOG" || true)
[ "$n" -ge 6 ] || fail "$n pipe summaries (want 3 devices in both tests)"
echo "us/60-ev: green"
