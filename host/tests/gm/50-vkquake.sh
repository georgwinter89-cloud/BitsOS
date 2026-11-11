#!/bin/bash
# GM-8: the vkquake launcher (user/ports/vkquake/bitsos/vkquake) on the host, no QEMU, no game: a made-up tree and Mesa tree whose
# "loader" is a script that writes down how it was called. Checks: a missing file FAILs --check with its name, a whole tree passes,
# the start's arguments and clean environment, RADV only by default and no driver path from outside, the pid stays (exec all the
# way), no desktop = no start but the server starts without one, root hands over to uid 1000 or refuses, the home on the data
# volume or in RAM, a RAM folder someone else planted, a device node the game's user cannot open. Then quake_check.py's logic on made-up answers (tools/test_quake_check.py), and the shell scripts' syntax.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT=$PWD
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/gm-50-vkquake}"
rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
L=$ROOT/user/ports/vkquake/bitsos/vkquake
red() { echo "RED: $*"; exit 1; }
for s in "$L" user/ports/vkquake/scripts/stage-v2.sh host/tests/minipc/quake.sh host/tests/gm/51-vkquake-window.sh; do bash -n "$s" || red "syntax of $s"; done
grep -q '^tree  vkquake host:' user/packaging/manifest.txt && grep -q '^stick vkquake file:user/ports/vkquake/bitsos/vkquake' user/packaging/manifest.txt \
  || red "the manifest has no vkquake lines"

T=$RUN_DIR/trees/vkquake M=$RUN_DIR/trees/mesa F=$RUN_DIR/fakebin X=$RUN_DIR/xdg D=$RUN_DIR/dev
mkdir -m 700 -p "$X"
mkdir -p "$D/dri"
mkdir -p "$T/bin" "$T/lib" "$T/id1" "$M/lib" "$M/share/vulkan/icd.d" "$F" "$RUN_DIR/empty"
for f in "$T/bin/vkquake" "$T/lib/libSDL2-2.0.so.0" "$T/lib/libasound.so.2" "$T/id1/pak0.pak" "$T/id1/pak1.pak" "$M/lib/libvulkan.so.1" "$M/lib/libvulkan_radeon.so"; do : >"$f"; done
echo "vkQuake 1.37.0 x" >"$T/VERSION.txt"
echo '{}' >"$M/share/vulkan/icd.d/radeon_icd.json"; echo '{}' >"$M/share/vulkan/icd.d/lvp_icd.json"
# The loader: --list answers like ld.so, a start writes its pid, arguments and environment.
cat >"$M/lib/ld-linux-x86-64.so.2" <<'LD'
#!/bin/sh
case " $* " in *" --list "*) [ -z "$FAKE_MISSING" ] || echo "	$FAKE_MISSING => not found"; exit 0 ;; esac
{ echo "pid=$$ uid=$(id -u)"; echo "args=$*"; env; } >"$(cat "${0%/*}/outpath")"
LD
chmod +x "$M/lib/ld-linux-x86-64.so.2"
echo "$RUN_DIR/game.txt" >"$M/lib/outpath"
# id: FAKE_ID, and 1000 once chpst ran. chpst: as BusyBox's, or broken when FAKE_NOCHPST is set. pidof: no desktop process.
printf '#!/bin/sh\necho "${FAKE_ID:-1000}"\n' >"$F/id"
printf '#!/bin/sh\n[ -z "$FAKE_NOCHPST" ] || { echo "chpst: no such user"; exit 1; }\nshift 2\nFAKE_ID=1000 exec "$@"\n' >"$F/chpst"
printf '#!/bin/sh\nexit 1\n' >"$F/pidof"
chmod +x "$F"/*
python3 -I -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1])' "$X/w0"

# Runs the launcher in a clean environment with the fake trees; its output in $RUN_DIR/out.txt, its exit in $code.
OUT=$RUN_DIR/game.txt
vkq() {
    rm -f "$OUT"
    set +e
    env -i PATH=/usr/bin:/bin VKQ_PATH="$F:/usr/bin:/bin" VKQ_TREE="$T" VKQ_TMP="$RUN_DIR" VKQ_DATA="$RUN_DIR/nodata" VKQ_USER_RUN="$RUN_DIR/user-run" VKQ_DEV="$D" \
        "${ENVS[@]}" sh "$L" "$@" >"$RUN_DIR/out.txt" 2>&1
    code=$?
    set -e
    grep -q "No such file or directory\|not found$\|syntax error" "$RUN_DIR/out.txt" && { cat "$RUN_DIR/out.txt"; red "the launcher's own commands failed: $*"; }
    return 0
}
said() { grep -q -- "$1" "$RUN_DIR/out.txt" || { cat "$RUN_DIR/out.txt"; red "${2:-no '$1' in the output}"; }; }
got() { grep -q -- "$1" "$OUT" || { cat "$OUT"; red "${2:-the game did not get '$1'}"; }; }
DESK=(XDG_RUNTIME_DIR="$X" WAYLAND_DISPLAY=w0)

ENVS=("${DESK[@]}")
vkq --check; [ $code = 0 ] || { cat "$RUN_DIR/out.txt"; red "--check on a whole tree: exit $code"; }
said "tree $T: ok (vkQuake 1.37.0)"; said "resolve inside"; said "drivers: radeon_icd"; said "desktop: $X/w0"; said "user: runs as uid 1000"
[ ! -e "$OUT" ] || red "--check started the game"

mv "$T/id1/pak1.pak" "$RUN_DIR/keep"
vkq --check; [ $code = 1 ] || red "--check without pak1.pak: exit $code"; said 'id1/pak1.pak is missing'
vkq; [ $code = 1 ] && [ ! -e "$OUT" ] || red "a start without pak1.pak: exit $code"
mv "$RUN_DIR/keep" "$T/id1/pak1.pak"
mv "$M/lib/libvulkan_radeon.so" "$RUN_DIR/keep"
vkq --check; [ $code = 1 ] || red "--check without RADV: exit $code"; said 'libvulkan_radeon.so is missing'
mv "$RUN_DIR/keep" "$M/lib/libvulkan_radeon.so"
ENVS=("${DESK[@]}" FAKE_MISSING=libfoo.so.1); vkq --check; [ $code = 1 ] || red "--check with a missing library: exit $code"; said 'missing libfoo.so.1'

# Device nodes: seen is not enough, the game's user must be able to open them. A plain file stands in for the node.
: >"$D/dri/renderD128"
ENVS=("${DESK[@]}"); vkq --check; [ $code = 0 ] || { cat "$RUN_DIR/out.txt"; red "--check with an open node: exit $code"; }; said "render nodes: renderD128"
chmod 000 "$D/dri/renderD128"
if [ ! -r "$D/dri/renderD128" ]; then
    vkq --check; [ $code = 1 ] || red "--check with a node the user cannot open: exit $code"; said "renderD128 cannot be opened"
else
    red "this test cannot take away its own read right (run as root?): the node check is unproven"
fi
rm -f "$D/dri/renderD128"

# The start: arguments, environment, the pid.
ENVS=("${DESK[@]}" LD_PRELOAD=/evil.so VK_ICD_FILENAMES=/evil.json FOO=bar)
env -i PATH=/usr/bin:/bin VKQ_PATH="$F:/usr/bin:/bin" VKQ_TREE="$T" VKQ_TMP="$RUN_DIR" VKQ_DATA="$RUN_DIR/nodata" "${ENVS[@]}" sh "$L" -nosound +map start >"$RUN_DIR/out.txt" 2>&1 &
pid=$!
wait $pid
got "pid=$pid " "the pid changed on the way: the check of the window goes by it"
got "args=--inhibit-cache --library-path $T/lib:$M/lib --argv0 $T/bin/vkquake $T/bin/vkquake -basedir $T -current -nosound +map start"
got "WAYLAND_DISPLAY=w0"; got "XDG_RUNTIME_DIR=$X"; got "SDL_VIDEODRIVER=wayland"; got "SDL_AUDIODRIVER=alsa"
got "VK_DRIVER_FILES=$M/share/vulkan/icd.d/radeon_icd.json$"; got "VK_ICD_FILENAMES=$M/share/vulkan/icd.d/radeon_icd.json$"
got "LD_LIBRARY_PATH=$T/lib:$M/lib$"; got "VK_LAYER_PATH=$M/share/vulkan/explicit_layer.d"
if grep -q "evil\|^FOO=\|^VKQ_" "$OUT"; then cat "$OUT"; red "the caller's environment leaks into the game"; fi

ENVS=("${DESK[@]}" MESA_VK_DRIVERS=lvp_icd); vkq; [ $code = 0 ] || red "lavapipe by name: exit $code"; got "VK_DRIVER_FILES=$M/share/vulkan/icd.d/lvp_icd.json$"
ENVS=("${DESK[@]}" MESA_VK_DRIVERS=../../evil); vkq; [ $code = 1 ] && [ ! -e "$OUT" ] || red "a driver path from outside: exit $code"
ENVS=("${DESK[@]}" MESA_VK_DRIVERS=nothing); vkq; [ $code = 1 ] && [ ! -e "$OUT" ] || red "an unknown driver: exit $code"

# No desktop: the game refuses, the server does not ask for one and gets no -current.
ENVS=(); vkq; [ $code = 1 ] && [ ! -e "$OUT" ] || red "no desktop: exit $code"; said "desktop: none runs"
ENVS=(); vkq -dedicated 1 +quit; [ $code = 0 ] || red "the server without a desktop: exit $code"
got "-basedir $T -dedicated 1 +quit"; if grep -q -- "-current" "$OUT"; then red "the server got -current"; fi
if grep -q "^WAYLAND_DISPLAY" "$OUT"; then red "the server got a desktop"; fi

# Home: RAM without a data volume (said so), the data volume's when it is there and writable.
# A RAM folder under the shared /tmp that someone else made first is refused.
ENVS=(); rm -rf "$RUN_DIR/vkquake-1000"; ln -s "$RUN_DIR/empty" "$RUN_DIR/vkquake-1000"
vkq -dedicated 1 +quit; [ $code = 1 ] && [ ! -e "$OUT" ] || red "a planted RAM folder: exit $code"; said "is not this user's own folder"
[ ! -e "$RUN_DIR/empty/home" ] || red "the home was made through the planted link"
rm -f "$RUN_DIR/vkquake-1000"
ENVS=("${DESK[@]}"); vkq; said "home: $X/vkquake/home (in RAM"; got "HOME=$X/vkquake/home$"; [ -d "$X/vkquake/home" ] || red "the RAM home was not made"
mkdir -p "$RUN_DIR/data/home/1000"
ENVS=("${DESK[@]}" VKQ_DATA="$RUN_DIR/data"); vkq; said "home: $RUN_DIR/data/home/1000 (data volume"; got "HOME=$RUN_DIR/data/home/1000$"
ENVS=("${DESK[@]}" VKQ_HOME="$RUN_DIR/own"); vkq; got "HOME=$RUN_DIR/own$"

# Root: hands over to uid 1000, or refuses.
ENVS=("${DESK[@]}" FAKE_ID=0); vkq; [ $code = 0 ] || { cat "$RUN_DIR/out.txt"; red "root: exit $code"; }; got "uid=1000" "root's start did not run as uid 1000"
ENVS=("${DESK[@]}" FAKE_ID=0 FAKE_NOCHPST=1); vkq; [ $code = 1 ] && [ ! -e "$OUT" ] || red "root without chpst: exit $code"; said "does not run as root"
ENVS=("${DESK[@]}" FAKE_ID=0 FAKE_NOCHPST=1); vkq --check; [ $code = 1 ] || red "--check as root without chpst: exit $code"; said "refused"

# --env prints what a start gets, starts nothing, no test setting in it.
ENVS=("${DESK[@]}"); vkq --env; [ $code = 0 ] && [ ! -e "$OUT" ] || red "--env: exit $code"; said "^SDL_AUDIODRIVER=alsa$"; said "^WAYLAND_DISPLAY=w0$"

python3 -I -B host/tests/gm/tools/test_quake_check.py >"$RUN_DIR/logic.txt" 2>&1 || { grep -E "^FAIL [a-z-]+: exit|^FAIL the|Traceback|Error" "$RUN_DIR/logic.txt" | head -20; red "quake_check.py's logic"; }
echo "gm vkquake: green (launcher on made-up trees, quake_check.py's logic)"
