#!/bin/bash
# US-R: a stalled USB stick costs a retry, not the boot disk. One boot with one CPU (us.recover.cpu_stop holds it
# for seconds), the xtask boot stick and three QEMU usb-storage sticks on image files of this run only:
#   scr.img  id 11, scratch range: traffic through the block layer during a real port reset in place
#   gup.img  id 12: every reset fails (injected in the kernel): the device goes, its port finds it again
#   stl.img  id 13: the script throttles the drive under the stick (QMP) until a data stage runs out of time
# What QEMU's device cannot do (a failing Bulk-Only reset) is injected at the kernel's link.
# Never judges time: only what happened.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/us-55-recover}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
SOCK="$RUN_DIR/qmp.sock"
LOG="$RUN_DIR/serial.log"
NAME=us-recover
IMG="target/xtask/$NAME.img"
BOOT_LINE="BdsDxe: loading"

# Image maker: path size block_size id flags ranges ("lba:blocks,..."), as in 50-msd.sh.
MAKE='
import struct, sys
path, size, bs, ident, flags = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
ranges = [tuple(int(x) for x in r.split(":")) for r in sys.argv[6].split(",") if r]
T = [(i * 7) & 0xFF for i in range(512)]
ROW = [bytes(a ^ t for t in T) for a in range(256)]
def pattern(off, n):
    s0 = off >> 9
    return b"".join(ROW[((s * 31) ^ (s >> 8)) & 0xFF] for s in range(s0, s0 + n // 512))
with open(path, "wb") as f:
    f.truncate(size)
    hdr = b"BITSOS-US4-PATTERN-V1\0\0\0" + struct.pack("<III", ident, flags, len(ranges))
    hdr += b"".join(struct.pack("<QQ", a, b) for a, b in ranges)
    f.write(hdr)
    for lba, blocks in ranges:
        off, left = lba * bs, blocks * bs
        while left:
            k = min(left, 4 << 20)
            f.seek(off)
            f.write(pattern(off, k))
            off += k
            left -= k
'
mk() { python3 -I -c "$MAKE" "$@"; }
MIB=$((1 << 20))
mk "$RUN_DIR/scr.img" $((32 * MIB)) 512 11 1 "1:4096,4097:61439"
mk "$RUN_DIR/gup.img" $((4 * MIB))  512 12 0 "1:8191"
mk "$RUN_DIR/stl.img" $((32 * MIB)) 512 13 0 "1:65535"

qmp() {
    python3 -I -c '
import json, socket, sys
s = socket.socket(socket.AF_UNIX); s.settimeout(20); s.connect(sys.argv[1]); f = s.makefile("rw")
f.readline()
for c in ({"execute": "qmp_capabilities"}, json.loads(sys.argv[2])):
    f.write(json.dumps(c) + "\n"); f.flush()
    while True:
        r = json.loads(f.readline())
        if "return" in r or "error" in r:
            break
    if "error" in r:
        sys.exit("qmp: %s" % r)
' "$SOCK" "$1"
}
wait_line() {
    local n=$(( $3 * 10 ))
    for _ in $(seq 1 "$n"); do grep -aq "$1" "$2" 2>/dev/null && return 0; sleep 0.1; done
    return 1
}
QPID=""
stop_own() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
        local q
        q=$(cat "target/xtask/$NAME.qemu.pid" 2>/dev/null || true)
        if [ -n "$q" ]; then kill "$q" 2>/dev/null || true; fi
        kill "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    QPID=""
}
trap stop_own EXIT
fail() { echo "RED: $*"; exit 1; }
throttle() { # $1 = bytes per second, 0 = off
    qmp "{\"execute\":\"block_set_io_throttle\",\"arguments\":{\"device\":\"dstl\",\"bps\":$1,\"bps_rd\":0,\"bps_wr\":0,\"iops\":0,\"iops_rd\":0,\"iops_wr\":0}}"
}

cargo xtask image --name "$NAME" --ktest \
    --config-extra "ktest=us.recover.;usb_msd_write=on;loglevel=info"
for attempt in 1 2; do
    rm -f "$SOCK" "$LOG"
    cargo xtask qemu --name "$NAME" --smp 1 --timeout 600 --serial-log "$LOG" -- \
        -drive "if=none,id=dscr,format=raw,file=$RUN_DIR/scr.img" -drive "if=none,id=dgup,format=raw,file=$RUN_DIR/gup.img" \
        -drive "if=none,id=dstl,format=raw,file=$RUN_DIR/stl.img" \
        -device usb-storage,bus=xhci.0,drive=dscr -device usb-storage,bus=xhci.0,drive=dgup \
        -device usb-storage,bus=xhci.0,drive=dstl \
        -qmp "unix:$SOCK,server=on,wait=off" &
    QPID=$!
    if wait_line "$BOOT_LINE" "$LOG" 60; then break; fi
    echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
    stop_own
    [ "$attempt" = 2 ] && fail "$NAME never booted"
done

wait_line "ktest.recover_stall_ready" "$LOG" 400 || fail "no ktest.recover_stall_ready"
before=$(grep -ac "msd.stage_timeout" "$LOG" || true)
# 64 KiB take 16 s at this rate, the data stage has 10 s. Not much slower: the driver's Stop Endpoint makes QEMU cancel
# the request, and QEMU's main loop (and with it the guest's MMIO) waits for that request to finish (scsi_req_cancel).
throttle 4096
for _ in $(seq 1 1200); do
    [ "$(grep -ac "msd.stage_timeout" "$LOG" || true)" -gt "$before" ] && break
    sleep 0.1
done
[ "$(grep -ac "msd.stage_timeout" "$LOG" || true)" -gt "$before" ] || fail "the throttled stick never ran out of time"
throttle 0
wait "$QPID" || true
QPID=""

grep -a "KTEST\|ktest\.recover\|msd\.stage_timeout\|msd\.stage_late\|msd\.port_reset\|msd\.gave_up\|device\.reset\|device\.redetect\|stick\.gone\|port\.given_up" "$LOG" || true
grep -aq "KTEST done passed=.* failed=0" "$LOG" || fail "ktests not green"
for t in boot_stick clean_stop cpu_stop give_up port_reset stall z_slots; do
    grep -aq "KTEST ok us.recover.$t " "$LOG" || fail "us.recover.$t did not pass"
done
grep -aq "device.reset .*warm=false" "$LOG" || fail "no port reset in place"
grep -aq "msd.clean_stop .*ok=true" "$LOG" || fail "no clean stop of the sick stick"
grep -aq "device.reset_for_firmware .*ok=true" "$LOG" || fail "no port reset for the firmware"
grep -aq "msd.gave_up " "$LOG" || fail "the hopeless stick was not given up on"
grep -aq "device.redetect " "$LOG" || fail "no logical disconnect"
if grep -aq "stick.gone" "$LOG"; then
    fail "the boot stick was unmounted"
fi
# The stick's image holds the file whose pages were dirty over the port reset.
grep -aq "BITSOS-RECOVER-DIRTY-PAGES-WRITTEN-AFTER-THE-PORT-RESET" "$IMG" || fail "the dirty pages never reached the stick"
if grep -aq "PANIC\|lock.violation\|xhci.failed\|xhci.abort_stuck\|xhci.cancel_stuck\|msd.run_abandoned" "$LOG"; then
    fail "a failure line in the log"
fi
echo "stage_late=$(grep -ac 'msd.stage_late' "$LOG" || true) (how often the look at the controller saved a stage in cpu_stop; only logged)"
echo "us/55-recover: green"
