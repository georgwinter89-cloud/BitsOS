#!/bin/bash
# slow
# The installer end to end. `xtask release-assets` packs the system (no program tree: QEMU images carry none,
# Georg 30.10.) and builds bitsos-install with that manifest's hash; the installer writes an MBR stick image from the
# assets with every answer as a flag; QEMU boots it to the desktop (generation 1 confirmed, the stick writable). Then
# --update adds the release once more: generation 2 gets its one try (TRY), CONFIG.TXT and data.img stay.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/in-60-installer}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
NAME=in-installer
IMG="target/xtask/$NAME.img"
V=0.0.0-scenario
A="$RUN_DIR/assets"
ID=image-4.3G

qemu_pid() {
    local pf="target/xtask/qemu-$NAME.pid" pid
    [ -f "$pf" ] || return 0
    pid="$(head -1 "$pf" 2>/dev/null || true)"
    case "$pid" in ''|*[!0-9]*) return 0 ;; esac
    if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then echo "$pid"; fi
}
stop_qemu() {
    local pid
    pid="$(qemu_pid)"
    if [ -n "$pid" ]; then
        kill "$pid" 2>/dev/null || true
        for _ in $(seq 1 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
        if kill -0 "$pid" 2>/dev/null; then kill -9 "$pid" 2>/dev/null || true; fi
    fi
    rm -f "target/xtask/qemu-$NAME.pid"
}
QPID=""
cleanup() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then kill "$QPID" 2>/dev/null || true; fi
    stop_qemu
    if [ -n "$QPID" ]; then wait "$QPID" 2>/dev/null || true; fi
}
trap cleanup EXIT

# Boots the image until every marker is in the serial log (or 240 s pass), then stops QEMU.
boot() {
    local n="$1" log="$RUN_DIR/serial-$1.log" m ok
    shift
    stop_qemu
    rm -f "$log"
    cargo xtask qemu --name "$NAME" --timeout 300 --serial-log "$log" > "$RUN_DIR/qemu-$n.out" 2>&1 &
    QPID=$!
    for _ in $(seq 1 240); do
        ok=1
        for m in "$@"; do grep -aq -- "$m" "$log" 2>/dev/null || ok=0; done
        [ "$ok" = 1 ] && break
        kill -0 "$QPID" 2>/dev/null || break
        sleep 1
    done
    cleanup
    QPID=""
    for m in "$@"; do grep -aq -- "$m" "$log" || { echo "RED (boot $n): no '$m' in the serial log"; exit 1; }; done
    if grep -aq "PANIC" "$log"; then echo "RED (boot $n): a panic"; exit 1; fi
    if grep -aq "$TOKEN" "$log"; then echo "RED (boot $n): the token appears in the serial log"; exit 1; fi
    grep -a "stick.decided" "$log" | grep -q "rw=true" || { echo "RED (boot $n): the kernel did not take the stick as writable"; grep -a "stick.decided" "$log"; exit 1; }
}

on_stick() { mdir -b -i "$IMG@@1M" "::/$1" 2>/dev/null; }

cargo xtask release-assets --version "$V" --trees none --out "$A" --zstd-level 3 > "$RUN_DIR/release-assets.log" 2>&1 \
    || { tail -20 "$RUN_DIR/release-assets.log"; echo "RED: release-assets failed"; exit 1; }
INST="$A/bitsos-install-$V-linux-x86_64"
[ -x "$INST" ] || { echo "RED: no installer in $A"; exit 1; }
if readelf -l "$INST" | grep -q "INTERP"; then echo "RED: $INST needs a program loader (not static)"; exit 1; fi

COMMON=(--image "$IMG" --assets "$A" --host-dir "$RUN_DIR/host" --cache "$RUN_DIR/cache" --steam no --brave no --trees none)
rm -rf "$RUN_DIR/host" "$IMG"
# A wrong id is refused before anything is written.
if "$INST" "${COMMON[@]}" --image-size 4G --erase sdb-31.0G --cable-only --remote no > "$RUN_DIR/wrong-id.log" 2>&1; then
    echo "RED: the installer accepted a disk id that is not the image's"; exit 1
fi
"$INST" "${COMMON[@]}" --image-size 4G --erase "$ID" --cable-only --remote yes --extra-config "desktop=on;data_img_mb=64" \
    > "$RUN_DIR/install.log" 2>&1 || { cat "$RUN_DIR/install.log"; echo "RED: the installer failed"; exit 1; }
grep -q "generations: 1 OK" "$RUN_DIR/install.log" || { cat "$RUN_DIR/install.log"; echo "RED: no generation 1 OK"; exit 1; }
TOKEN="$(cat "$RUN_DIR"/host/sticks/*/token.txt)"
[ "${#TOKEN}" = 64 ] || { echo "RED: no token saved on the host"; exit 1; }
[ "$(stat -c %a "$RUN_DIR"/host/sticks/*/token.txt)" = 600 ] || { echo "RED: the saved token is not 0600"; exit 1; }
# With every answer as a flag (an agent's run) the token is not printed, only where it is kept; the cable install
# has nothing else secret.
if grep -q "$TOKEN" "$RUN_DIR/install.log"; then echo "RED: a batch run printed the token"; exit 1; fi
grep -q "token is in token.txt" "$RUN_DIR/install.log" || { cat "$RUN_DIR/install.log"; echo "RED: the run does not say where the token is"; exit 1; }
for k in nvme_write nvme_data nvme_boot stick_write usb_msd_write mcp_confirmer; do
    if mtype -i "$IMG@@1M" ::/BITSOS2/CONFIG.TXT | grep -Eq "^\s*$k\s*="; then echo "RED: CONFIG.TXT sets $k"; exit 1; fi
done
mtype -i "$IMG@@1M" ::/BITSOS2/CONFIG.TXT | grep -q "^mcp=service$" || { echo "RED: no mcp=service after --remote yes"; exit 1; }

boot 1 "bitsboot generation.chosen number=1 reason=NEWEST_OK" "fat stick.decided" "console desktop.up program=/bin/bitscomp" "console bootscreen.end reason=handover" "ext4 data.created"
on_stick BITSOS2 | grep -q "/data.img$" || { echo "RED: BitsOS made no data.img on the installed stick"; exit 1; }
DATA_SUM="$(mtype -i "$IMG@@1M" ::/BITSOS2/data.img | sha256sum)"
[ "$DATA_SUM" != "$(sha256sum < /dev/null)" ] || { echo "RED: data.img cannot be read from the image"; exit 1; }
CFG_SUM="$(mtype -i "$IMG@@1M" ::/BITSOS2/CONFIG.TXT | sha256sum)"

"$INST" "${COMMON[@]}" --update "$ID" > "$RUN_DIR/update.log" 2>&1 || { cat "$RUN_DIR/update.log"; echo "RED: the update failed"; exit 1; }
grep -q "generations: 1 OK, 2 NEW" "$RUN_DIR/update.log" || { cat "$RUN_DIR/update.log"; echo "RED: the update made no generation 2 NEW"; exit 1; }
[ "$(mtype -i "$IMG@@1M" ::/BITSOS2/CONFIG.TXT | sha256sum)" = "$CFG_SUM" ] || { echo "RED: the update changed CONFIG.TXT"; exit 1; }
[ "$(mtype -i "$IMG@@1M" ::/BITSOS2/data.img | sha256sum)" = "$DATA_SUM" ] || { echo "RED: the update changed data.img"; exit 1; }

boot 2 "bitsboot generation.chosen number=2 reason=NEWEST_TRY" "fat stick.decided" "console desktop.up program=/bin/bitscomp" "created=false image=/boot/BITSOS2/data.img"
on_stick BITSOS2/SYS/2 | grep -q "TRY" || { echo "RED: bitsboot wrote no TRY next to generation 2"; exit 1; }
echo "in installer: green (release-assets packed the system, the static installer wrote an MBR/FAT32 stick image from them with flags only, it booted to the desktop with generation 1 confirmed and the stick writable; --update added generation 2, which got its one try, CONFIG.TXT and data.img unchanged)"
