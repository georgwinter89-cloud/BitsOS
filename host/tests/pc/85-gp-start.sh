#!/bin/bash
# slow
# GP-START: the Gaming-PC's first start on a slow USB-2 stick (round 1, photo foto-1.jpg in
# plan/hw/gaming-pc-round1-2026-11-15). The board's DMI names (profile
# gaming-pc), the firmware picture at 2560x1440, the stick throttled to USB-2 stick speed (8 MiB/s writing, 30 MiB/s
# reading), 3.5 GiB, without data.img and with old bytes in its free space (a file of a marker, removed again).
#  a: data.img (2048 MiB) is made within GPS_CREATE_MAX_S (30 s) with its clusters allocated, not zero-filled, and is
#    root's alone; settingsd never calls the stick hung and no resync fails; /data is mounted from it.
#     the report page turns by keys and gives the console; the console's latency figures are printed (QEMU timing
#    is indicative; ktest in.draw.timing judges the drawing, the Gaming-PC round the real figures).
#     after a clean restart: the volume taken out of the pool on the host is e2fsck -fn clean and holds no byte of
#    the old marker, while the raw data.img does (the proof the clusters were not written: nothing old shows through).
#  b (first): the same at 64 KiB/s writing: settingsd meets the busy stick, says so quietly and later carries its
#     settings back to the stick with one plain line.
# GPS_BEFORE=1 measures without judging (the code before GP-START has neither property).
. "$(dirname "$0")/../bt/lib.bash"
HERE="$ROOT/host/tests/pc"
LO="${BITSOS_PORT_LO:-22500}"
FWD=$((LO + 85))
TOKEN="gp-start-scenario-token-4e19"
QMP="$RUN_DIR/qmp.sock"
NAME=gp-start
IMG="$(img "$NAME")"
MARK="BITSOS-OLD-STICK-BYTES:"
GPC_DMI=(-smbios "type=1,manufacturer=ASRock,product=Z890 Taichi OCF" -smbios "type=2,manufacturer=ASRock,product=Z890 Taichi OCF")
# The keyboard QMP types on: a USB one (no PS/2, i8042=off), as Georg's on the board.
KBD=(-machine i8042=off -device usb-kbd,id=gpskbd,bus=xhci.0)
SLOW=(-set "drive.stick.throttling.bps-write=${GPS_WRITE_BPS:-8388608}" -set "drive.stick.throttling.bps-read=${GPS_READ_BPS:-31457280}")

QPID=""
stop_own() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
        local q; q="$(head -1 "$X/qemu-$NAME.pid" 2>/dev/null || true)"
        case "$q" in ''|*[!0-9]*) ;; *) kill "$q" 2>/dev/null || true ;; esac
        kill "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    QPID=""
}
cleanup() { stop_own; rm -f "$QMP" "$RUN_DIR/old.bin" "$RUN_DIR/data.img" "$RUN_DIR/vol.img"; }
trap cleanup EXIT

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;gp_net=on;gp_stick=on;screen=2560x1440" \
    >"$RUN_DIR/image.txt" 2>&1 || { cat "$RUN_DIR/image.txt"; echo "RED: cannot build the image"; exit 1; }
# Room for data.img (2048 MiB) and the 512 MiB V2 keeps free: the FAT image made again larger from xtask's ESP tree.
fresh_stick() {
    rm -f "$IMG"
    mkfs.fat -F 32 -n BITSOS -C "$IMG" $((3584 * 1024)) >/dev/null
    mcopy -s -i "$IMG" "$X/esp-$NAME/EFI" "$X/esp-$NAME/BITSOS2" ::/
    mdir -i "$IMG" ::BITSOS2/data.img >/dev/null 2>&1 && { echo "RED: the fresh stick holds data.img"; exit 1; }
    return 0
}
# $1 = tag; the rest = the checker's environment.
boot() {
    local tag="$1"; shift
    LOG="$(logf "$tag")"
    rm -f "$LOG" "$QMP"
    cargo xtask qemu --name "$NAME" --boot-retries 9 --timeout 1500 --serial-log "$LOG" --smp 4 --net virtio --hostfwd "$FWD:8765" \
        --res 2560x1440 -- "${GPC_DMI[@]}" "${KBD[@]}" "${SLOW[@]}" -qmp "unix:$QMP,server=on,wait=off" >"$RUN_DIR/$tag.out" 2>&1 &
    QPID=$!
    for _ in $(seq 1 120); do [ -S "$QMP" ] && break; sleep 0.5; done
    env "$@" python3 -I "$HERE/gp_start_check.py" "$FWD" "$TOKEN" "$QMP" "$LOG" || FAILED=1
    for _ in $(seq 1 120); do kill -0 "$QPID" 2>/dev/null || break; sleep 1; done
    stop_own
    if grep -a "PANIC" "$LOG"; then echo "RED: the kernel panicked"; FAILED=1; fi
}

# b (first, no marker needed): a stick far slower still, 64 KiB/s writing: even the FAT chain keeps the stick busy for
# about a minute, and settingsd meets it. It must say "busy", wait without warnings and carry its settings back.
if [ "${GPS_BEFORE:-0}" != 1 ]; then
    echo "--- boot b: a very slow stick (64 KiB/s writing): settingsd meets the busy stick"
    fresh_stick
    SLOW=(-set "drive.stick.throttling.bps-write=65536" -set "drive.stick.throttling.bps-read=${GPS_READ_BPS:-31457280}")
    boot b GPS_CREATE_MAX_S=900 GPS_EXPECT_BUSY=1 GPS_KEYS=0
    SLOW=(-set "drive.stick.throttling.bps-write=${GPS_WRITE_BPS:-8388608}" -set "drive.stick.throttling.bps-read=${GPS_READ_BPS:-31457280}")
fi

# a: USB-2 stick speed, with old bytes in nearly all of the free space, as a used stick has them.
fresh_stick
python3 -I - "$RUN_DIR/old.bin" "$MARK" <<'PY'
import sys
block = (sys.argv[2].encode() * 4096)[: 1 << 16]
with open(sys.argv[1], "wb") as f:
    for _ in range(2600 * 16):
        f.write(block)
PY
mcopy -i "$IMG" "$RUN_DIR/old.bin" ::old.bin
mdel -i "$IMG" ::old.bin
rm -f "$RUN_DIR/old.bin"
echo "--- boot a: first start on a USB-2-speed stick"
boot a

echo "--- the image on the host"
mcopy -n -i "$IMG" ::BITSOS2/data.img "$RUN_DIR/data.img" || { echo "RED: no data.img on the stick"; exit 1; }
raw=$(grep -caF "$MARK" "$RUN_DIR/data.img" || true)
BITSVOL_POOL_IMG="$RUN_DIR/data.img" BITSVOL_OUT="$RUN_DIR/vol.img" \
    cargo test -q -p fs-ext4 --lib tests::data::extract_volume -- --ignored --exact >"$RUN_DIR/extract.log" 2>&1 \
    || { cat "$RUN_DIR/extract.log"; echo "RED: the pool in data.img does not open on the host"; exit 1; }
inside=$(grep -caF "$MARK" "$RUN_DIR/vol.img" || true)
echo "  lines with the old marker: raw data.img $raw, the volume inside $inside"
if [ "${GPS_BEFORE:-0}" != 1 ]; then
    [ "$raw" -gt 0 ] && echo "  ok: the raw image keeps old bytes where nothing was written (no zeros were written)" \
        || { echo "  RED: the raw image holds no old bytes (zero-filled?)"; FAILED=1; }
fi
[ "$inside" = 0 ] && echo "  ok: no old byte shows through the pool's volume" || { echo "  RED: old bytes inside the volume"; FAILED=1; }
if e2fsck -fn "$RUN_DIR/vol.img" >"$RUN_DIR/fsck.log" 2>&1; then echo "  ok: e2fsck -fn clean"; else cat "$RUN_DIR/fsck.log"; echo "  RED: e2fsck -fn"; FAILED=1; fi
fsck.fat -n "$IMG" >"$RUN_DIR/fsckfat.log" 2>&1 && echo "  ok: fsck.fat -n clean on the stick" || { cat "$RUN_DIR/fsckfat.log"; echo "  RED: fsck.fat"; FAILED=1; }
[ "${GPS_BEFORE:-0}" = 1 ] && { echo "MEASURED (GPS_BEFORE=1: nothing judged)"; exit 0; }
[ "$FAILED" = 0 ] && echo "GREEN: data.img made fast on the slow stick, settingsd calm, nothing old inside, console measured" || exit 1
