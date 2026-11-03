#!/usr/bin/env bash
# WY-15 on the host (headless back end, nothing on the screen): the gpu build drawing with GLES (Smithay's
# GlesRenderer on Mesa's surfaceless EGL, llvmpipe forced) and reading the picture back into the buffer the back
# ends copy to fb0. The gpu build is linked for a copy of the staged Mesa tree (tools/gpu-tree.sh), as in 97.
# Proves: `--renderer gles` starts EGL from the tree (start line and `ctl stats` renderer=gles, the log names
# llvmpipe, every mapped library from the tree: libEGL, Mesa's vendor library, libgallium, LLVM); the same desktop
# as the Pixman picture within the screenshot tolerance (img.py diff, TOL 24, at most 0.3 % of the pixels), also
# after a mode change (a new EGL context) and with a window moved (damage readback of a part of the picture); the
# rescue build hands over for `--build gles` and for CONFIG.TXT `bitscomp=gles` (then with `--renderer gles`
# added); a program bitscomp starts gets none of the EGL variables; a tree whose EGL vendor file is missing makes
# the gpu build draw with Pixman (byte for byte the rescue picture) and the rescue build keep running when it
# would hand over. Prints the render cost per frame of both renderers here (numbers only; QEMU and the Mini-PC
# measure their own). Needs the Mesa stage ("SKIP:" and exit 0 when it cannot be made, as 97).
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
cd "$ROOT"
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
WYC=$CARGO_TARGET_DIR/release/wyclient
IMG="python3 host/tests/wy/tools/img.py"
export XDG_RUNTIME_DIR=$RUN/xdg
mkdir -m 700 -p "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY

fail() { echo "FAIL: $*"; exit 1; }

STAGE=${MESA_STAGE:-$ROOT/user/ports/mesa/out/mesa}
if [ ! -f "$STAGE/lib/ld-linux-x86-64.so.2" ]; then
    STAGE=$CARGO_TARGET_DIR/wy-mesa-out/mesa
    if [ ! -f "$STAGE/lib/ld-linux-x86-64.so.2" ]; then
        echo "== stage the Mesa tree into $STAGE"
        MESA_OUT=$CARGO_TARGET_DIR/wy-mesa-out bash user/ports/mesa/scripts/stage-v2.sh >"$RUN/stage.out" 2>&1 \
            || { echo "SKIP: not run: no Mesa stage ($(tail -1 "$RUN/stage.out"))"; exit 0; }
    fi
fi
export MESA_STAGE=$STAGE

echo "== build the rescue build, wyclient, and the gpu build for a tree at $RUN/tree"
(cd user/services/bitscomp && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)
GPU=$RUN/bitscomp-gpu
bash host/tests/wy/tools/gpu-tree.sh "$RUN/tree" "$GPU" "$RUN/tree" >"$RUN/tree.out" 2>&1 || fail "gpu-tree.sh: $(tail -5 "$RUN/tree.out")"

PIDS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
    for p in "${PIDS[@]}"; do wait "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

started() { # log
    for _ in $(seq 300); do grep -q "^WAYLAND_DISPLAY=" "$1" && return 0; sleep 0.1; done
    fail "no start line in $1: $(tail -5 "$1")"
}
ctl() { "$BIN" ctl --socket "$(cat "$RUN/$1.sock")" "${@:2}"; }
# One compositor with one wyclient; a screenshot once the picture is still. $1 name, $2.. the program and its args.
desktop() {
    local name=$1 sock=wy98-$1-$$
    shift
    "$@" --backend headless --size 1280x800 --socket "$sock" --fixed-status --mcp none --settings none --installer none \
        >"$RUN/$name.log" 2>&1 &
    local cp=$!
    PIDS+=("$cp")
    echo "$cp" >"$RUN/$name.pid"
    echo "$sock" >"$RUN/$name.sock"
    started "$RUN/$name.log"
    WAYLAND_DISPLAY=$sock "$WYC" --title one --pattern 0 >"$RUN/$name-client.out" 2>&1 &
    PIDS+=("$!")
    for _ in $(seq 100); do ctl "$name" list 2>/dev/null | grep -q 'title="one"' && break; sleep 0.1; done
    ctl "$name" list | grep -q 'title="one"' || fail "$name: the client did not map"
    sleep 3 # the start animation and the window's opening are over
    ctl "$name" shot "$RUN/$name.ppm" >/dev/null
    ctl "$name" stats >"$RUN/$name.stats"
}
quit() { ctl "$1" quit >/dev/null 2>&1 || true; }
same() { # A B what: within the screenshot tolerance
    $IMG diff "$1" "$2" >"$RUN/diff.out" 2>&1 || fail "$3: the GLES picture differs from Pixman's: $(cat "$RUN/diff.out")"
    echo "ok: $3: $(cat "$RUN/diff.out")"
}
cost() { # stats file: render ms and damaged megapixels per frame
    echo "$(sed -n 's/.*render_avg_ms=\([0-9.]*\).*/\1/p' "$1") ms ($(sed -n 's/.*frames=\([0-9]*\) .*/\1/p' "$1") frames, $(sed -n 's/.*damaged_mpx_avg=\([0-9.]*\).*/\1/p' "$1") Mpx damaged each)"
}
# Twenty maximise toggles (animated frames with damage over parts of the picture), then the stats; both renderers
# get the same sequence, so their pictures afterwards must agree.
churn() { # name
    local i
    for i in $(seq 10); do ctl "$1" max >/dev/null; sleep 0.4; ctl "$1" max >/dev/null; sleep 0.4; done
    sleep 1
    ctl "$1" stats >"$RUN/$1.churn"
}

echo "== Pixman (the rescue build): the reference picture"
desktop pixman "$BIN"
grep -q " build=rescue renderer=pixman egl=off$" "$RUN/pixman.log" || fail "rescue start line: $(grep WAYLAND_DISPLAY "$RUN/pixman.log")"
churn pixman
ctl pixman shot "$RUN/pixman-churn.ppm" >/dev/null
ctl pixman size 1024 768 >/dev/null
sleep 1
ctl pixman shot "$RUN/pixman-1024.ppm" >/dev/null
quit pixman

echo "== the gpu build with --renderer gles"
desktop gles "$GPU" --renderer gles
grep -q " build=gpu renderer=gles egl=loadable$" "$RUN/gles.log" || fail "gles start line: $(grep WAYLAND_DISPLAY "$RUN/gles.log") $(tail -3 "$RUN/gles.log")"
grep -q "renderer gles: GL_RENDERER=llvmpipe" "$RUN/gles.log" || fail "the log does not name llvmpipe: $(grep -i 'renderer' "$RUN/gles.log")"
grep -q " build=gpu renderer=gles$" "$RUN/gles.stats" || fail "gles ctl stats: $(cat "$RUN/gles.stats")"
pid=$(cat "$RUN/gles.pid")
libs=$(awk '$6 ~ /\.so(\.[0-9]+)*$/ {print $6}' "/proc/$pid/maps" | sort -u)
outside=$(echo "$libs" | grep -v "^$RUN/tree/lib/" || true)
[ -z "$outside" ] || fail "the gles build maps libraries outside the tree: $outside"
for so in libEGL.so.1 libEGL_mesa.so.0 libgallium; do
    echo "$libs" | grep -q "/$so" || fail "$so is not mapped: $(echo "$libs" | sed 's#.*/##' | tr '\n' ' ')"
done
echo "ok: $(grep -o 'GL_RENDERER=[^;]*' "$RUN/gles.log"); $(echo "$libs" | wc -l) libraries, all from the tree"
same "$RUN/pixman.ppm" "$RUN/gles.ppm" "the first picture"
# A started program never sees the EGL variables (they name the tree, for bitscomp's glibc only).
child=$(ctl gles spawn sleep 30 | sed -n 's/^pid //p')
[ -n "$child" ] || fail "spawn gave no pid"
sleep 0.5
for v in __EGL_VENDOR_LIBRARY_FILENAMES LIBGL_DRIVERS_PATH GBM_BACKENDS_PATH DRIRC_CONFIGDIR LIBGL_ALWAYS_SOFTWARE MESA_SHADER_CACHE_DISABLE; do
    if tr '\0' '\n' <"/proc/$child/environ" | grep -q "^$v="; then fail "the started program inherited $v"; fi
done
grep -q "^WAYLAND_DISPLAY=" <(tr '\0' '\n' <"/proc/$child/environ") || fail "the started program has no WAYLAND_DISPLAY"
kill "$child" 2>/dev/null || true
echo "ok: a started program has none of the EGL variables"
churn gles
ctl gles shot "$RUN/gles-churn.ppm" >/dev/null
same "$RUN/pixman-churn.ppm" "$RUN/gles-churn.ppm" "after twenty maximise toggles (partial readback)"
ctl gles size 1024 768 >/dev/null
sleep 1
ctl gles shot "$RUN/gles-1024.ppm" >/dev/null
same "$RUN/pixman-1024.ppm" "$RUN/gles-1024.ppm" "after the mode change to 1024x768 (a new EGL context)"
# Three more round trips: each makes a new compositor (a new EGL context) while the old one lives, then drops the
# old one; the shared EGL display must survive that (render_gl.rs, create), and the picture must keep drawing.
for i in 1 2 3; do
    ctl gles size 1280 800 >/dev/null
    sleep 0.7
    ctl gles size 1024 768 >/dev/null
    sleep 0.7
done
sleep 0.6
ctl gles shot "$RUN/gles-1024b.ppm" >/dev/null
same "$RUN/pixman-1024.ppm" "$RUN/gles-1024b.ppm" "after three more mode round trips (seven new EGL contexts)"
kill -0 "$(cat "$RUN/gles.pid")" 2>/dev/null || fail "gles: bitscomp ended during the mode changes: $(tail -5 "$RUN/gles.log")"
grep -q "drawing with pixman\|render:" "$RUN/gles.log" && fail "gles: a fallback or a skipped frame: $(grep 'drawing with pixman\|render:' "$RUN/gles.log")"
quit gles
echo "render cost per frame here (host, $(nproc) CPUs; numbers only; gles = draw + readback): pixman $(cost "$RUN/pixman.churn"), gles $(cost "$RUN/gles.churn")"

echo "== hand-over: the rescue build with --build gles"
desktop over "$BIN" --build gles --gpu-exe "$GPU"
pid=$(cat "$RUN/over.pid")
grep -q "build gpu, renderer gles (--build gles): handing over to $GPU" "$RUN/over.log" || fail "no hand-over line: $(head -5 "$RUN/over.log")"
grep -q " pid=$pid .* build=gpu renderer=gles egl=loadable$" "$RUN/over.log" || fail "after the hand-over: $(grep WAYLAND_DISPLAY "$RUN/over.log")"
[ "$(readlink "/proc/$pid/exe")" = "$GPU" ] || fail "pid $pid runs $(readlink "/proc/$pid/exe")"
same "$RUN/pixman.ppm" "$RUN/over.ppm" "after the hand-over"
quit over

echo "== CONFIG.TXT bitscomp=gles (CRLF): the hand-over adds --renderer gles"
printf 'loglevel=warn\r\nbitscomp=gles\r\n' >"$RUN/gles.txt"
desktop conf "$BIN" --config "$RUN/gles.txt" --gpu-exe "$GPU"
pid=$(cat "$RUN/conf.pid")
grep -q "build gpu, renderer gles (CONFIG.TXT bitscomp=gles): handing over to $GPU" "$RUN/conf.log" || fail "conf: $(head -5 "$RUN/conf.log")"
grep -q " build=gpu renderer=gles egl=loadable$" "$RUN/conf.log" || fail "conf start line: $(grep WAYLAND_DISPLAY "$RUN/conf.log")"
tr '\0' ' ' <"/proc/$pid/cmdline" | grep -q -- "--renderer gles" || fail "conf: no --renderer gles in $(tr '\0' ' ' <"/proc/$pid/cmdline")"
quit conf
printf 'bitscomp=gpu\n' >"$RUN/gpu.txt"
desktop confgpu "$BIN" --config "$RUN/gpu.txt" --gpu-exe "$GPU"
grep -q " build=gpu renderer=pixman egl=loadable$" "$RUN/confgpu.log" || fail "bitscomp=gpu must stay on pixman: $(grep WAYLAND_DISPLAY "$RUN/confgpu.log")"
cmp -s "$RUN/pixman.ppm" "$RUN/confgpu.ppm" || fail "bitscomp=gpu: not the Pixman picture"
quit confgpu
echo "ok: bitscomp=gles hands over with --renderer gles, bitscomp=gpu stays on Pixman"

echo "== no EGL vendor file in the tree: GLES cannot start"
mv "$RUN/tree/share/glvnd/egl_vendor.d/50_mesa.json" "$RUN/50_mesa.json.off"
desktop novendor "$GPU" --renderer gles
grep -q "renderer gles: .*; drawing with pixman" "$RUN/novendor.log" || fail "novendor: no fallback line: $(head -8 "$RUN/novendor.log")"
grep -q " build=gpu renderer=pixman egl=loadable$" "$RUN/novendor.log" || fail "novendor start line: $(grep WAYLAND_DISPLAY "$RUN/novendor.log")"
cmp -s "$RUN/pixman.ppm" "$RUN/novendor.ppm" || fail "novendor: not the Pixman picture"
echo "ok: $(grep 'drawing with pixman' "$RUN/novendor.log" | head -1)"
quit novendor
desktop nocheck "$BIN" --build gles --gpu-exe "$GPU"
grep -q "check-build: .*the rescue build runs on" "$RUN/nocheck.log" || fail "nocheck: $(head -8 "$RUN/nocheck.log")"
grep -q " build=rescue renderer=pixman egl=off$" "$RUN/nocheck.log" || fail "nocheck start line: $(grep WAYLAND_DISPLAY "$RUN/nocheck.log")"
cmp -s "$RUN/pixman.ppm" "$RUN/nocheck.ppm" || fail "nocheck: not the Pixman picture"
echo "ok: $(grep 'the rescue build runs on' "$RUN/nocheck.log" | head -1)"
quit nocheck
mv "$RUN/50_mesa.json.off" "$RUN/tree/share/glvnd/egl_vendor.d/50_mesa.json"

echo "== the rescue build asked for gles alone stays on pixman when the gpu build is missing"
desktop nogpu "$BIN" --renderer gles --gpu-exe "$RUN/no-such/bitscomp-gpu"
grep -q "the rescue build runs on" "$RUN/nogpu.log" || fail "nogpu: $(head -5 "$RUN/nogpu.log")"
grep -q " build=rescue renderer=pixman egl=off$" "$RUN/nogpu.log" || fail "nogpu start line: $(grep WAYLAND_DISPLAY "$RUN/nogpu.log")"
quit nogpu

echo "GREEN: GLES on llvmpipe draws the Pixman picture within tolerance and reads it back to the fb0 path; every failure keeps Pixman"
