#!/bin/bash
# BR-3: the host's picture of a test page, made the way V2 makes it (10-start.sh compares the two): bitscomp
# headless at 1280x800 (never a window on the desktop), the staged Brave tree (user/ports/brave-launcher/scripts/
# stage-v2.sh: the same binary, libraries, fonts and fontconfig rules the stick carries), the launcher's flags and
# environment (run_brave in user/ports/brave-launcher/bitsos/brave), a fresh profile, the window maximised.
#   reference.sh OUT HTTP_PORT URL MARK [Brave arguments...]
# Writes OUT/shot.ppm (the screen once MARK is in the window title and the picture stood still), OUT/list.txt,
# OUT/bitscomp.log, OUT/brave.log. Exit 0 = picture made. The HTTP server on 127.0.0.1:HTTP_PORT is the caller's.
set -uo pipefail
OUT=$1 PORT=$2 URL=$3 MARK=$4
shift 4
ROOT="$(cd "$(dirname "$0")/../../../.." && pwd)"
STAGE=${BRAVE_STAGE:-/var/tmp/claude-v2/stage/brave}
BIN=${BITSCOMP_BIN:-${CARGO_TARGET_DIR:-$ROOT/target}/release/bitscomp}
LAUNCHER=$ROOT/user/ports/brave-launcher/bitsos/brave
SOCK=br3-ref
[ -x "$STAGE/brave" ] || { echo "FAIL: no staged Brave in $STAGE (bash user/ports/brave-launcher/scripts/stage-v2.sh)"; exit 1; }
[ -x "$BIN" ] || { echo "FAIL: no bitscomp at $BIN"; exit 1; }
# A Unix socket path has at most 107 bytes (SUN_LEN): $OUT/xdg/br3-ref.ctl must fit.
[ ${#OUT} -le 80 ] || { echo "FAIL: $OUT is too long for the compositor's socket path (at most 80 bytes)"; exit 1; }
rm -rf "$OUT"
mkdir -m 700 -p "$OUT/xdg" "$OUT/home"
# The launcher's flags, read from the launcher itself so the two never drift apart.
FLAGS=$(sed -n '/^FLAGS="/,/"$/p' "$LAUNCHER" | tr '\n' ' ' | sed 's/^FLAGS="//; s/" *$//')
[ -n "$FLAGS" ] || { echo "FAIL: no FLAGS= in $LAUNCHER"; exit 1; }
# The stick's fontconfig with the stage's paths; its cache goes to OUT, never into the stage (which is the stick's
# copy: the prebuilt cache names the stick's path, so fontconfig rescans here and would write one more file).
sed "s#<cachedir>/boot/BITSOS2/brave/fontcache</cachedir>#<cachedir>$OUT/fontcache</cachedir>#; s#/boot/BITSOS2/brave#$STAGE#g" \
    "$STAGE/etc/fonts/fonts.conf" > "$OUT/fonts.conf"
grep -qF "<cachedir>$OUT/fontcache</cachedir>" "$OUT/fonts.conf" || { echo "FAIL: the stage's fonts.conf has no cachedir to move"; exit 1; }

PG="" COMP=""
cleanup() {
    if [ -n "$PG" ]; then
        kill -TERM -- "-$PG" 2>/dev/null || true
        for _ in $(seq 50); do kill -0 -- "-$PG" 2>/dev/null || break; sleep 0.2; done
        kill -KILL -- "-$PG" 2>/dev/null || true
    fi
    [ -n "$COMP" ] && kill "$COMP" 2>/dev/null || true
    wait 2>/dev/null || true
    rm -rf "$OUT/home" "$OUT/profile" "$OUT/cache"
}
trap cleanup EXIT

XDG_RUNTIME_DIR=$OUT/xdg "$BIN" --backend headless --size 1280x800 --socket "$SOCK" >"$OUT/bitscomp.log" 2>&1 &
COMP=$!
for _ in $(seq 50); do [ -S "$OUT/xdg/$SOCK.ctl" ] && break; sleep 0.2; done
ctl() { XDG_RUNTIME_DIR=$OUT/xdg "$BIN" ctl --socket "$SOCK" "$@"; }

# shellcheck disable=SC2086
setsid bash -c 'echo $$ >"$0"; exec "$@"' "$OUT/pgid" env -i PATH=/usr/bin HOME="$OUT/home" LANG=de_DE.UTF-8 \
    XDG_RUNTIME_DIR="$OUT/xdg" WAYLAND_DISPLAY="$SOCK" LD_LIBRARY_PATH="$STAGE/lib" \
    FONTCONFIG_FILE="$OUT/fonts.conf" XCURSOR_PATH="$STAGE/share/icons" XCURSOR_THEME=Adwaita \
    XDG_DATA_DIRS="$STAGE/share:/usr/share" GSETTINGS_SCHEMA_DIR="$STAGE/share/glib-2.0/schemas" GSETTINGS_BACKEND=memory \
    GTK_USE_PORTAL=0 NO_AT_BRIDGE=1 DBUS_SESSION_BUS_ADDRESS=disabled: DBUS_SYSTEM_BUS_ADDRESS=disabled: \
    PULSE_SERVER=none "$STAGE/brave" $FLAGS --user-data-dir="$OUT/profile" --disk-cache-dir="$OUT/cache" \
    "$@" "$URL" >"$OUT/brave.log" 2>&1 &
for _ in $(seq 50); do [ -s "$OUT/pgid" ] && break; sleep 0.1; done
PG=$(cat "$OUT/pgid" 2>/dev/null || true)
[ -n "$PG" ] || { echo "FAIL: no process group id from setsid"; exit 1; }

ok=0
for _ in $(seq 60); do
    if ctl list 2>/dev/null | grep -qF "$MARK"; then ok=1; break; fi
    sleep 1
done
[ "$ok" = 1 ] || { echo "FAIL: '$MARK' never in the host's window title"; ctl list; exit 1; }
ctl max >/dev/null
# The picture stands still: two shots a second apart are the same file.
prev=""
for i in $(seq 20); do
    sleep 1
    ctl shot "$OUT/shot$i.ppm" >/dev/null 2>&1 || continue
    if [ -n "$prev" ] && cmp -s "$prev" "$OUT/shot$i.ppm"; then cp "$OUT/shot$i.ppm" "$OUT/shot.ppm"; break; fi
    prev=$OUT/shot$i.ppm
done
rm -f "$OUT"/shot[0-9]*.ppm
ctl list >"$OUT/list.txt" 2>&1
[ -s "$OUT/shot.ppm" ] || { echo "FAIL: the host's picture never stood still"; exit 1; }
echo "reference: $OUT/shot.ppm ($(grep -o 'title="[^"]*"' "$OUT/list.txt" | head -1))"
