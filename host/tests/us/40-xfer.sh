#!/bin/bash
# US-3: bulk and interrupt transfers on a second qemu-xhci (QEMU bus port n = xHCI port n for USB 3, n + 4 for
# USB 2):
#   1    usb-kbd (high speed)          2  usb-tablet (high speed)
#   3    usb-hub, 3.1 usb-storage (full speed: bulk packets of 64 bytes, behind the hub)
#   4    usb-storage (SuperSpeed, bulk bursts)
# The us.xfer ktests read every stick (1 MiB as one TD cut at 64 KiB, the same in 120 KiB reads, K-2 chains queued
# from completion callbacks), stall and clear the IN endpoints, time out on the idle keyboard (stop and skip) and
# run interrupt pipes with idle reports; for us.xfer.hid_input this script presses "a" and moves the tablet through
# QMP once the kernel says `ktest.hid_ready`. No PS/2 (i8042=off), so QEMU sends keys to the USB keyboard.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/us-40-xfer}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
SOCK="$RUN_DIR/qmp.sock"
LOG="$RUN_DIR/serial.log"
NAME=us-xfer
BOOT_LINE="BdsDxe: loading"

# Two 16 MiB sticks with a pattern and a boot signature in LBA 0 (the ktest checks 55 AA).
for d in fs ss; do
    python3 -I -c '
import sys
n = 16 << 20
b = bytearray((bytes(range(251)) * (n // 251 + 1))[:n])
b[510], b[511] = 0x55, 0xAA
open(sys.argv[1], "wb").write(b)
' "$RUN_DIR/$d.img"
done

qmp() { # $1 = JSON command
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
wait_line() { # $1 = pattern, $2 = log, $3 = seconds
    local n=$(( $3 * 10 ))
    for _ in $(seq 1 "$n"); do grep -aq "$1" "$2" 2>/dev/null && return 0; sleep 0.1; done
    return 1
}
QPID=""
# Only our own processes: the QEMU this xtask started (its pid file) and the xtask itself.
stop_own() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
        local q; q=$(cat "target/xtask/qemu-$NAME.pid" 2>/dev/null || true)
        if [ -n "$q" ]; then kill "$q" 2>/dev/null || true; fi
        kill "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    QPID=""
}
trap stop_own EXIT
fail() { echo "RED: $*"; exit 1; }

# usb_msd_skip=on: the US-4 mass storage driver leaves the sticks to these raw transfer tests (US-4's own tests are
# in 50-msd.sh).
cargo xtask image --name "$NAME" --ktest --config-extra "ktest=us.xfer,us.enum.boot;usb_xfer_test=on;usb_msd_skip=on;loglevel=info"
for attempt in 1 2; do
    rm -f "$SOCK" "$LOG"
    cargo xtask qemu --name "$NAME" --timeout 240 --serial-log "$LOG" -- \
        -machine i8042=off \
        -drive "if=none,id=dfs,format=raw,file=$RUN_DIR/fs.img" -drive "if=none,id=dss,format=raw,file=$RUN_DIR/ss.img" \
        -device qemu-xhci,id=xb \
        -device usb-kbd,bus=xb.0,port=1 -device usb-tablet,bus=xb.0,port=2 \
        -device usb-hub,bus=xb.0,port=3 -device usb-storage,bus=xb.0,port=3.1,drive=dfs \
        -device usb-storage,bus=xb.0,port=4,drive=dss \
        -qmp "unix:$SOCK,server=on,wait=off" &
    QPID=$!
    if wait_line "$BOOT_LINE" "$LOG" 60; then break; fi
    echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
    stop_own
    [ "$attempt" = 2 ] && fail "$NAME never booted"
done
wait_line "ktest.hid_ready" "$LOG" 180 || fail "no ktest.hid_ready"
qmp '{"execute":"input-send-event","arguments":{"events":[{"type":"key","data":{"down":true,"key":{"type":"qcode","data":"a"}}}]}}'
qmp '{"execute":"input-send-event","arguments":{"events":[{"type":"abs","data":{"axis":"x","value":20000}},{"type":"abs","data":{"axis":"y","value":12000}}]}}'
sleep 0.5
qmp '{"execute":"input-send-event","arguments":{"events":[{"type":"key","data":{"down":false,"key":{"type":"qcode","data":"a"}}}]}}'
wait "$QPID" || true
QPID=""

grep -a "KTEST\|ktest\.\|xhci.transfer_timeout\|xhci.ep_reset\|device\.attached" "$LOG" || true
grep -aq "KTEST done passed=.* failed=0" "$LOG" || fail "ktests not green"
for t in stick_read chain stall timeout hid_pipes hid_input; do
    grep -aq "KTEST ok us.xfer.$t " "$LOG" || fail "us.xfer.$t did not pass"
done
n=$(grep -ac "ktest.stick_read device=" "$LOG" || true)
[ "$n" -eq 3 ] || fail "$n sticks read (want 3: boot stick, full speed behind the hub, SuperSpeed)"
grep -aq "ktest.stick_read .*speed=full" "$LOG" || fail "the full speed stick was not read"
grep -aq "ktest.stick_read .*speed=super" "$LOG" || fail "the SuperSpeed stick was not read"
[ "$(grep -ac "ktest.hid_pipe device=" "$LOG" || true)" -eq 2 ] || fail "keyboard and tablet pipes"
grep -aq "xhci.transfer_timeout .*how=skipped" "$LOG" || fail "the timeout was not stopped and skipped"
if grep -aq "xhci.failed\|xhci.slot_leaked\|xhci.cancel_stuck\|xhci.abort_stuck\|xhci.td_skipped\|lock.violation\|PANIC" "$LOG"; then
    fail "a failure line in the log"
fi
echo "us-40-xfer: green"
