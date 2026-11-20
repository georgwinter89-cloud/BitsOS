#!/bin/bash
# GP-R2: a USB storage function without a medium, as the Brother device (04f9:0228) on the Gaming-PC's port 5 that
# registered as msd0 with blocks=0 and made so.sysk.diskstats red in round 1 (read msd0: BeyondEnd). QEMU's
# usb-storage with an empty removable drive is that function. The boot runs so.sysk.diskstats and block.usb.stick:
# the empty disk is listed in /proc/diskstats with size 0 and is not read, the boot stick is still the one read
# and still recognised as the stick. QEMU registers the empty disk after the stick (msd1, the second xHCI), so this
# boot shows the empty disk handled; the order of round 1 (the empty one first) is the host-side choice among the
# readable disks.
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
NAME="$BITSOS_LANE-empty"
LOG="$RUN/empty.log"
cargo xtask image --name "$NAME" --ktest --config-extra "ktest=so.sysk.diskstats,block.usb.stick;loglevel=info"
cargo xtask qemu --name "$NAME" --timeout 180 --serial-log "$LOG" -- \
    -drive if=none,id=empty0 -device qemu-xhci,id=xe -device usb-storage,bus=xe.0,port=1,drive=empty0,removable=on > "$RUN/empty.out" 2>&1 || true
touch "$LOG"
fail=0
has() { if grep -aEq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3 (no '$2' in $(basename "$1"))"; fail=1; fi; }
grep -a "KTEST\|block disk\.\|msd.*blocks" "$LOG" | cut -c1-200 || true
has "$LOG" 'blocks=0' "an empty USB disk registered (blocks=0)"
has "$LOG" 'KTEST ok so.sysk.diskstats ' "so.sysk.diskstats is green beside a disk without a medium"
has "$LOG" 'KTEST ok block.usb.stick ' "block.usb.stick still finds the boot stick"
has "$LOG" 'KTEST done passed=[0-9]+ failed=0' "no ktest of the filter failed (block.usb.stick_write comes with the prefix)"
exit $fail
