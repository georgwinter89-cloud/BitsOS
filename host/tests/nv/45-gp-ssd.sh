#!/bin/bash
# slow
# time-limit: 3000
# GP-DISK (Georg 15.11.2026): the Gaming-PC moves from its stick onto two partitions of its Lexar SSD, in QEMU.
# OVMF with the board's DMI names (the profile gaming-pc), three NVMe controllers at fixed places: 00:10.0 stands for
# the Windows Samsung (an ESP, NTFS), 00:11.0 for the Lexar (CachyOS's ext4 "daten", the 1 GiB-like ESP BITSOS, the
# empty "Linux data" partition), 00:12.0 for the CachyOS Samsung (an ESP, ext4). QEMU's controllers all carry
# 1b36:0010, so the line names the place with QEMU's own ids: gp_nvme=00:11.0/1b36:0010 (no test-only key: the ids
# are part of the line, the rule compares them as on the real board). Never a real disk.
#   R  gp_nvme names 00:11.0 with the Lexar's real ids: no controller matches, all three stay untouched.
#   S  from the stick, gp_nvme, nvme_write=on and nvme_boot= by hand: only 00:11.0 is driven; bitsdisk typed on
#      QEMU's USB keyboard (refused over MCP, with an injected or a wrong code) writes nvme_data= and makes the pool.
#   M  the next boot from the stick: /data on the window, the stick's data.img read-only at /stickdata;
#      `bitsdisk umzug` typed at the keyboard copies it over once (a second run refuses); `poweroff` typed.
#   E  the stick's \EFI\BOOT\BOOTX64.EFI and \BITSOS2\ (without data.img) copied onto the ESP, as Georg does it by hand.
#   B  booted from that ESP (bootindex 0), the stick plugged in as the rescue: /boot is the ESP and writable below
#      \BITSOS2\, /data the window; install.mjs --reboot installs generation 2 onto the ESP, which gets its one try and
#      confirms itself.
# After every boot: the two other images byte for byte as they were, and QEMU's block statistics of their drives
# unchanged from bitsboot's first line on (no read, write, flush or discard reached them); no `nvme probe` for them,
# both `pci untouched ... key=nvme_bind`; on the Lexar every byte outside the two windows as it was.
# Ports: the lane's range (BITSOS_PORT_LO): MCP LO+460, install.mjs's server LO+470..LO+489.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/nv-45-gp-ssd}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
NV="$(pwd)/host/tests/nv"
LO="${BITSOS_PORT_LO:-19000}"
FWD=$((LO + 460))
export BITSOS_INSTALL_PORTS="$((LO + 470))-$((LO + 489))"
export BITSOS_TOKEN="gp-disk-scenario-token-5d21"
TOKEN="$BITSOS_TOKEN"
export MTOOLS_SKIP_CHECK=1
NAME=gp-ssd
SERIAL=LXQEMUGPSSD0042
MKIMG() { python3 -I "$NV/mkimg.py" "$@"; }
CHECK() { python3 -I "$NV/gp_ssd_check.py" "$@"; }
sum() { sha256sum "$1" | cut -d' ' -f1; }
fail() { echo "RED: $*"; exit 1; }
GPC_DMI=(-smbios "type=1,manufacturer=ASRock,product=Z890 Taichi OCF" -smbios "type=2,manufacturer=ASRock,product=Z890 Taichi OCF")
QMP="$RUN_DIR/qmp.sock"
QMP2="$RUN_DIR/qmp2.sock"
NAMED="00:11.0/1b36:0010"

QPID=""
WPID=""
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
    if [ -n "$WPID" ]; then kill "$WPID" 2>/dev/null || true; wait "$WPID" 2>/dev/null || true; fi
    WPID=""
}
trap stop_own EXIT

# ---- the three drives
WIN="$RUN_DIR/samsung-win.img"
CACHY="$RUN_DIR/samsung-cachy.img"
LEXAR="$RUN_DIR/lexar.img"
MKIMG real "$WIN" 131072 512 3 efi:2048:67583 msdata:67584:129023:ntfs
MKIMG real "$CACHY" 131072 512 5 efi:2048:67583 linux:67584:129023
# ext4's magic in the CachyOS stand-ins (the signature that keeps "daten" from being offered).
ext4_at() { printf '\x53\xef' | dd of="$1" bs=1 seek=$(( $2 * 512 + 0x438 )) conv=notrunc status=none; }
ext4_at "$CACHY" 67584
DATEN=(2048 67583)
ESP_FIRST=67584
cargo xtask image --name "$NAME" --ktest --config-extra "ktest=contract.frame_alloc" > "$RUN_DIR/image0.out" 2>&1 || { tail -20 "$RUN_DIR/image0.out"; fail "image"; }
CONTENT=$(du -sb "target/xtask/esp-$NAME" | cut -f1)
ESP_N=$(( (CONTENT * 3 + (256 << 20)) / 512 / 2048 * 2048 ))
DATA_FIRST=$((ESP_FIRST + ESP_N))
DATA_N=1048576
BLOCKS=$((DATA_FIRST + DATA_N + 4096))
MKIMG real "$LEXAR" $BLOCKS 512 9 linux:${DATEN[0]}:${DATEN[1]} efi:$ESP_FIRST:$((DATA_FIRST - 1)) linux:$DATA_FIRST:$((DATA_FIRST + DATA_N - 1))
ext4_at "$LEXAR" "${DATEN[0]}"
EKEY="$SERIAL:$(MKIMG guid 2):$ESP_FIRST:$ESP_N"
WIN_SUM=$(sum "$WIN")
CACHY_SUM=$(sum "$CACHY")
OUT=$(MKIMG outside-ranges "$LEXAR" 512 $ESP_FIRST:$ESP_N $DATA_FIRST:$DATA_N)
LEXAR_SUM=$(sum "$LEXAR")
echo "lexar: $((BLOCKS / 2048)) MiB, daten ${DATEN[0]}-${DATEN[1]}, ESP $ESP_FIRST+$ESP_N, window $DATA_FIRST+$DATA_N"

# LEXAR_BOOT: ",bootindex=0" when the firmware starts from the Lexar's ESP (boot B).
LEXAR_BOOT=""
drives() {
    DRIVES=(-drive "if=none,id=nvw,format=raw,file=$WIN" -device "nvme,id=nqw,serial=SAMWINQEMU0001,drive=nvw,bus=pcie.0,addr=0x10"
            -drive "if=none,id=nvl,format=raw,file=$LEXAR,discard=unmap" -device "nvme,id=nql,serial=$SERIAL,drive=nvl,bus=pcie.0,addr=0x11$LEXAR_BOOT"
            -drive "if=none,id=nvc,format=raw,file=$CACHY" -device "nvme,id=nqc,serial=SAMCACHYQEMU01,drive=nvc,bus=pcie.0,addr=0x12")
}
KBD=(-machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1)

# Room for data.img (64 MiB) and the 512 MiB V2 keeps free on the stick (as nv/window_lib.bash).
grow_stick() {
    local stick="target/xtask/$NAME.img" kib
    kib=$(( $(stat -c %s "$stick") / 1024 + 700 * 1024 ))
    rm -f "$stick"
    mkfs.fat -F 32 -n BITSOS -C "$stick" "$kib" > /dev/null
    mcopy -s -i "$stick" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
}

# start TAG [xtask qemu options] -- [extra QEMU args]: one QEMU in the background, its block-statistics watcher beside it.
start() {
    local tag="$1"; shift
    local log="$RUN_DIR/$tag.log" opts=() extra=()
    while [ $# -gt 0 ] && [ "$1" != "--" ]; do opts+=("$1"); shift; done
    [ $# -gt 0 ] && shift
    extra=("$@")
    rm -f "$log" "$QMP" "$QMP2" "$RUN_DIR/$tag.stats"
    drives
    cargo xtask qemu --name "$NAME" --smp 4 --mem 4096 --timeout "${GP_TIMEOUT:-900}" --serial-log "$log" "${opts[@]}" -- \
        "${GPC_DMI[@]}" "${DRIVES[@]}" -qmp "unix:$QMP2,server=on,wait=off" "${extra[@]}" > "$RUN_DIR/$tag.out" 2>&1 &
    QPID=$!
    CHECK watch "$QMP2" "$log" "$RUN_DIR/$tag.stats" nvw nvc > "$RUN_DIR/$tag.watch" 2>&1 &
    WPID=$!
}
finish() {
    wait "$QPID" || true
    QPID=""
    wait "$WPID" || true
    WPID=""
}

# after TAG: the rules every boot must keep.
after() {
    local tag="$1" log="$RUN_DIR/$1.log"
    [ "$(sum "$WIN")" = "$WIN_SUM" ] || fail "$tag: the Windows stand-in changed"
    [ "$(sum "$CACHY")" = "$CACHY_SUM" ] || fail "$tag: the CachyOS stand-in changed"
    [ "$(MKIMG outside-ranges "$LEXAR" 512 $ESP_FIRST:$ESP_N $DATA_FIRST:$DATA_N)" = "$OUT" ] || fail "$tag: a byte of the Lexar outside its two windows changed"
    python3 -I - "$RUN_DIR/$tag.stats" <<'PY' || fail "$tag: a request reached an unnamed drive after bitsboot started"
import json, sys
d = json.load(open(sys.argv[1]))
a, b = d["at_loader"], d["last"]
assert set(a) == {"nvw", "nvc"}, a
for k in a:
    assert a[k] == b[k], (k, a[k], b[k])
PY
    for f in 00:10.0 00:12.0; do
        grep -aq "pci untouched function=$f key=nvme_bind" "$log" || fail "$tag: $f is not untouched by nvme_bind"
        if grep -aq "nvme probe\(_refused\)\? function=$f" "$log"; then fail "$tag: the NVMe driver was offered $f"; fi
    done
    if grep -aq "PANIC\|lock.violation" "$log"; then fail "$tag: a failure line in the log"; fi
    echo "ok: $tag: the two other drives untouched (images, no request since bitsboot, never offered), the Lexar outside its windows as it was"
}
named_driven() {
    local log="$RUN_DIR/$1.log"
    grep -aq "bitsboot profile.tiers net=on stick=on nvme=on" "$log" || fail "$1: the loader did not release the nvme tier"
    grep -aq "pci gp_nvme.released function=00:11.0" "$log" || fail "$1: 00:11.0 was not released"
    grep -aq "nvme probe function=00:11.0" "$log" || fail "$1: the named controller has no driver"
    grep -aq "block internal_disks.one" "$log" || fail "$1: the block layer does not know the one named controller"
    if grep -a "nvme probe function=" "$log" | grep -vq "function=00:11.0 "; then fail "$1: another controller was probed"; fi
    if grep -aq "nvme_bind=on\|kernel profile.guard" "$log"; then fail "$1: the guard was not the loader's"; fi
}

# ---- R. the real Lexar's ids at QEMU's place: nothing matches, nothing is driven.
cargo xtask image --name "$NAME" --ktest --config-extra "ktest=contract.frame_alloc;loglevel=info;gp_nvme=00:11.0/1d97:5236;nvme_write=on;nvme_boot=$EKEY" > "$RUN_DIR/image.out" 2>&1 || { tail -20 "$RUN_DIR/image.out"; fail "image"; }
GP_TIMEOUT=240 start r
finish
grep -aq "KTEST done passed=1 failed=0" "$RUN_DIR/r.log" || fail "R: the kernel did not come up"
grep -aq "pci gp_nvme.refused function=00:11.0 wanted=00:11.0/1d97:5236" "$RUN_DIR/r.log" || { grep -a "gp_nvme\|untouched" "$RUN_DIR/r.log"; fail "R: the mismatch is not said"; }
grep -aq "pci untouched function=00:11.0 key=nvme_bind" "$RUN_DIR/r.log" || fail "R: 00:11.0 is not untouched"
if grep -aq "nvme probe" "$RUN_DIR/r.log"; then fail "R: a controller was probed"; fi
grep -aq "block configured .*nvme_write=true" "$RUN_DIR/r.log" || fail "R: nvme_write=on stands for the named place"
grep -aq "block internal_disks.one" "$RUN_DIR/r.log" || fail "R: the block layer knows a controller is named (none is driven)"
after r
[ "$(sum "$LEXAR")" = "$LEXAR_SUM" ] || fail "R: the Lexar changed"

# ---- S. bitsdisk at the keyboard, from the stick
BASE="token=$TOKEN;mcp_port=8765;loglevel=info;data_img_mb=64;gp_net=on;gp_stick=on;gp_nvme=$NAMED;nvme_write=on;nvme_boot=$EKEY"
cargo xtask image --name "$NAME" --config-extra "$BASE" > "$RUN_DIR/image.out" 2>&1 || { tail -20 "$RUN_DIR/image.out"; fail "image"; }
grow_stick
start s --net virtio --hostfwd "$FWD:8765" -- "${KBD[@]}" -qmp "unix:$QMP,server=on,wait=off"
CHECK report "$QMP" "$RUN_DIR/s.log" || fail "S: the report page"
python3 -I "$NV/window_check.py" setup "$FWD" "$TOKEN" "$QMP" "$RUN_DIR/s.log" "$SERIAL" 3 "$DATA_FIRST" "$DATA_N" \
    || { grep -a "nvme \|ext4 data\|window\|gp_nvme" "$RUN_DIR/s.log" | tail -40; fail "S: bitsdisk"; }
finish
named_driven s
grep -aq "nvme setup.done" "$RUN_DIR/s.log" || fail "S: bitsdisk did not finish"
after s
mtype -i "target/xtask/$NAME.img" ::BITSOS2/CONFIG.TXT | tr -d '\r' | grep -q "^nvme_data=$SERIAL:.*:$DATA_FIRST:$DATA_N$" || fail "S: the stick's CONFIG.TXT has no nvme_data= line"

# ---- M. /data on the window; the move typed at the keyboard
# data.img as boot S left it, read on the host while no QEMU runs (root's alone since GP-START, so not over MCP).
mcopy -n -i "target/xtask/$NAME.img" ::BITSOS2/data.img "$RUN_DIR/data.img" || fail "S: the stick has no data.img"
sum "$RUN_DIR/data.img" > "$RUN_DIR/data-img.sum"
rm -f "$RUN_DIR/data.img"
start m --net virtio --hostfwd "$FWD:8765" -- "${KBD[@]}" -qmp "unix:$QMP,server=on,wait=off"
CHECK report "$QMP" "$RUN_DIR/m.log" || fail "M: the report page"
python3 -I "$NV/window_check.py" moved "$FWD" "$TOKEN" "$QMP" "$RUN_DIR/m.log" "$SERIAL" 3 "$DATA_FIRST" "$DATA_N" || fail "M: /data on the window"
CHECK umzug "$FWD" "$TOKEN" "$QMP" "$RUN_DIR/m.log" || fail "M: bitsdisk umzug"
finish
named_driven m
grep -aq "block shutdown.disk why=[a-z]* disk=nvme0n1 " "$RUN_DIR/m.log" || fail "M: no storage stop of nvme0n1 at the power-off"
after m
IMG_SUM=$(cat "$RUN_DIR/data-img.sum")
mcopy -n -i "target/xtask/$NAME.img" ::BITSOS2/data.img "$RUN_DIR/data.img" || fail "M: the stick has no data.img any more"
[ "$(sum "$RUN_DIR/data.img")" = "$IMG_SUM" ] || fail "M: data.img changed while it was /stickdata"
rm -f "$RUN_DIR/data.img"

# ---- E. the copy onto the ESP, as the guide has Georg do it: a FAT32 of its own with the label BITSOS, the stick's
# \EFI\ and \BITSOS2\ without data.img (it would not fit, and it stays the stick's).
ESPDIR="$RUN_DIR/esp-copy"
rm -rf "$ESPDIR"
mkdir -p "$ESPDIR"
mcopy -s -n -i "target/xtask/$NAME.img" ::EFI ::BITSOS2 "$ESPDIR/"
rm -f "$ESPDIR/BITSOS2/data.img"
# The stick's generation is the confirmed one Georg's stick carries (generation 107 is OK there since round 1).
touch "$ESPDIR/BITSOS2/SYS/1/OK"
F="$RUN_DIR/esp.fat"
rm -f "$F"
mkfs.fat -F 32 -n BITSOS -h $ESP_FIRST -C "$F" $((ESP_N / 2)) > /dev/null
mcopy -s -i "$F" "$ESPDIR/EFI" "$ESPDIR/BITSOS2" ::/
dd if="$F" of="$LEXAR" bs=1M seek=$((ESP_FIRST * 512)) oflag=seek_bytes conv=notrunc,sparse status=none
rm -f "$F"
STICK_SUM=$(sum "target/xtask/$NAME.img")

# ---- B. from the ESP: /boot and /data on the SSD, a generation installed there from afar
B="$RUN_DIR/bundle"
rm -rf "$B"
mkdir -p "$B"
objcopy --strip-debug "$ESPDIR/BITSOS2/SYS/1/KERNEL" "$B/KERNEL"
cp "$ESPDIR/BITSOS2/SYS/1/INITRD" "$B/"
LEXAR_BOOT=",bootindex=0"
start b --boot nvme --net virtio --hostfwd "$FWD:8765" -- -action reboot=reset
CHECK ssd "$FWD" "$TOKEN" $ESP_FIRST $ESP_N $DATA_FIRST $DATA_N || fail "B: the SSD as /boot and /data"
grep -aq "stick: device path .*NVMe(" "$RUN_DIR/b.log" || fail "B: bitsboot did not come from the NVMe"
grep -aq "fat stick.decided .*nvme=true rw=true" "$RUN_DIR/b.log" || fail "B: /boot on the NVMe is not writable"
node host/mcp/install.mjs "127.0.0.1:$FWD" --bundle "$B" --bind 127.0.0.1 --reboot --back-s 240 --confirm-s 240 | tee "$RUN_DIR/install.out" || fail "B: install.mjs --reboot exited non-zero"
grep -q "install: generation 2 installed" "$RUN_DIR/install.out" || fail "B: generation 2 was not installed"
stop_own
[ "$(grep -ac "fat stick.decided .*nvme=true rw=true" "$RUN_DIR/b.log")" -ge 2 ] || fail "B: the second start did not take the ESP as /boot"
grep -aq "kexec boot.confirmed generation=2" "$RUN_DIR/b.log" || fail "B: generation 2 did not confirm itself"
named_driven b
after b
[ "$(sum "target/xtask/$NAME.img")" = "$STICK_SUM" ] || fail "B: the stick (the rescue) changed"
F="$RUN_DIR/esp.now"
dd if="$LEXAR" of="$F" bs=1M skip=$((ESP_FIRST * 512)) count=$((ESP_N * 512)) iflag=skip_bytes,count_bytes conv=sparse status=none
fsck.fat -n "$F" > "$RUN_DIR/fsck.log" 2>&1 || { cat "$RUN_DIR/fsck.log"; fail "B: fsck.fat -n of the ESP is not clean"; }
SYS=$(mdir -b -i "$F" ::/BITSOS2/SYS | tr -d '\r' | sed 's#/$##; s#.*/##' | grep -v '^$' | sort | tr '\n' ' ')
[ "$SYS" = "1 2 " ] || fail "B: SYS on the ESP is not 1 2: $SYS"
mdir -b -i "$F" ::/BITSOS2/SYS/2 | grep -q "/OK$" || fail "B: SYS\\2\\OK missing on the ESP"
mtype -i "$F" ::/BITSOS2/SYS/2/KERNEL | cmp -s - "$B/KERNEL" || fail "B: SYS\\2\\KERNEL on the ESP is not the bundle's"
mtype -i "$F" ::/BITSOS2/GPSSD.TXT | grep -q "written on the SSD's ESP" || fail "B: the file written over MCP is not on the ESP"
mtype -i "$F" ::/EFI/BOOT/BOOTX64.EFI | cmp -s - "$ESPDIR/EFI/BOOT/BOOTX64.EFI" || fail "B: \\EFI\\BOOT\\BOOTX64.EFI changed"
rm -f "$F"
echo "GREEN: only the named controller driven, the other two never reached; bitsdisk and the move typed at the keyboard; booted from the SSD's ESP with /data on its window; a generation installed there and confirmed"
