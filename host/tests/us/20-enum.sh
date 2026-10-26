#!/bin/bash
# US-2: enumeration on a second qemu-xhci with hubs and devices. Topology (QEMU bus port paths; qemu-xhci puts
# its USB 3 ports first, so bus port n is xHCI port n (USB 3) or n + 4 (USB 2) and the log shows those):
#   1      usb-hub (8 ports, port power switched by the driver)          -> xhci1-5
#   1.1    usb-kbd            1.2  usb-tablet        1.3  usb-storage (full speed behind the full speed hub)
#   1.4    usb-hub            1.4.1 usb-mouse        (route string 0x14, two tiers)
#   2      usb-kbd            3    usb-tablet        (high speed, endpoint 0 size 64)
#   4      usb-storage (SuperSpeed)                                       -> xhci1-4
# The us.enum ktests check the controller's view (route strings, TT, rings past their links, a restart that
# finds everything again); this script checks the log for the nine devices of the second controller (the boot
# stick sits on the first), the two hubs, the routes and the endpoint 0 sizes (8 = the Evaluate Context path).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/us-20-enum}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LOG="$RUN_DIR/serial.log"
truncate -s 16M "$RUN_DIR/d2.img"
truncate -s 16M "$RUN_DIR/d3.img"

cargo xtask image --name us-enum --ktest --config-extra "ktest=us.enum,us.xhci;loglevel=info"
cargo xtask qemu --name us-enum --timeout 150 --serial-log "$LOG" -- \
    -drive "if=none,id=d2,format=raw,file=$RUN_DIR/d2.img" -drive "if=none,id=d3,format=raw,file=$RUN_DIR/d3.img" \
    -device qemu-xhci,id=xb \
    -device usb-hub,bus=xb.0,port=1,port-power=on \
    -device usb-kbd,bus=xb.0,port=1.1 -device usb-tablet,bus=xb.0,port=1.2 \
    -device usb-storage,bus=xb.0,port=1.3,drive=d2 \
    -device usb-hub,bus=xb.0,port=1.4 -device usb-mouse,bus=xb.0,port=1.4.1 \
    -device usb-kbd,bus=xb.0,port=2 -device usb-tablet,bus=xb.0,port=3 \
    -device usb-storage,bus=xb.0,port=4,drive=d3 || true

grep -a "KTEST\|usb device\.\|usb hub\.\|enum\.failed\|port\.given_up" "$LOG" || true
fail() { echo "RED: $*"; exit 1; }
grep -aq "KTEST done passed=.* failed=0" "$LOG" || fail "ktests not green"
for t in boot topology rings_wrap restart; do
    grep -aq "KTEST ok us.enum.$t " "$LOG" || fail "us.enum.$t did not pass"
done
# Before the restart test: the first enumeration of xhci1 (the scenario's controller, PCI order).
first=$(awk '/KTEST ok us.enum.boot/{exit} {print}' "$LOG")
n=$(echo "$first" | grep -ac "usb device.attached device=xhci1-" || true)
[ "$n" -eq 9 ] || fail "xhci1: $n devices attached at boot (want 9)"
hubs=$(echo "$first" | grep -ac "usb hub.bound hub=xhci1-" || true)
[ "$hubs" -eq 2 ] || fail "xhci1: $hubs hubs bound (want 2)"
echo "$first" | grep -aq "device.attached device=xhci1-5.4.1 .*route=0x14 depth=2" || fail "mouse behind two hubs: route 0x14"
echo "$first" | grep -aq "device.attached device=xhci1-5.3 .*speed=full .*vendor=0x46f4" || fail "storage behind the hub"
echo "$first" | grep -aq "device.attached device=xhci1-4 .*speed=super .*vendor=0x46f4" || fail "SuperSpeed storage on the USB 3 half"
echo "$first" | grep -aq "hub.bound hub=xhci1-5 ports=8" || fail "hub on xhci1-5"
kb=$(echo "$first" | grep -a "device.attached device=xhci1-5\." | grep -a "vendor=0x627" | grep -ac "mps0=8" || true)
[ "$kb" -eq 3 ] || fail "$kb HID devices behind the hub with endpoint 0 size 8 (want 3: Evaluate Context path)"
hs=$(echo "$first" | grep -a "device.attached device=xhci1-[67] " | grep -ac "speed=high .*mps0=64" || true)
[ "$hs" -eq 2 ] || fail "$hs high speed HID devices on root ports (want 2)"
if grep -aq "enum.failed\|port.given_up\|xhci.failed\|xhci.slot_leaked\|lock.violation\|xhci.control_timeout" "$LOG"; then
    fail "a failure line in the log"
fi
echo "us-20-enum: green"
