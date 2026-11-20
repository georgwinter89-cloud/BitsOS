#!/usr/bin/env bash
# DB5a: what the launchers of Brave, LibreOffice and Steam give their program for Durchblick, under BusyBox's sh on
# the build host with fake trees, a fake desktop and a fake accessibility bus socket (BRAVE_A11Y_BUS, LO_A11Y_BUS,
# SB_A11Y_BUS). With the bus: Brave and LibreOffice get AT_SPI_BUS_ADDRESS (Brave also ACCESSIBILITY_ENABLED and
# --force-renderer-accessibility) and no NO_AT_BRIDGE; Steam AT_SPI_BUS_ADDRESS and ACCESSIBILITY_ENABLED
# (DECISIONS 18.11.: Durchblick reads Steam by default). Without the bus: NO_AT_BRIDGE=1 for Brave and LibreOffice,
# nothing for Steam.
# Host only, a few seconds; no program runs, no QEMU, no screen. Red when it cannot do its work.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
wt=${BITSOS_ROOT:-$(cd "$here/../../.." && pwd)}
R=${BITSOS_RUN_DIR:-$wt/target/us/a11y-launchers}/fake
BB=$wt/user/ports/busybox/out/busybox
if [ ! -x "$BB" ]; then
  V2_OFFLINE=1 sh "$wt/user/ports/busybox/build.sh" >/dev/null 2>&1 || true
fi
[ -x "$BB" ] || { echo "RED: no BusyBox build ($BB): user/ports/busybox/build.sh"; exit 1; }
command -v python3 >/dev/null || { echo "RED: no python3 on this host"; exit 1; }
rm -rf "$R"; mkdir -p "$R/bin" "$R/xdg" "$R/tmp" "$R/user" "$R/data/home/1000" "$R/bus"
fails=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fails=$((fails + 1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1"; echo "     output: $(tr '\n' '|' <"$R/out" | cut -c1-600)"; fi; }
pids=
trap 'for p in $pids; do kill "$p" 2>/dev/null; done' EXIT
sock() { python3 -I -c 'import socket, sys; socket.socket(socket.AF_UNIX).bind(sys.argv[1])' "$1" || { echo "RED: no socket at $1 (a shorter BITSOS_RUN_DIR?)"; exit 1; }; }

for a in $("$BB" --list); do ln -sf "$BB" "$R/bin/$a"; done
rm -f "$R/bin/pidof" "$R/bin/id" "$R/bin/unshare"
printf '#!%s sh\n[ "$1" = bitscomp ] && [ -n "$FAKE_BITSCOMP" ] && echo "$FAKE_BITSCOMP"\nexit 0\n' "$BB" >"$R/bin/pidof"
printf '#!%s sh\n[ "$1" != -G ] || { echo 1000; exit 0; }\n[ "$1" != -un ] || { echo georg; exit 0; }\necho 1000\n' "$BB" >"$R/bin/id"
printf '#!%s sh\nexit 0\n' "$BB" >"$R/bin/unshare"
chmod +x "$R/bin/pidof" "$R/bin/id" "$R/bin/unshare"
BUS=$R/bus/a11y
sock "$BUS"

# Brave: a fake tree and desktop (as br/20-launcher has them), a fake brave that writes its environment and arguments.
T=$R/mnt/BITSOS2/brave
mkdir -p "$T/lib/dri" "$T/locales" "$T/etc/fonts" "$T/share/glib-2.0/schemas" "$T/fonts" "$T/fontcache" "$T/share/mime" \
  "$T/share/icons/Adwaita" "$T/etc/gdk-pixbuf" "$T/glycin" "$T/share/glycin-loaders/2+/conf.d" "$T/libexec" "$T/share/glvnd/egl_vendor.d"
for f in resources.pak icudtl.dat v8_context_snapshot.bin locales/en-US.pak lib/libgtk-3.so.0 lib/libsoftokn3.so \
    lib/libfreeblpriv3.so etc/fonts/fonts.conf share/glib-2.0/schemas/gschemas.compiled fonts/a.ttf \
    fontcache/x-le64.cache-12 share/mime/mime.cache share/icons/Adwaita/index.theme etc/gdk-pixbuf/loaders.cache \
    glycin/glycin-image-rs share/glycin-loaders/2+/conf.d/glycin-image-rs.conf libexec/bwrap lib/libEGL.so.1 \
    lib/libva.so.2 lib/dri/radeonsi_drv_video.so share/glvnd/egl_vendor.d/50_mesa.json; do : >"$T/$f"; done
printf '#!%s sh\nenv | sort >"%s/env.txt"; echo "$*" >"%s/args.txt"\n' "$BB" "$R" "$R" >"$T/brave"
chmod +x "$T/brave"
echo "/dev/sda1 $R/mnt vfat rw,fmask=0022 0 0" >"$R/mounts"
echo "/dev/nvme0n1p1 $R/data ext4 rw,relatime 0 0" >>"$R/mounts"
sock "$R/xdg/wayland-bits-0"
env -i XDG_RUNTIME_DIR="$R/xdg" "$BB" sleep 120 & desk=$!; pids="$pids $desk"
sleep 0.2
brave() { rm -f "$R/env.txt" "$R/args.txt"; env -i BRAVE_MOUNT="$R/mnt" BRAVE_MOUNTS="$R/mounts" BRAVE_PATH="$R/bin" BRAVE_ALSA_SYS=/nonexistent \
  BRAVE_TMP="$R/tmp" BRAVE_USER_RUN="$R/user" BRAVE_DATA="$R/data" XDG_RUNTIME_DIR="$R/xdg" BRAVE_DATA_STATE="$R/nostate/x" \
  BRAVE_THEME=light FAKE_BITSCOMP="$desk" "$@" LANG=de_DE.UTF-8 PATH="$R/bin" "$BB" sh "$wt/user/ports/brave-launcher/bitsos/brave" >"$R/out" 2>&1; }
envv() { grep -qx -- "$1" "$R/env.txt"; }
brave BRAVE_A11Y_BUS="$BUS"; rc=$?
check "Brave with the bus: started" '[ $rc = 0 ] && [ -s "$R/env.txt" ]'
check "Brave with the bus: AT_SPI_BUS_ADDRESS and ACCESSIBILITY_ENABLED, no NO_AT_BRIDGE" \
  'envv "AT_SPI_BUS_ADDRESS=unix:path=$BUS" && envv ACCESSIBILITY_ENABLED=1 && ! grep -q "^NO_AT_BRIDGE=" "$R/env.txt"'
check "Brave with the bus: --force-renderer-accessibility" 'grep -q -- " --force-renderer-accessibility" "$R/args.txt"'
grep -v '^AT_SPI_BUS_ADDRESS=\|^ACCESSIBILITY_ENABLED=' "$R/env.txt" >"$R/brave-on.txt"
sed 's/ --force-renderer-accessibility//' "$R/args.txt" >"$R/brave-on-args.txt"
brave BRAVE_A11Y_BUS="$R/bus/none"; rc=$?
check "Brave without the bus: NO_AT_BRIDGE=1 as before, no bus, no flag" \
  '[ $rc = 0 ] && envv NO_AT_BRIDGE=1 && ! grep -q "^AT_SPI\|^ACCESSIBILITY" "$R/env.txt" && ! grep -q accessibility "$R/args.txt"'
check "Brave: the rest of the environment and the flags do not change" \
  '[ "$(grep -v "^NO_AT_BRIDGE=" "$R/env.txt")" = "$(cat "$R/brave-on.txt")" ] && cmp -s "$R/args.txt" "$R/brave-on-args.txt"'
mkdir -p "$R/user/at-spi"; sock "$R/user/at-spi/bus"
brave; rc=$?
check "Brave: the bus's default place is the user's runtime folder (at-spi/bus)" '[ $rc = 0 ] && envv "AT_SPI_BUS_ADDRESS=unix:path=$R/user/at-spi/bus"'
rm -rf "$R/user/at-spi"

# LibreOffice --env and Steam --env: the environment the program would get.
lo() { env -i PATH="$R/bin" LO_PATH="$R/bin" LO_USER_RUN="$R/user" LO_TMP="$R/tmp" LO_TREE="$R/lo" LO_MOUNTS=/dev/null \
  LO_DATA="$R/nodata" LO_DATA_STATE="$R/nostate/x" "$@" "$BB" sh "$wt/user/ports/libreoffice-launcher/bitsos/libreoffice" --env >"$R/out" 2>&1; }
lov() { grep -qx -- "$1" "$R/out"; }
lo LO_A11Y_BUS="$BUS"; rc=$?
check "LibreOffice with the bus: AT_SPI_BUS_ADDRESS, no NO_AT_BRIDGE" '[ $rc = 0 ] && lov "AT_SPI_BUS_ADDRESS=unix:path=$BUS" && ! grep -q "^NO_AT_BRIDGE=" "$R/out"'
grep -v '^AT_SPI_BUS_ADDRESS=' "$R/out" >"$R/lo-on.txt"
lo LO_A11Y_BUS="$R/bus/none"; rc=$?
check "LibreOffice without the bus: NO_AT_BRIDGE=1 as before, the rest unchanged" \
  '[ $rc = 0 ] && lov NO_AT_BRIDGE=1 && ! grep -q "^AT_SPI" "$R/out" && [ "$(grep -v "^NO_AT_BRIDGE=" "$R/out")" = "$(cat "$R/lo-on.txt")" ]'

sb() { env -i PATH="$R/bin" SB_PATH="$R/bin" SB_USER_RUN="$R/user" SB_DATA="$R/data" SB_MOUNTS=/dev/null "$@" \
  "$BB" sh "$wt/user/ports/steam/bitsos/steam" --env >"$R/out" 2>&1; }
sb SB_A11Y_BUS="$R/bus/none"; rc=$?
cp "$R/out" "$R/steam-off.txt"
check "Steam without the bus: no accessibility variable" '[ $rc = 0 ] && grep -q "^DISPLAY=:0$" "$R/out" && ! grep -q "^AT_SPI\|^ACCESSIBILITY\|^NO_AT" "$R/out"'
sb SB_A11Y_BUS="$BUS"; rc=$?
check "Steam with the bus: AT_SPI_BUS_ADDRESS and ACCESSIBILITY_ENABLED" \
  '[ $rc = 0 ] && grep -qx "AT_SPI_BUS_ADDRESS=unix:path=$BUS" "$R/out" && grep -qx ACCESSIBILITY_ENABLED=1 "$R/out"'
check "Steam with the bus: the rest unchanged" '[ "$(grep -v "^AT_SPI_BUS_ADDRESS=\|^ACCESSIBILITY_ENABLED=" "$R/out")" = "$(cat "$R/steam-off.txt")" ]'

if [ $fails != 0 ]; then echo "RED: $fails check(s) failed"; exit 1; fi
echo "GREEN: the launchers hand the accessibility bus over only while it runs"
