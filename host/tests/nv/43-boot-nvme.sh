#!/bin/bash
# time-limit: 2400
# slow
# Georg 6.11.2026: BitsOS boots from the Lexar's ESP, the stick only a rescue. A QEMU NVMe image laid out like the Lexar
# (GPT: Microsoft reserved, Windows data with an NTFS boot sector, the ESP CONFIG nvme_boot= names, the bitsvol window
# nvme_data= names, a free rest) boots bitsboot from its ESP (bootindex 0); the USB stick, the same files and STICK.ID,
# stays plugged in. Never a real disk.
#  A. ktests: /boot is that ESP (/dev/nvme0n1p3), writable below \BITSOS2\ only, through its own window at both layers;
#     raw writes to it, to the Windows partitions, the GPT and the whole disk refused; the stick an ordinary disk; no
#     data.img on the NVMe; the window's pool made for B (nv.boot.esp, nv.boot.stick_beside, nv.boot.pool).
#  B. MCP: install.mjs --reboot installs generation 2 onto the ESP, reboots into its one try and sees it confirm itself;
#     /data on the window's pool beside the open boot window.
#  C. nvme_boot= names the ESP with a wrong size: the disk refused as a whole, /boot on the ESP read-only, no /data
#     (nv.boot.refused_key).
#  D. the keys right, the ESP's label not BITSOS: the window and /data on its pool, /boot read-only, its window never
#     opened (nv.boot.refused_label).
#  After each: every byte outside the two windows as it was (GPTs, MSR, Windows, the rest), the stick image unchanged,
#  fsck.fat -n of the ESP clean, its boot sector and backup unchanged, \EFI\ unchanged; after B SYS holds 1 and 2 (OK).
# Ports: NV's range (BITSOS_PORT_LO, default 19000): MCP LO+400, install.mjs's server LO+450..LO+469.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/nv-43-boot-nvme}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
NV="$(pwd)/host/tests/nv"
LO="${BITSOS_PORT_LO:-19000}"
FWD=$((LO + 400))
export BITSOS_INSTALL_PORTS="$((LO + 450))-$((LO + 469))"
export BITSOS_TOKEN="nv-boot-nvme-token-3e9a"
NAME=nv-boot
SERIAL=LEXARQEMUZY1234
MKIMG() { python3 -I "$NV/mkimg.py" "$@"; }
sum() { sha256sum "$1" | cut -d' ' -f1; }
fail() { echo "RED: $*"; exit 1; }

QPID=""
stop_own() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
        local q; q=$(head -1 "target/xtask/qemu-$NAME.pid" 2>/dev/null || true)
        # The QEMU of this run only: its pid file, checked against its -name.
        if [ -n "$q" ] && tr '\0' ' ' < "/proc/$q/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then
            kill "$q" 2>/dev/null || true
        fi
        kill "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    QPID=""
}
trap stop_own EXIT

# ---- the layout: 1 MSR 16 MiB, 2 Windows 256 MiB, 3 the ESP, 4 the window 512 MiB, a free rest
MSR=(2048 34815)
WIN=(34816 559103)
ESP_FIRST=559104
# The ESP's size from the first image (two generations and room); the window right behind it.
cargo xtask image --name "$NAME" --ktest --config-extra "loglevel=info" > "$RUN_DIR/image0.out" 2>&1 || { tail -20 "$RUN_DIR/image0.out"; fail "image"; }
CONTENT=$(du -sb "target/xtask/esp-$NAME" | cut -f1)
ESP_N=$(( (CONTENT * 3 + (256 << 20)) / 512 / 2048 * 2048 ))
DATA_FIRST=$((ESP_FIRST + ESP_N))
DATA_N=1048576
BLOCKS=$((DATA_FIRST + DATA_N + 4096))
IMG="$RUN_DIR/lexar.img"
MKIMG real "$IMG" $BLOCKS 512 9 msr:${MSR[0]}:${MSR[1]} msdata:${WIN[0]}:${WIN[1]}:ntfs efi:$ESP_FIRST:$((DATA_FIRST - 1)) msdata:$DATA_FIRST:$((DATA_FIRST + DATA_N - 1))
DKEY="$SERIAL:$(MKIMG guid 4):$DATA_FIRST:$DATA_N"
EKEY="$SERIAL:$(MKIMG guid 3):$ESP_FIRST:$ESP_N"
OUT=$(MKIMG outside-ranges "$IMG" 512 $ESP_FIRST:$ESP_N $DATA_FIRST:$DATA_N)
KEYS="nvme_write=on;nvme_data=$DKEY;nvme_boot=$EKEY"
echo "image: $((BLOCKS / 2048)) MiB, ESP $ESP_FIRST+$ESP_N, window $DATA_FIRST+$DATA_N"

# esp [LABEL]: the image's files as Georg copies them in Windows: a FAT32 volume of its own (another serial than the
# stick's), label BITSOS, put into partition 3. The stick is the image's own.
esp() {
    local f="$RUN_DIR/esp.fat"
    rm -f "$f"
    mkfs.fat -F 32 -n "${1:-BITSOS}" -h $ESP_FIRST -C "$f" $((ESP_N / 2)) > /dev/null
    mcopy -s -i "$f" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
    dd if="$f" of="$IMG" bs=1M seek=$((ESP_FIRST * 512)) oflag=seek_bytes conv=notrunc,sparse status=none
    dd if="$f" of="$RUN_DIR/esp.head" bs=512 count=32 status=none
    rm -f "$f"
    STICK_SUM=$(sum "target/xtask/$NAME.img")
}
# checks: everything outside the two windows, the stick, the ESP's file system, boot sector and loader.
checks() {
    local what="$1" f="$RUN_DIR/esp.now"
    [ "$(MKIMG outside-ranges "$IMG" 512 $ESP_FIRST:$ESP_N $DATA_FIRST:$DATA_N)" = "$OUT" ] || fail "$what: a byte outside the two windows changed"
    [ "$(sum "target/xtask/$NAME.img")" = "$STICK_SUM" ] || fail "$what: the stick changed"
    dd if="$IMG" of="$f" bs=1M skip=$((ESP_FIRST * 512)) count=$((ESP_N * 512)) iflag=skip_bytes,count_bytes conv=sparse status=none
    fsck.fat -n "$f" > "$RUN_DIR/fsck-$what.log" 2>&1 || { cat "$RUN_DIR/fsck-$what.log"; fail "$what: fsck.fat -n is not clean"; }
    # The reserved sectors but FSInfo (sector 1): boot sector, its backup at 6, backup FSInfo.
    python3 -I - "$RUN_DIR/esp.head" "$f" <<'PY' || fail "$what: the ESP's boot sector or its backup changed"
import sys
a, b = open(sys.argv[1], "rb").read(), open(sys.argv[2], "rb").read(32 * 512)
assert a[:512] == b[:512] and a[1024:] == b[1024:], "reserved sectors differ"
PY
    mtype -i "$f" ::/EFI/BOOT/BOOTX64.EFI | cmp -s - "target/xtask/esp-$NAME/EFI/BOOT/BOOTX64.EFI" || fail "$what: \\EFI\\BOOT\\BOOTX64.EFI changed"
    if mdir -b -i "$f" ::/ | grep -qi "NVBOOT"; then fail "$what: a file in the ESP's root"; fi
    echo "ok: $what: outside unchanged, stick unchanged, fsck clean, boot sector, backup and loader unchanged"
}
# start LOG [extra QEMU args]: the NVMe boots first (bootindex 0), the stick plugged in without a boot index.
start() {
    local log="$1"; shift
    rm -f "$log"
    cargo xtask qemu --name "$NAME" --boot nvme --smp 4 --mem 4096 --timeout "${NV_TIMEOUT:-900}" --serial-log "$log" "$@" -- \
        -drive "if=none,id=nv0,format=raw,file=$IMG,discard=unmap" -device "nvme,id=nvq0,serial=$SERIAL,drive=nv0,bootindex=0" \
        -action reboot=reset > "$RUN_DIR/qemu.out" 2>&1 &
    QPID=$!
}
# bitsboot came from the NVMe ESP: its STICK record names the NVMe controller, no USB ids.
loader_from_nvme_only() {
    local log="$1"
    grep -aq "stick: device path .*NVMe(" "$log" || { grep -a "stick:" "$log"; fail "bitsboot did not come from the NVMe"; }
    grep -aq "stick: no USB VID:PID" "$log" || fail "bitsboot's record has USB ids"
    grep -aq "stick: controller PCI " "$log" || fail "no PCI address of the NVMe controller"
    grep -aq "stick: the boot volume is an NVMe partition" "$log" || fail "bitsboot did not say NVMe"
}
# ... and /boot there is writable.
loader_from_nvme() {
    loader_from_nvme_only "$1"
    grep -aq "fat stick.decided .*nvme=true rw=true" "$1" || { grep -a "fat stick\." "$1"; fail "/boot on the NVMe is not writable"; }
}

# ---- A. ktests from the NVMe's ESP
cargo xtask image --name "$NAME" --ktest --config-extra "ktest=nv.boot.esp,nv.boot.stick_beside,nv.boot.pool;ktest_run=nv/43-boot-nvme;loglevel=info;$KEYS" > "$RUN_DIR/image.out" 2>&1 || { tail -20 "$RUN_DIR/image.out"; fail "image"; }
esp
start "$RUN_DIR/a.log"
wait "$QPID" || true
QPID=""
grep -aq "KTEST done passed=.* failed=0" "$RUN_DIR/a.log" || { grep -a "KTEST\|window\|fat stick\|PANIC" "$RUN_DIR/a.log" | tail -30; fail "A: ktests not green"; }
for t in esp stick_beside pool; do grep -aq "KTEST ok nv.boot.$t " "$RUN_DIR/a.log" || fail "A: nv.boot.$t did not pass"; done
loader_from_nvme "$RUN_DIR/a.log"
if grep -aq "PANIC\|lock.violation" "$RUN_DIR/a.log"; then fail "A: a failure line in the log"; fi
checks A

# ---- B. a generation onto the ESP over MCP, its one try, its confirmation
cargo xtask image --name "$NAME" --config-extra "token=$BITSOS_TOKEN;mcp_port=8765;loglevel=info;boot_confirm_s=20;sys_keep=2;$KEYS" > "$RUN_DIR/image.out" 2>&1 || { tail -20 "$RUN_DIR/image.out"; fail "image"; }
B="$RUN_DIR/bundle"
rm -rf "$B"
mkdir -p "$B"
objcopy --strip-debug "target/xtask/esp-$NAME/BITSOS2/SYS/1/KERNEL" "$B/KERNEL"
cp "target/xtask/esp-$NAME/BITSOS2/SYS/1/INITRD" "$B/"
touch "target/xtask/esp-$NAME/BITSOS2/SYS/1/OK"
esp
start "$RUN_DIR/b.log" --net virtio --hostfwd "$FWD:8765"
CALL() { node host/mcp/mcp-call.mjs "127.0.0.1:$FWD" "$@"; }
for i in $(seq 180); do
    CALL version > "$RUN_DIR/version.txt" 2>/dev/null && break
    [ "$i" = 180 ] && fail "B: MCP does not answer within 180 s"
    sleep 1
done
loader_from_nvme "$RUN_DIR/b.log"
node host/mcp/install.mjs "127.0.0.1:$FWD" --bundle "$B" --bind 127.0.0.1 --reboot --back-s 200 --confirm-s 200 | tee "$RUN_DIR/install.out" || fail "B: install.mjs --reboot exited non-zero"
grep -q "install: generation 2 installed" "$RUN_DIR/install.out" || fail "B: generation 2 was not installed"
CALL run '{"path":"/bin/sh","args":["-c","cat /proc/mounts"],"timeout_ms":20000}' > "$RUN_DIR/mounts.txt"
grep -q "/dev/nvme0n1p3 /boot " "$RUN_DIR/mounts.txt" || { cat "$RUN_DIR/mounts.txt"; fail "B: /boot is not the NVMe's ESP after the reboot"; }
CALL read_file '{"path":"/proc/bitsos/data-window","encoding":"text"}' > "$RUN_DIR/window.txt"
grep -q "window=armed .*boot_first=$ESP_FIRST boot_blocks=$ESP_N" "$RUN_DIR/window.txt" || { cat "$RUN_DIR/window.txt"; fail "B: the windows are not both open"; }
for i in $(seq 60); do
    CALL read_file '{"path":"/run/bitsos/data-state","encoding":"text"}' > "$RUN_DIR/data.txt" 2>/dev/null || true
    grep -q "mounted pool rw" "$RUN_DIR/data.txt" && break
    [ "$i" = 60 ] && { cat "$RUN_DIR/data.txt"; fail "B: /data is not on the window's pool"; }
    sleep 1
done
echo "ok: B: generation 2 installed, tried and confirmed on the NVMe's ESP; /data on the window"
stop_own
grep -a "generation\.chosen\|bitsboot marker\.written\|kexec install\.done\|kexec boot\.confirmed\|fat stick\.decided\|block window\.boot_opened" "$RUN_DIR/b.log" | cut -c1-200
if grep -aq "PANIC\|lock.violation" "$RUN_DIR/b.log"; then fail "B: a failure line in the log"; fi
[ "$(grep -ac "fat stick.decided .*nvme=true rw=true" "$RUN_DIR/b.log")" -ge 2 ] || fail "B: the second start did not take the ESP as /boot"
checks B
F="$RUN_DIR/esp.now"
SYS=$(mdir -b -i "$F" ::/BITSOS2/SYS | tr -d '\r' | sed 's#/$##; s#.*/##' | grep -v '^$' | sort | tr '\n' ' ')
[ "$SYS" = "1 2 " ] || fail "B: SYS is not 1 2: $SYS"
mdir -b -i "$F" ::/BITSOS2/SYS/2 | grep -q "/OK$" || fail "B: SYS\\2\\OK missing"
mtype -i "$F" ::/BITSOS2/SYS/2/KERNEL | cmp -s - "$B/KERNEL" || fail "B: SYS\\2\\KERNEL is not the bundle's"
rm -f "$F"

# ---- C, D. a broken rule: /boot on the ESP read-only, no boot window at either layer, nothing outside written
# refused TEST KEYS LABEL
refused() {
    local t="$1" log="$RUN_DIR/$1.log"
    cargo xtask image --name "$NAME" --ktest --config-extra "ktest=nv.boot.$t;ktest_run=nv/43-boot-nvme;loglevel=info;$2" > "$RUN_DIR/image.out" 2>&1 || { tail -20 "$RUN_DIR/image.out"; fail "image"; }
    esp "$3"
    start "$log"
    wait "$QPID" || true
    QPID=""
    grep -aq "KTEST done passed=.* failed=0" "$log" && grep -aq "KTEST ok nv.boot.$t " "$log" || { grep -a "KTEST\|window\|fat stick\|PANIC" "$log" | tail -30; fail "$t: ktests not green"; }
    loader_from_nvme_only "$log"
    grep -aq "fat stick.decided .*nvme=true rw=false" "$log" || { grep -a "fat stick\." "$log"; fail "$t: /boot on the NVMe is not read-only"; }
    if grep -aq "window\.boot_opened\|PANIC\|lock.violation" "$log"; then fail "$t: a boot window opened, or a failure line"; fi
    checks "$t"
}
refused refused_key "nvme_write=on;nvme_data=$DKEY;nvme_boot=$SERIAL:$(MKIMG guid 3):$ESP_FIRST:$((ESP_N - 2048))" BITSOS
refused refused_label "$KEYS" NOTBITSOS
echo "GREEN: booted from the NVMe's ESP, /boot written only there below \\BITSOS2\\, a generation installed and confirmed, a broken rule left it read-only, outside unchanged"
