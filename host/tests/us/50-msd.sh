#!/bin/bash
# US-4: USB mass storage (Bulk-Only, SCSI, C: BlockDevice) on image files of this run only. One boot with:
#   xb (qemu-xhci, SuperSpeed ports)            xh (qemu-xhci p3=0: USB 2 only)
#   1    big.img  3 TiB sparse, 512 B (READ(16))  1  hs.img  32 MiB, 512 B, scratch range (write test)
#   2    usb-hub, 2.1 fs.img (full speed)         2  k4.img  16 MiB, 4096-byte logical blocks
#   3    usb-bot: LUN 0 lun0.img, LUN 1 lun1.img  3  usb-bot: removable scsi-hd "med" (eject + new medium)
#   4    unp.img (pulled with device_del while us.msd.unplug reads)
# Every image carries a header in block 0 (id, flags, pattern ranges; crates/drivers/usb-msd/src/ktests.rs).
# The script answers the ktests' cues through QMP (eject, new medium, device_del) and afterwards checks the
# checksums the kernel logged against the image files, and that hs.img holds exactly what the write test wrote.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/us-50-msd}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
SOCK="$RUN_DIR/qmp.sock"
LOG="$RUN_DIR/serial.log"
NAME=us-msd
BOOT_LINE="BdsDxe: loading"

# Image maker: path size block_size id flags ranges ("lba:blocks,..."); pattern ranges only (the rest stays
# sparse zeros). Prints nothing.
MAKE='
import struct, sys
path, size, bs, ident, flags = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
ranges = [tuple(int(x) for x in r.split(":")) for r in sys.argv[6].split(",") if r]
T = [(i * 7) & 0xFF for i in range(512)]
ROW = [bytes(a ^ t for t in T) for a in range(256)]
def pattern(off, n):  # off and n multiples of 512
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
# CRC-32 of the first min(size, 64 MiB) bytes of a file (what us.msd.read logs), as 0x%x.
CRC='
import sys, zlib
c = 0
with open(sys.argv[1], "rb") as f:
    left = 64 << 20
    while left:
        b = f.read(min(left, 1 << 20))
        if not b:
            break
        c = zlib.crc32(b, c)
        left -= len(b)
print("0x%x" % c)
'
mk() { python3 -I -c "$MAKE" "$@"; }
crc() { python3 -I -c "$CRC" "$1"; }

TIB=$((1 << 40)); MIB=$((1 << 20))
mk "$RUN_DIR/hs.img"   $((32 * MIB)) 512  1 1 "1:4096,4097:61439"
mk "$RUN_DIR/k4.img"   $((16 * MIB)) 4096 2 0 "1:4095"
mk "$RUN_DIR/big.img"  $((3 * TIB))  512  3 0 "1:2048,4294967040:512,6442448896:2048"
mk "$RUN_DIR/fs.img"   $((8 * MIB))  512  4 0 "1:16383"
mk "$RUN_DIR/lun0.img" $((4 * MIB))  512  5 0 "1:8191"
mk "$RUN_DIR/lun1.img" $((4 * MIB))  512  6 0 "1:8191"
mk "$RUN_DIR/med1.img" $((4 * MIB))  512  7 4 "1:8191"
mk "$RUN_DIR/med2.img" $((6 * MIB))  512  8 4 "1:12287"
mk "$RUN_DIR/unp.img"  $((64 * MIB)) 512  9 2 "1:131071"
declare -A WANT
for i in hs:1 k4:2 big:3 fs:4 lun0:5 lun1:6 med2:8 unp:9; do WANT[${i#*:}]=$(crc "$RUN_DIR/${i%%:*}.img"); done
# hs.img as the write test leaves it: the scratch range (LBA 1, 2 MiB) XOR A5.
python3 -I -c '
import sys
d = bytearray(open(sys.argv[1], "rb").read())
for i in range(512, 512 + (2 << 20)):
    d[i] ^= 0xA5
open(sys.argv[2], "wb").write(d)
' "$RUN_DIR/hs.img" "$RUN_DIR/hs.want"
HS_WRITTEN=$(crc "$RUN_DIR/hs.want")

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

drv() { echo "-drive if=none,id=$1,format=raw,file=$RUN_DIR/$2"; }
cargo xtask image --name "$NAME" --ktest \
    --config-extra "ktest=us.msd;usb_msd_write=on;usb_msd_unplug_test=on;usb_msd_medium_test=on;loglevel=info"
for attempt in 1 2; do
    rm -f "$SOCK" "$LOG"
    # shellcheck disable=SC2046
    cargo xtask qemu --name "$NAME" --timeout 420 --serial-log "$LOG" -- \
        $(drv dhs hs.img) $(drv dk4 k4.img) $(drv dbig big.img) $(drv dfs fs.img) $(drv dl0 lun0.img) $(drv dl1 lun1.img) \
        $(drv dmed med1.img) $(drv dunp unp.img) \
        -device qemu-xhci,id=xb -device qemu-xhci,id=xh,p3=0 \
        -device usb-storage,bus=xh.0,port=1,drive=dhs \
        -device usb-storage,bus=xh.0,port=2,drive=dk4,logical_block_size=4096,physical_block_size=4096 \
        -device usb-storage,bus=xb.0,port=1,drive=dbig \
        -device usb-hub,bus=xb.0,port=2 -device usb-storage,bus=xb.0,port=2.1,drive=dfs \
        -device usb-bot,bus=xb.0,port=3,id=bot \
        -device scsi-hd,bus=bot.0,scsi-id=0,lun=0,drive=dl0 -device scsi-hd,bus=bot.0,scsi-id=0,lun=1,drive=dl1 \
        -device usb-bot,bus=xh.0,port=3,id=botm -device scsi-hd,bus=botm.0,scsi-id=0,lun=0,drive=dmed,id=med,removable=on \
        -device usb-storage,bus=xb.0,port=4,drive=dunp,id=unp \
        -qmp "unix:$SOCK,server=on,wait=off" &
    QPID=$!
    if wait_line "$BOOT_LINE" "$LOG" 60; then break; fi
    echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
    stop_own
    [ "$attempt" = 2 ] && fail "$NAME never booted"
done

# us.msd.medium: eject, then a new (bigger) medium.
wait_line "ktest.msd_eject_ready" "$LOG" 180 || fail "no ktest.msd_eject_ready"
qmp '{"execute":"eject","arguments":{"id":"med","force":true}}'
wait_line "ktest.msd_ejected" "$LOG" 90 || fail "the kernel did not see the medium go"
qmp "{\"execute\":\"blockdev-change-medium\",\"arguments\":{\"id\":\"med\",\"filename\":\"$RUN_DIR/med2.img\",\"format\":\"raw\"}}"
# us.msd.unplug: pull the stick while it reads.
wait_line "ktest.msd_unplug_ready" "$LOG" 300 || fail "no ktest.msd_unplug_ready"
sleep 0.5
qmp '{"execute":"device_del","arguments":{"id":"unp"}}'
wait "$QPID" || true
QPID=""

grep -a "KTEST\|ktest\.msd\|msd\.unit_ready\|msd\.reset\|msd\.transport_error\|msd\.stage_timeout\|msd\.medium\|msd\.detached" "$LOG" || true
grep -aq "KTEST done passed=.* failed=0" "$LOG" || fail "ktests not green"
for t in read write sense reset luns unplug medium; do
    grep -aq "KTEST ok us.msd.$t " "$LOG" || fail "us.msd.$t did not pass"
done
for id in 1 2 3 4 5 6 8 9; do
    line=$(grep -a "ktest.msd_read .* id=$id " "$LOG" | head -1)
    [ -n "$line" ] || fail "image $id was not read"
    got=$(sed -n 's/.* crc=\(0x[0-9a-f]*\).*/\1/p' <<< "$line")
    if [ "$got" != "${WANT[$id]}" ] && ! { [ "$id" = 1 ] && [ "$got" = "$HS_WRITTEN" ]; }; then
        fail "image $id: kernel crc $got, file ${WANT[$id]}"
    fi
done
grep -aq "ktest.msd_read .* id=4 .*speed=full" "$LOG" || fail "the stick behind the hub is not full speed"
grep -aq "ktest.msd_read .* id=1 .*speed=high" "$LOG" || fail "hs.img is not high speed"
grep -aq "ktest.msd_read .* id=2 .*block_size=4096" "$LOG" || fail "k4.img is not 4096-byte blocks"
grep -aq "ktest.msd_read .* id=3 .*blocks=6442450944 " "$LOG" || fail "big.img size (READ CAPACITY(16))"
grep -aq "msd.unit_ready .*blocks=6442450944 .*rw16=true" "$LOG" || fail "big.img without READ/WRITE(16)"
grep -aq "ktest.msd_written .*blocks=4096" "$LOG" || fail "no write"
grep -aq "ktest.msd_unplugged .*error=Gone" "$LOG" || fail "the unplugged read did not end with Gone"
grep -aq "msd.reset " "$LOG" || fail "no reset recovery"
cmp -s "$RUN_DIR/hs.img" "$RUN_DIR/hs.want" || fail "hs.img does not hold exactly what the write test wrote"
for i in k4:2 fs:4 lun0:5 lun1:6 med2:8 unp:9; do
    [ "$(crc "$RUN_DIR/${i%%:*}.img")" = "${WANT[${i#*:}]}" ] || fail "${i%%:*}.img changed (no write may reach it)"
done
if grep -aq "PANIC\|lock.violation\|xhci.failed\|xhci.cancel_stuck\|xhci.abort_stuck\|msd.run_abandoned" "$LOG"; then
    fail "a failure line in the log"
fi
echo "us-50-msd: green"
