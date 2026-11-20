#!/bin/bash
# slow
# time-limit: 2400
# CRASHDISK: the crash record on the boot ESP survives a cold reset. A QEMU NVMe image laid out like the Lexar
# (host/tests/nv/43-boot-nvme.sh: MSR, Windows, the ESP nvme_boot= names, the bitsvol window) boots bitsboot from its
# ESP; each part panics in a first QEMU, and a second QEMU process on the same image (RAM gone: a cold start) must
# read the record back whole. Never a real disk.
#  A. panic: CONFIG keys allow /boot on the ESP; boot 1 arms the disk copy and panics on purpose (panic=halt: QEMU
#     ends); \BITSOS2\CRASH\SLOTS.BIN holds record 1 in slot 0, checksum right. Boot 2 (cold): rb.crashdisk.forced
#     finds it from the disk with backtrace, every CPU's registers, memory line, log; crash_info lists it;
#     /proc/bitsos/crash has the desktop's line; the next panic would go to slot 1 as number 2.
#  B. hang: the fake watchdog armed, CPU 1 spins with interrupts off; the reporter turns the stall into a panic 3 s
#     before the reset; boot 2 finds a record marked as a hang.
#  C. off: no nvme_boot= line: /boot read-only, the disk copy off ("RAM only"), the panic writes nothing: the ESP's
#     bytes are the same before and after.
#  After A and B: every byte outside the ESP and the window unchanged, fsck.fat -n of the ESP clean, the ESP's boot
#  sector and \EFI\ unchanged.
# Ports: none (no network).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/rb-45-crashdisk}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
NV="$(pwd)/host/tests/nv"
RB="$(pwd)/host/tests/rb"
NAME=rb-crashdisk
SERIAL=LEXARQEMUCD5678
MKIMG() { python3 -I "$NV/mkimg.py" "$@"; }
sum() { sha256sum "$1" | cut -d' ' -f1; }
red() { echo "RED: $1"; [ -n "${2:-}" ] && grep -a "KTEST\|crash \|PANIC\|fat stick\|watchdog" "$2" | tail -40 | cut -c1-240; exit 1; }

QPID=""
stop_own() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
        local q; q=$(head -1 "target/xtask/qemu-$NAME.pid" 2>/dev/null || true)
        if [ -n "$q" ] && tr '\0' ' ' < "/proc/$q/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then
            kill "$q" 2>/dev/null || true
        fi
        kill "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    QPID=""
}
trap stop_own EXIT

# ---- the layout (as nv/43): 1 MSR, 2 Windows, 3 the ESP, 4 the window, a free rest
MSR=(2048 34815)
WIN=(34816 559103)
ESP_FIRST=559104
cargo xtask image --name "$NAME" --ktest --config-extra "loglevel=info" > "$RUN_DIR/image0.out" 2>&1 || { tail -20 "$RUN_DIR/image0.out"; red "image"; }
CONTENT=$(du -sb "target/xtask/esp-$NAME" | cut -f1)
ESP_N=$(( (CONTENT * 3 + (256 << 20)) / 512 / 2048 * 2048 ))
DATA_FIRST=$((ESP_FIRST + ESP_N))
DATA_N=1048576
BLOCKS=$((DATA_FIRST + DATA_N + 4096))
IMG="$RUN_DIR/lexar.img"
DKEY="$SERIAL:$(MKIMG guid 4):$DATA_FIRST:$DATA_N"
EKEY="$SERIAL:$(MKIMG guid 3):$ESP_FIRST:$ESP_N"
KEYS="nvme_write=on;nvme_data=$DKEY;nvme_boot=$EKEY"

# fresh_disk: a new image, so each part starts with an ESP without CRASH.
fresh_disk() {
    rm -f "$IMG"
    MKIMG real "$IMG" $BLOCKS 512 9 msr:${MSR[0]}:${MSR[1]} msdata:${WIN[0]}:${WIN[1]}:ntfs efi:$ESP_FIRST:$((DATA_FIRST - 1)) msdata:$DATA_FIRST:$((DATA_FIRST + DATA_N - 1))
    OUT=$(MKIMG outside-ranges "$IMG" 512 $ESP_FIRST:$ESP_N $DATA_FIRST:$DATA_N)
}
# image CONFIG: the ESP made from the image's files plus the symbol table (FAT32, label BITSOS), into partition 3.
image() {
    cargo xtask image --name "$NAME" --ktest --config-extra "ktest_run=rb/45-crashdisk;loglevel=info;panic=halt;$1" > "$RUN_DIR/image.out" 2>&1 || { tail -20 "$RUN_DIR/image.out"; red "image"; }
    "$RB/ksyms.sh" "$NAME" ktest > /dev/null
    mkdir -p "target/xtask/esp-$NAME/BITSOS2/fw"
    cp "target/xtask/$NAME.ksyms" "target/xtask/esp-$NAME/BITSOS2/fw/KSYMS"
    local f="$RUN_DIR/esp.fat"
    rm -f "$f"
    mkfs.fat -F 32 -n BITSOS -h $ESP_FIRST -C "$f" $((ESP_N / 2)) > /dev/null
    mcopy -s -i "$f" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
    dd if="$f" of="$IMG" bs=1M seek=$((ESP_FIRST * 512)) oflag=seek_bytes conv=notrunc,sparse status=none
    dd if="$f" of="$RUN_DIR/esp.head" bs=512 count=32 status=none
    rm -f "$f"
}
esp_out() { # the ESP as a file
    dd if="$IMG" of="$1" bs=1M skip=$((ESP_FIRST * 512)) count=$((ESP_N * 512)) iflag=skip_bytes,count_bytes conv=sparse status=none
}
checks() {
    local what="$1" f="$RUN_DIR/esp.now"
    [ "$(MKIMG outside-ranges "$IMG" 512 $ESP_FIRST:$ESP_N $DATA_FIRST:$DATA_N)" = "$OUT" ] || red "$what: a byte outside the ESP and the window changed"
    esp_out "$f"
    fsck.fat -n "$f" > "$RUN_DIR/fsck-$what.log" 2>&1 || { cat "$RUN_DIR/fsck-$what.log"; red "$what: fsck.fat -n is not clean"; }
    python3 -I - "$RUN_DIR/esp.head" "$f" <<'PY' || red "$what: the ESP's boot sector or its backup changed"
import sys
a, b = open(sys.argv[1], "rb").read(), open(sys.argv[2], "rb").read(32 * 512)
assert a[:512] == b[:512] and a[1024:] == b[1024:], "reserved sectors differ"
PY
    mtype -i "$f" ::/EFI/BOOT/BOOTX64.EFI | cmp -s - "target/xtask/esp-$NAME/EFI/BOOT/BOOTX64.EFI" || red "$what: \\EFI\\BOOT\\BOOTX64.EFI changed"
    echo "ok: $what: outside unchanged, fsck clean, boot sector and loader unchanged"
}
slots() { # the area's slots as crashdisk_check.py reads them
    local f="$RUN_DIR/esp.now" s="$RUN_DIR/slots.bin"
    esp_out "$f"
    rm -f "$s"
    mtype -i "$f" ::/BITSOS2/CRASH/SLOTS.BIN > "$s" 2>/dev/null || { echo "no SLOTS.BIN"; return 0; }
    python3 -I "$RB/crashdisk_check.py" "$s"
}
# boot LOG: one QEMU process on the image (bitsboot from the NVMe's ESP, no stick).
boot() {
    local log="$1"
    rm -f "$log"
    cargo xtask qemu --name "$NAME" --boot nvme --smp 4 --mem 4096 --timeout "${RB_TIMEOUT:-600}" --serial-log "$log" -- \
        -drive "if=none,id=nv0,format=raw,file=$IMG,discard=unmap" -device "nvme,id=nvq0,serial=$SERIAL,drive=nv0,bootindex=0" \
        > "$log.qemu" 2>&1 &
    QPID=$!
    wait "$QPID" && RC=0 || RC=$?
    QPID=""
}
# boot_nvme LOG: as boot, again (at most twice more) when the firmware started bitsboot from the stick: under the
# host's load xtask's stall check may end OVMF's slow first read of the NVMe and its retry then picks the stick
# (seen 13.11.: no `BdsDxe` line, `stick: device path ...USB`). That is the firmware's choice, not the item's.
boot_nvme() {
    local n
    for n in 1 2 3; do
        boot "$1"
        grep -aq "^stick: device path .*NVMe(" "$1" && return 0
        echo "note: attempt $n of $(basename "$1") came from the stick, not the NVMe: once more"
        mv "$1" "$1.stick-$n"
    done
    red "$(basename "$1"): bitsboot never came from the NVMe"
}
first_boot() { # LOG: the panic boot
    boot_nvme "$1"
    grep -aq "KTEST PANIC" "$1" || red "$2: boot 1 did not panic" "$1"
    grep -aq "E crash disk.written slot=0 seq=1 " "$1" || red "$2: boot 1 wrote no disk record" "$1"
    if grep -aq "lock.violation\|memory allocation of .* failed" "$1"; then red "$2: a lock violation or an allocation in the panic path" "$1"; fi
}
second_boot() { # LOG: the cold boot after it
    boot_nvme "$1"
    grep -aq "KTEST done passed=.* failed=0" "$1" || red "$2: boot 2 ktests not green" "$1"
    grep -aq "KTEST ok rb.crashdisk.forced " "$1" || red "$2: rb.crashdisk.forced did not pass" "$1"
    grep -aq "I crash page .*marker=absent" "$1" || red "$2: boot 2 is not a cold start (the RAM marker survived)" "$1"
    grep -aq "E crash disk.previous slot=0 seq=1 " "$1" || red "$2: no journal event of the disk record" "$1"
    grep -aq "I crash disk.armed .*slot=1 seq=2 " "$1" || red "$2: boot 2 did not arm slot 1 as number 2" "$1"
}

# ---- A. a panic, then a cold start
fresh_disk
image "$KEYS;ktest=rb.crashdisk.format,rb.crashdisk.extents,rb.crashdisk.forced;rb_crashdisk=panic"
first_boot "$RUN_DIR/a1.log" A
slots | tee "$RUN_DIR/a.slots"
grep -q "^slot 0 record seq=1 .* sum=ok first=panic on cpu .*rb.crash forced kind=panic" "$RUN_DIR/a.slots" || red "A: slot 0 does not hold the record whole"
[ "$(grep -c " empty$" "$RUN_DIR/a.slots")" = 3 ] || red "A: slots 1..3 are not prepared and empty"
checks A1
second_boot "$RUN_DIR/a2.log" A
# Boot 1 may panic before they run (the ktests' order is not the filter's); boot 2 runs all three.
grep -aq "KTEST ok rb.crashdisk.format " "$RUN_DIR/a2.log" && grep -aq "KTEST ok rb.crashdisk.extents " "$RUN_DIR/a2.log" || red "A: the format and extent ktests did not pass" "$RUN_DIR/a2.log"
checks A2
grep -aq "fat stick.decided .*nvme=true rw=true" "$RUN_DIR/a2.log" || red "A: /boot on the NVMe not writable in boot 2" "$RUN_DIR/a2.log"
echo "ok: A: panic record written to slot 0, read back whole after a cold start"

# ---- B. a hang the watchdog ends
fresh_disk
image "$KEYS;ktest=rb.crashdisk.forced,rb.crashdisk.hang;rb_crashdisk=hang;watchdog_timeout=12;watchdog_alive=4"
first_boot "$RUN_DIR/b1.log" B
grep -aq "E watchdog stall " "$RUN_DIR/b1.log" || red "B: no stall was seen" "$RUN_DIR/b1.log"
grep -aq "E watchdog hang.panic .*Grace" "$RUN_DIR/b1.log" || red "B: the chip was not put on the panic's grace before the hang's panic" "$RUN_DIR/b1.log"
grep -aq "E kernel panic | panicked at .*watchdog: CPU \[1\] stalled" "$RUN_DIR/b1.log" || red "B: the stall did not become the watchdog's panic" "$RUN_DIR/b1.log"
slots | tee "$RUN_DIR/b.slots"
grep -q "^slot 0 record seq=1 .*flags=[0-9]* sum=ok first=panic on cpu .*watchdog: CPU \[1\] stalled" "$RUN_DIR/b.slots" || red "B: slot 0 does not hold the hang's record"
checks B1
second_boot "$RUN_DIR/b2.log" B
grep -aq "KTEST ok rb.crashdisk.hang " "$RUN_DIR/b2.log" || red "B: rb.crashdisk.hang did not pass in boot 2" "$RUN_DIR/b2.log"
grep -aq "E crash disk.previous .*hang=true" "$RUN_DIR/b2.log" || red "B: the journal event does not say hang" "$RUN_DIR/b2.log"
checks B2
echo "ok: B: the hang became a panic, its record read back after a cold start"

# ---- C. no nvme_boot=: nothing written
fresh_disk
image "nvme_write=on;nvme_data=$DKEY;ktest=rb.crashdisk.forced;rb_crashdisk=off"
esp_out "$RUN_DIR/esp.before"
ESP_SUM=$(sum "$RUN_DIR/esp.before")
boot_nvme "$RUN_DIR/c1.log"
grep -aq "KTEST PANIC" "$RUN_DIR/c1.log" || red "C: boot 1 did not panic" "$RUN_DIR/c1.log"
grep -aq "I crash disk.off why=.*nvme_boot=" "$RUN_DIR/c1.log" || red "C: the disk copy was not off for nvme_boot=" "$RUN_DIR/c1.log"
grep -aq "E crash disk.not_written .*not armed" "$RUN_DIR/c1.log" || red "C: the panic did not say why there is no disk copy" "$RUN_DIR/c1.log"
if grep -aq "crash disk.written" "$RUN_DIR/c1.log"; then red "C: a disk record was written" "$RUN_DIR/c1.log"; fi
esp_out "$RUN_DIR/esp.after"
[ "$(sum "$RUN_DIR/esp.after")" = "$ESP_SUM" ] || red "C: the ESP changed although nvme_boot= is missing"
[ "$(MKIMG outside-ranges "$IMG" 512 $ESP_FIRST:$ESP_N $DATA_FIRST:$DATA_N)" = "$OUT" ] || red "C: a byte outside the ESP and the window changed"
echo "ok: C: without nvme_boot= the ESP stayed byte for byte as it was"
rm -f "$RUN_DIR"/esp.* "$RUN_DIR/slots.bin"
echo "GREEN: the crash record survives a cold start on the boot ESP (panic and hang), and nothing is written without nvme_boot="
