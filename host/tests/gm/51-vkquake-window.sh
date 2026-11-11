#!/bin/bash
# slow
# GM-8: the staged tree itself on the build host, never in QEMU (Georg 30.10.) and never on the host's screen, GPU or speakers:
# the tree's files, `V2_STICK_TREES=vkquake` puts it on an image with its list, `vkquake --check`, the server loads map start,
# then the game on lavapipe (MESA_VK_DRIVERS=lvp_icd, so only the CPU driver is loaded) with -nosound in a headless bitscomp at
# 1280x800, whose scale is not 1: the window is fullscreen, visible and focused, the game draws the output's real pixels, the
# picture moves, `ctl close` ends it with exit 0. What only the chip can say is host/tests/minipc/quake.sh.
# Needs: bash user/ports/vkquake/scripts/stage-v2.sh (without the stage this is RED, never left out).
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT=$PWD
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/gm-51-vkquake-window}"
rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-32000}"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
STAGE=${V2_STAGE:-/var/tmp/claude-v2/stage}
T=$STAGE/vkquake
L=$ROOT/user/ports/vkquake/bitsos/vkquake
NAME=gm-vkq
red() { echo "RED: $*"; exit 1; }
[ -f "$T/bin/vkquake" ] && [ -f "$STAGE/mesa/run" ] || red "no $T or $STAGE/mesa: bash user/ports/mesa/scripts/stage-stick.sh, bash user/ports/vkquake/scripts/stage-v2.sh"

# The tree against the stage script's pins: an older stage says so.
S=user/ports/vkquake/scripts/stage-v2.sh
for p in "vkQuake $(sed -n 's/^VKQ_V=\([^ ]*\).*/\1/p' "$S")" "SDL2 $(sed -n 's/^SDL_V=\([^ ]*\).*/\1/p' "$S")" "LibreQuake $(sed -n 's/^LQ_V=\([^ ]*\).*/\1/p' "$S")" "sdl2-highdpi.patch"; do
    grep -q "$p" "$T/VERSION.txt" || red "$T/VERSION.txt does not say '$p': stage again"
done
for f in bin/vkquake lib/libSDL2-2.0.so.0 lib/libasound.so.2 lib/libwayland-cursor.so.0 lib/libvorbisfile.so.3 id1/pak0.pak id1/pak1.pak id1/music/track02.ogg \
    PACKAGES.txt LICENSES/vkquake/LICENSE.txt LICENSES/sdl2/LICENSE.txt LICENSES/librequake/COPYING-BSD3 LICENSES/librequake/COPYING-GPL2; do
    [ -f "$T/$f" ] || red "the tree has no $f"
done
[ -z "$(find "$T" -type l)" ] || red "the tree holds links (FAT has none)"
[ -z "$(cd "$T" && find . | tr '[:upper:]' '[:lower:]' | sort | uniq -d)" ] || red "names that FAT folds together"
head -c 4 "$T/id1/pak0.pak" | grep -q PACK || red "id1/pak0.pak is no pak"
git ls-files --error-unmatch user/ports/vkquake/out >/dev/null 2>&1 && red "the game data or the build is in git"
readelf -l "$T/bin/vkquake" | grep -q "interpreter: /boot/BITSOS2/mesa/lib/ld-linux-x86-64.so.2" || red "the program's PT_INTERP is not the Mesa tree's"
for so in libpulse libpipewire libX11 libudev libdbus libdecor; do
    if readelf -d "$T/lib/libSDL2-2.0.so.0" | grep -q "NEEDED.*$so" || strings "$T/lib/libSDL2-2.0.so.0" | grep -q "^$so.*\.so"; then red "SDL2 uses $so"; fi
done

# As the other trees: only an image that names it carries it, with its own list.
V2_STICK_TREES=vkquake cargo xtask image --name "$NAME" --config-extra "token=gm8;mcp_port=8765;loglevel=warn;screen=1280x800" >"$RUN_DIR/image.txt" 2>&1 || { tail -20 "$RUN_DIR/image.txt"; red "cargo xtask image"; }
ESP=$CARGO_TARGET_DIR/xtask/esp-$NAME
trap 'rm -rf "$ESP" "$CARGO_TARGET_DIR/xtask/$NAME.img"' EXIT
[ -f "$ESP/BITSOS2/vkquake/bin/vkquake" ] && [ -f "$ESP/BITSOS2/vkquake/id1/pak0.pak" ] && [ -f "$ESP/BITSOS2/bin/vkquake" ] || red "V2_STICK_TREES=vkquake did not put the tree and its launcher on the image"
[ ! -e "$ESP/BITSOS2/brave" ] || red "a tree nobody named is on the image"
grep -q "/boot/BITSOS2/vkquake/id1/pak0.pak" "$ESP/BITSOS2/vkquake/PKGLIST.TXT" || red "the tree's own list does not name pak0.pak"
if find "$ESP/BITSOS2/vkquake/lib" -name 'libc.so.6' | grep -q .; then red "the host's glibc came into the tree (it runs on the Mesa tree's)"; fi
rm -rf "$ESP" "$CARGO_TARGET_DIR/xtask/$NAME.img"
trap - EXIT

# The clean start: nothing of this shell's desktop, so nothing can reach the host's own screen.
XDG=$(mktemp -d)
COMP=""
GAME=""
cleanup() {
    [ -z "$GAME" ] || kill "$GAME" 2>/dev/null || true
    [ -z "$COMP" ] || kill "$COMP" 2>/dev/null || true
    wait 2>/dev/null || true
    rm -rf "$XDG" "$RUN_DIR/home"
}
trap cleanup EXIT
mkdir -p "$RUN_DIR/home"
# VKQ_DEV: --check does not look at this machine's own device nodes (Georg's input devices among them).
vkq() { env -i PATH=/usr/bin:/bin VKQ_PATH=/usr/bin:/bin VKQ_TREE="$T" VKQ_MESA="$STAGE/mesa" VKQ_HOME="$RUN_DIR/home" VKQ_DEV="$RUN_DIR/no-dev" MESA_VK_DRIVERS=lvp_icd "$@"; }

vkq sh "$L" --check >"$RUN_DIR/check.txt" 2>&1 || { cat "$RUN_DIR/check.txt"; red "vkquake --check"; }
grep -q "resolve inside" "$RUN_DIR/check.txt" || red "--check names no libraries"
timeout 120 env -i PATH=/usr/bin:/bin VKQ_PATH=/usr/bin:/bin VKQ_TREE="$T" VKQ_MESA="$STAGE/mesa" VKQ_HOME="$RUN_DIR/home" sh "$L" -dedicated 1 -ip 127.0.0.1 -port $((LO + 90)) +map start +status +quit >"$RUN_DIR/server.txt" 2>&1 \
    || { tail -20 "$RUN_DIR/server.txt"; red "the server"; }
for w in "Using SDL version 2.32.10" "version: vkQuake 1.37.0" "map:     start"; do grep -q "$w" "$RUN_DIR/server.txt" || { tail -20 "$RUN_DIR/server.txt"; red "the server: no '$w'"; }; done

(cd user/services/bitscomp && cargo build --release -q --offline) >"$RUN_DIR/bitscomp-build.txt" 2>&1 || { tail -20 "$RUN_DIR/bitscomp-build.txt"; red "bitscomp's build"; }
BIN=$CARGO_TARGET_DIR/release/bitscomp
SOCK=gm8
env -i PATH=/usr/bin:/bin XDG_RUNTIME_DIR="$XDG" "$BIN" --backend headless --size 1280x800 --socket "$SOCK" >"$RUN_DIR/bitscomp.log" 2>&1 &
COMP=$!
for _ in $(seq 100); do [ -S "$XDG/$SOCK.ctl" ] && break; sleep 0.2; done
[ -S "$XDG/$SOCK.ctl" ] || { tail "$RUN_DIR/bitscomp.log"; red "bitscomp did not start"; }
ctl() { env -i XDG_RUNTIME_DIR="$XDG" "$BIN" ctl --socket "$SOCK" "$@"; }
# Not through vkq(): a function behind & is a shell of its own, and $! must be the game's pid.
env -i PATH=/usr/bin:/bin VKQ_PATH=/usr/bin:/bin VKQ_TREE="$T" VKQ_MESA="$STAGE/mesa" VKQ_HOME="$RUN_DIR/home" MESA_VK_DRIVERS=lvp_icd XDG_RUNTIME_DIR="$XDG" WAYLAND_DISPLAY="$SOCK" \
    sh "$L" -nosound -noudp +map start +vid_describecurrentmode >"$RUN_DIR/game.txt" 2>&1 &
GAME=$!
# A bound of a stuck program, no timing. The game's own lines come when it ends (stdout into a file or pipe is buffered).
line=""
for _ in $(seq 180); do
    kill -0 "$GAME" 2>/dev/null || { tail -20 "$RUN_DIR/game.txt"; red "the game ended by itself"; }
    line=$(ctl list | grep " pid=$GAME " || true)
    case $line in *" fullscreen "*) break ;; esac
    line=""
    sleep 1
done
[ -n "$line" ] || { ctl list; grep -i fullscreen "$RUN_DIR/bitscomp.log" || true; red "no fullscreen window with the game's pid within 180 s"; }
for w in "visible=true" "focused=true" "app_id=vkquake" "hung=false"; do
    case $line in *"$w"*) ;; *) red "the window is not '$w': $line" ;; esac
done
# The map may still load: the picture moves within the bound.
moved=""
for _ in $(seq 60); do
    ctl shot "$RUN_DIR/a.ppm" >/dev/null; sleep 1; ctl shot "$RUN_DIR/b.ppm" >/dev/null
    if ! cmp -s "$RUN_DIR/a.ppm" "$RUN_DIR/b.ppm"; then moved=1; break; fi
done
ctl stats >"$RUN_DIR/stats0.txt"; sleep 3; ctl stats >"$RUN_DIR/stats1.txt"
grep -q "output=1280x800" "$RUN_DIR/stats1.txt" || red "the output's size changed: $(cat "$RUN_DIR/stats1.txt")"
c0=$(sed -n 's/.* commits=\([0-9]*\).*/\1/p' "$RUN_DIR/stats0.txt"); c1=$(sed -n 's/.* commits=\([0-9]*\).*/\1/p' "$RUN_DIR/stats1.txt")
[ "$c1" -gt "$c0" ] || red "no commits in 3 s ($c0 -> $c1)"
[ -n "$moved" ] || red "the picture stands still"
wid=$(echo "$line" | sed -n 's/.* id=\([0-9]*\) app_id=.*/\1/p')
ctl "close $wid" >/dev/null
set +e
for _ in $(seq 60); do kill -0 "$GAME" 2>/dev/null || break; sleep 1; done
kill -0 "$GAME" 2>/dev/null && red "the game does not end on ctl close"
wait "$GAME"; rc=$?
set -e
GAME=""
[ "$rc" = 0 ] || { tail -20 "$RUN_DIR/game.txt"; red "the game ended with $rc on ctl close"; }
if ctl list | grep -q " id=$wid "; then red "the window stayed"; fi
grep -q "^Device: llvmpipe" "$RUN_DIR/game.txt" || { grep -n "Device" "$RUN_DIR/game.txt"; red "the game does not name lavapipe: did it touch the host's GPU?"; }
grep -q "^1280x800x[0-9]* .*fullscreen" "$RUN_DIR/game.txt" || { tail -20 "$RUN_DIR/game.txt"; red "the game does not draw the output's 1280x800"; }
rm -f "$RUN_DIR"/a.ppm "$RUN_DIR"/b.ppm
echo "gm vkquake window: green (tree, image, server, lavapipe fullscreen in a headless bitscomp: $(grep -m1 '^1280x800x' "$RUN_DIR/game.txt"), $((c1 - c0)) commits in 3 s, close exit 0)"
