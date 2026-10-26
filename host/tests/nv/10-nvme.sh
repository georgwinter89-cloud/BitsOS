#!/bin/bash
# NV-1: the read-only NVMe driver on QEMU `-device nvme` controllers backed by image files of this run only.
# Two boots (ktest=nv.; the second with nvme_irq=off, polled) with two controllers:
#   NVQ0  ns1 16 MiB, 512 B, pattern seed 1, behind blkdebug: reads of sector 20000 fail (nv.errors)
#         ns2  8 MiB, 4 KiB blocks, pattern seed 2
#         ns3 32 MiB, 512 B, fake Windows GPT: EFI System 2048-4095, MS reserved 4096-8191, MS basic data
#             8192-65000 (pattern seed 3 outside the table)
#   NVQ1  ns1 16 MiB, 4 KiB blocks, GPT with two Linux partitions 256-1023 and 1024-3800 (seed 4)
# Afterwards every image (and the boot image) is byte for byte what it was (sha256): the driver has no write
# command, and the log shows media_writes=0 for both controllers.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/nv-10-nvme}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/nv"
NAME=nv-nvme
BOOT_LINE="BdsDxe: loading"
SMP="${NV_SMP:-4}"
sum() { sha256sum "$1" | cut -d' ' -f1; }
fail() { echo "RED: $*"; exit 1; }

python3 -I "$HERE/mkimg.py" pattern "$RUN_DIR/q0n1.img" $((16 << 20)) 1
python3 -I "$HERE/mkimg.py" pattern "$RUN_DIR/q0n2.img" $((8 << 20)) 2
python3 -I "$HERE/mkimg.py" gpt "$RUN_DIR/win.img" 65536 512 3 efi:2048:4095 msr:4096:8191 msdata:8192:65000
python3 -I "$HERE/mkimg.py" gpt "$RUN_DIR/linux4k.img" 4096 4096 4 linux:256:1023 linux:1024:3800
cat > "$RUN_DIR/eio.conf" <<CONF
[inject-error]
event = "read_aio"
errno = "5"
sector = "20000"
CONF
declare -A WANT
for i in q0n1 q0n2 win linux4k; do WANT[$i]=$(sum "$RUN_DIR/$i.img"); done

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

# boot NAME CONFIG_EXTRA: builds the image and boots it once with both controllers; the log is $RUN_DIR/NAME.log.
boot() {
    local name="$1" log="$RUN_DIR/$1.log"
    cargo xtask image --name "$name" --ktest --config-extra "$2"
    for attempt in 1 2; do
        rm -f "$log"
        NAME="$name"
        cargo xtask qemu --name "$name" --smp "$SMP" --timeout 300 --serial-log "$log" -- \
            -drive "if=none,id=q0n1,format=raw,file.driver=blkdebug,file.config=$RUN_DIR/eio.conf,file.image.filename=$RUN_DIR/q0n1.img" \
            -drive "if=none,id=q0n2,format=raw,file=$RUN_DIR/q0n2.img" \
            -drive "if=none,id=q0n3,format=raw,file=$RUN_DIR/win.img" \
            -drive "if=none,id=q1n1,format=raw,file=$RUN_DIR/linux4k.img" \
            -device nvme,id=nvq0,serial=NVQ0 \
            -device nvme-ns,drive=q0n1,bus=nvq0,nsid=1 \
            -device nvme-ns,drive=q0n2,bus=nvq0,nsid=2,logical_block_size=4096,physical_block_size=4096 \
            -device nvme-ns,drive=q0n3,bus=nvq0,nsid=3 \
            -device nvme,id=nvq1,serial=NVQ1,drive=q1n1,logical_block_size=4096,physical_block_size=4096 &
        QPID=$!
        for _ in $(seq 1 600); do grep -aq "$BOOT_LINE" "$log" 2>/dev/null && break; sleep 0.1; done
        if grep -aq "$BOOT_LINE" "$log" 2>/dev/null; then break; fi
        echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
        stop_own
        [ "$attempt" = 2 ] && fail "$name never booted"
    done
    wait "$QPID" || true
    QPID=""
}

boot "$NAME" "ktest=nv.;loglevel=info"
LOG="$RUN_DIR/$NAME.log"
BOOT_SUM=$(sum "target/xtask/$NAME.img")
grep -a "KTEST\|nvme \|block disk\.registered\|block disk\.protected\|block write\.refused" "$LOG" || true
grep -aq "KTEST done passed=.* failed=0" "$LOG" || fail "ktests not green"
for t in cmd.no_write_opcodes ident read zero_writes smart flush irq tool qemu.pattern gpt errors reset; do
    grep -aq "KTEST ok nv.$t " "$LOG" || fail "nv.$t did not pass"
done
# What the tests assert inside the kernel (media_writes=0 on both controllers, the 4 KiB namespace, the Windows
# namespace protected, its partitions and the Linux ones as Stacked disks) is not grepped from klog lines: the
# last ones may still be queued when QEMU exits.
for i in q0n1 q0n2 win linux4k; do
    [ "$(sum "$RUN_DIR/$i.img")" = "${WANT[$i]}" ] || fail "$i.img changed (no write may reach it)"
done
[ "$(sum "target/xtask/$NAME.img")" = "$BOOT_SUM" ] || fail "the boot image changed"
if grep -aq "PANIC\|lock.violation" "$LOG"; then
    fail "a failure line in the log"
fi

# Second boot: CONFIG nvme_irq=off keeps both controllers polled; the same reads pass, nv.irq skips.
boot nv-polled "ktest=nv.;loglevel=info;nvme_irq=off"
PLOG="$RUN_DIR/nv-polled.log"
grep -a "KTEST" "$PLOG" || true
grep -aq "KTEST done passed=.* failed=0" "$PLOG" || fail "polled: ktests not green"
for t in ident read zero_writes qemu.pattern gpt errors reset; do
    grep -aq "KTEST ok nv.$t " "$PLOG" || fail "polled: nv.$t did not pass"
done
grep -aq "KTEST skip nv.irq: every controller is polled" "$PLOG" || fail "polled: nv.irq did not skip (a controller has vectors)"
for i in q0n1 q0n2 win linux4k; do
    [ "$(sum "$RUN_DIR/$i.img")" = "${WANT[$i]}" ] || fail "polled: $i.img changed"
done
echo "GREEN: nv ktests, images unchanged (win.img ${WANT[win]})"
