#!/bin/bash
# FS-6: the block layer on QEMU usb-storage disks made from image files of this run only. One boot (ktest=block.,
# usb_msd_write=on) with an extra qemu-xhci "xf":
#   port 1  scratch.img  8 MiB, 512 B, header SCRATCH (LBA 2048, 4096 blocks): block.usb.write writes it
#   port 2  eio.img      8 MiB, 512 B, header EIO, behind QEMU blkdebug: reads of sector 1000 and writes of
#                        sector 3000 fail with EIO (block.usb.eio)
#   port 3  k4.img       4 MiB, 4096-byte logical blocks, header only (block.usb.read)
#   port 4  win.img      16 MiB, GPT: EFI System (LBA 2048-4095) + Microsoft basic data (4096-30000): every
#                        writing request is refused even with usb_msd_write=on (block.usb.windows)
# The boot stick (xtask image) is recognised as the stick and refuses writes (stick_write off).
# Afterwards: scratch.img holds exactly the write test's bytes, every other image (and the boot image) is
# byte for byte what it was.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/fs-20-block}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LOG="$RUN_DIR/serial.log"
NAME=fs-block
BOOT_LINE="BdsDxe: loading"

# Image maker: path size block_size id flags a b. Block 0 = header, the rest the pattern
# byte(off) = ((off >> 9) * 31 + (off & 511) * 7) & 0xFF (crates/block/src/ktests.rs `spattern`).
MAKE='
import struct, sys
path, size, bs, ident, flags, a, b = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]), int(sys.argv[6]), int(sys.argv[7])
T = bytes((i * 7) & 0xFF for i in range(512))
def sector(s):
    k = (s * 31) & 0xFF
    return bytes((k + t) & 0xFF for t in T)
with open(path, "wb") as f:
    hdr = b"BITSOS-FS6-BLOCK-V1\0\0\0\0\0" + struct.pack("<IIQQ", ident, flags, a, b)
    f.write(hdr + bytes(bs - len(hdr)))
    for s in range(bs // 512, size // 512):
        f.write(sector(s))
'
# GPT image: path size; EFI System 2048-4095, Microsoft basic data 4096-30000 (zlib.crc32 = UEFI's CRC32).
GPT='
import struct, sys, uuid, zlib
path, size = sys.argv[1], int(sys.argv[2])
bs, blocks = 512, size // 512
img = bytearray(size)
img[0x1BE + 4] = 0xEE
struct.pack_into("<II", img, 0x1BE + 8, 1, min(blocks - 1, 0xFFFFFFFF))
img[510:512] = b"\x55\xaa"
arr = bytearray(128 * 128)
parts = [("C12A7328-F81F-11D2-BA4B-00A0C93EC93B", 2048, 4095, "EFI system partition"),
         ("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7", 4096, 30000, "Basic data partition")]
for i, (t, first, last, name) in enumerate(parts):
    e = uuid.UUID(t).bytes_le + uuid.UUID(int=i + 1).bytes_le + struct.pack("<QQQ", first, last, 0) + name.encode("utf-16-le")
    arr[i * 128:i * 128 + len(e)] = e
crc = zlib.crc32(bytes(arr))
eb = len(arr) // bs
last = blocks - 1
for my, alt, ent in ((1, last, 2), (last, 1, last - eb)):
    h = bytearray(struct.pack("<8sIIIIQQQQ16sQIII", b"EFI PART", 0x10000, 92, 0, 0, my, alt, 2 + eb, last - eb - 1,
                              uuid.UUID(int=99).bytes_le, ent, 128, 128, crc))
    struct.pack_into("<I", h, 16, zlib.crc32(bytes(h)))
    img[my * bs:my * bs + 92] = h
    img[ent * bs:ent * bs + len(arr)] = arr
open(path, "wb").write(img)
'
SUM='
import hashlib, sys
print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())
'
mk() { python3 -I -c "$MAKE" "$@"; }
sum() { python3 -I -c "$SUM" "$1"; }

MIB=$((1 << 20))
mk "$RUN_DIR/scratch.img" $((8 * MIB)) 512  1 1 2048 4096
mk "$RUN_DIR/eio.img"     $((8 * MIB)) 512  2 2 1000 3000
mk "$RUN_DIR/k4.img"      $((4 * MIB)) 4096 3 0 0 0
python3 -I -c "$GPT" "$RUN_DIR/win.img" $((16 * MIB))
printf '[inject-error]\nevent = "read_aio"\nerrno = "5"\nsector = "1000"\n\n[inject-error]\nevent = "write_aio"\nerrno = "5"\nsector = "3000"\n' > "$RUN_DIR/eio.conf"
# scratch.img as the write test leaves it: LBA 2048.. (4096 blocks) XOR A5.
python3 -I -c '
import sys
d = bytearray(open(sys.argv[1], "rb").read())
for i in range(2048 * 512, (2048 + 4096) * 512):
    d[i] ^= 0xA5
open(sys.argv[2], "wb").write(d)
' "$RUN_DIR/scratch.img" "$RUN_DIR/scratch.want"
declare -A WANT
for i in eio k4 win; do WANT[$i]=$(sum "$RUN_DIR/$i.img"); done

QPID=""
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

cargo xtask image --name "$NAME" --ktest --config-extra "ktest=block.;usb_msd_write=on;loglevel=info"
BOOT_SUM=$(sum "target/xtask/$NAME.img")
for attempt in 1 2; do
    rm -f "$LOG"
    cargo xtask qemu --name "$NAME" --timeout 300 --serial-log "$LOG" -- \
        -drive "if=none,id=dscr,format=raw,file=$RUN_DIR/scratch.img" \
        -drive "if=none,id=deio,format=raw,file.driver=blkdebug,file.config=$RUN_DIR/eio.conf,file.image.filename=$RUN_DIR/eio.img" \
        -drive "if=none,id=dk4,format=raw,file=$RUN_DIR/k4.img" \
        -drive "if=none,id=dwin,format=raw,file=$RUN_DIR/win.img" \
        -device qemu-xhci,id=xf \
        -device usb-storage,bus=xf.0,port=1,drive=dscr \
        -device usb-storage,bus=xf.0,port=2,drive=deio \
        -device usb-storage,bus=xf.0,port=3,drive=dk4,logical_block_size=4096,physical_block_size=4096 \
        -device usb-storage,bus=xf.0,port=4,drive=dwin &
    QPID=$!
    for _ in $(seq 1 600); do grep -aq "$BOOT_LINE" "$LOG" 2>/dev/null && break; sleep 0.1; done
    if grep -aq "$BOOT_LINE" "$LOG" 2>/dev/null; then break; fi
    echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
    stop_own
    [ "$attempt" = 2 ] && fail "$NAME never booted"
done
wait "$QPID" || true
QPID=""

grep -a "KTEST\|block ktest\.\|block disk\.registered\|block disk\.table\|block write\.refused\|block req\.failed\|block configured" "$LOG" || true
grep -aq "KTEST done passed=.* failed=0" "$LOG" || fail "ktests not green"
for t in ram.queue ram.aging ram.timeout ram.eio ram.refusal bdev.pagecache usb.stick usb.stick_write usb.read usb.write usb.eio usb.windows; do
    grep -aq "KTEST ok block.$t " "$LOG" || fail "block.$t did not pass"
done
grep -aq "block ktest.usb_written .*blocks=4096" "$LOG" || fail "no write on the scratch disk"
grep -aq "block ktest.usb_eio .*read_bad=1000 write_bad=3000" "$LOG" || fail "no EIO check"
grep -aq "block ktest.usb_windows .*usb_msd_write=true" "$LOG" || fail "no Windows refusal with usb_msd_write=on"
grep -aq "block disk.registered disk=msd[0-9]* kind=usb .*block_size=4096" "$LOG" || fail "the 4 KiB disk was not registered"
cmp -s "$RUN_DIR/scratch.img" "$RUN_DIR/scratch.want" || fail "scratch.img does not hold exactly what the write test wrote"
for i in eio k4 win; do
    [ "$(sum "$RUN_DIR/$i.img")" = "${WANT[$i]}" ] || fail "$i.img changed (no write may reach it)"
done
[ "$(sum "target/xtask/$NAME.img")" = "$BOOT_SUM" ] || fail "the boot image changed"
if grep -aq "PANIC\|lock.violation\|msd.run_abandoned" "$LOG"; then
    fail "a failure line in the log"
fi
echo "fs-20-block: green"
