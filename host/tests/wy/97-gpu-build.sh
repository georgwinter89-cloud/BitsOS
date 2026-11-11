#!/usr/bin/env bash
# WY-14 on the host (headless back end, nothing on the screen): bitscomp's gpu build (`--features gpu`, linked
# against the Mesa tree's own glibc) next to the rescue build. The gpu build is linked here for a copy of the staged
# tree on the host (host/tests/wy/tools/gpu-tree.sh; on V2 it is the stick's /boot/BITSOS2/mesa). Proves: every
# library of the gpu build comes from the tree (its loader, DT_RPATH; /proc/PID/maps), libEGL.so.1 of the tree
# loads (egl=loadable), the start line and `ctl stats` say build=gpu renderer=pixman; the same desktop with the
# same client is pixel for pixel the rescue build's picture; `--build gpu --gpu-exe` on the rescue build hands over
# (WY-21: as the child of the waiting rescue build, argv[0] kept; a crash hands the screen back to the rescue build,
# a clean end ends both); a gpu build that is not there keeps the rescue build running; the
# gpu build does not follow `--build rescue`. Fixer (WY-14): the rescue build hands over only after the gpu build's
# `check-build` passed (it loads, makes a renderer, maps no library from outside the tree), so an incomplete tree (a
# library removed: the loader would take the system's), no tree at all (no loader), a garbage file and a
# program that is not a gpu build all keep the rescue build running; CONFIG.TXT `bitscomp=gpu` with CRLF lines
# (`--config`) hands over, a garbage value does not; the gpu build's command name is bitscomp. Needs the Mesa stage
# (made here into the target directory when missing; RED when it cannot be made, the repo's
# convention; WY_STRICT_TREE=1 makes that RED, and RED when the stage itself lacks a library the gpu build links).
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
cd "$ROOT"
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
WYC=$CARGO_TARGET_DIR/release/wyclient
export XDG_RUNTIME_DIR=$RUN/xdg
mkdir -m 700 -p "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY

fail() { echo "FAIL: $*"; exit 1; }

STAGE=${MESA_STAGE:-$ROOT/user/ports/mesa/out/mesa}
if [ -z "${MESA_STAGE:-}" ] && [ ! -d "$STAGE" ]; then
    # A worktree has no out/: the manifest's `tree mesa`, as dr/20 does; an empty answer keeps the path above.
    _m=$(awk '$1 == "tree" && $2 == "mesa" { sub(/^host:/, "", $3); print $3 }' "$ROOT/user/packaging/manifest.txt")
    [ -z "$_m" ] || STAGE=$_m
fi
if [ ! -f "$STAGE/lib/ld-linux-x86-64.so.2" ]; then
    STAGE=$CARGO_TARGET_DIR/wy-mesa-out/mesa
    if [ ! -f "$STAGE/lib/ld-linux-x86-64.so.2" ]; then
        echo "== stage the Mesa tree into $STAGE"
        MESA_OUT=$CARGO_TARGET_DIR/wy-mesa-out bash user/ports/mesa/scripts/stage-v2.sh >"$RUN/stage.out" 2>&1 \
            || { [ "${WY_STRICT_TREE:-0}" = 1 ] && fail "no Mesa stage: $(tail -3 "$RUN/stage.out")"
                 echo "RED: no Mesa stage ($(tail -1 "$RUN/stage.out"))"; exit 1; }
    fi
fi
export MESA_STAGE=$STAGE

echo "== build the rescue build, wyclient, and the gpu build for a tree at $RUN/tree"
(cd user/services/bitscomp && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)
GPU=$RUN/bitscomp-gpu
bash host/tests/wy/tools/gpu-tree.sh "$RUN/tree" "$GPU" "$RUN/tree" | tee "$RUN/tree.out"
[ "${PIPESTATUS[0]}" = 0 ] || fail "gpu-tree.sh"

PIDS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
    for p in "${PIDS[@]}"; do wait "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

started() { # log: waits for the start line
    for _ in $(seq 100); do grep -q "^WAYLAND_DISPLAY=" "$1" && return 0; sleep 0.1; done
    fail "no start line in $1: $(tail -5 "$1")"
}
# One compositor with one wyclient; a screenshot once the picture is still. $1 name, $2.. the program and its args.
desktop() {
    local name=$1 sock=wy97-$1-$$
    shift
    "$@" --backend headless --size 1280x800 --socket "$sock" --fixed-status --mcp none --settings none --installer none \
        >"$RUN/$name.log" 2>&1 &
    local cp=$!
    PIDS+=("$cp")
    started "$RUN/$name.log"
    WAYLAND_DISPLAY=$sock "$WYC" --title one --pattern 0 >"$RUN/$name-client.out" 2>&1 &
    PIDS+=("$!")
    for _ in $(seq 100); do "$BIN" ctl --socket "$sock" list 2>/dev/null | grep -q 'title="one"' && break; sleep 0.1; done
    "$BIN" ctl --socket "$sock" list | grep -q 'title="one"' || fail "$name: the client did not map"
    sleep 3 # the start animation and the window's opening are over
    "$BIN" ctl --socket "$sock" shot "$RUN/$name.ppm" >/dev/null
    "$BIN" ctl --socket "$sock" stats >"$RUN/$name.stats"
    echo "$cp" >"$RUN/$name.pid"
    echo "$sock" >"$RUN/$name.sock"
}
quit() { "$BIN" ctl --socket "$(cat "$RUN/$1.sock")" quit >/dev/null 2>&1 || true; }

echo "== the rescue build"
desktop rescue "$BIN"
grep -q " build=rescue renderer=pixman egl=off$" "$RUN/rescue.log" || fail "rescue start line: $(grep WAYLAND_DISPLAY "$RUN/rescue.log")"
grep -q " build=rescue renderer=pixman" "$RUN/rescue.stats" || fail "rescue ctl stats: $(cat "$RUN/rescue.stats")"
pid=$(cat "$RUN/rescue.pid")
if grep -q "$RUN/tree" "/proc/$pid/maps"; then fail "the rescue build maps something of the tree"; fi
quit rescue

echo "== the gpu build, started directly"
desktop gpu "$GPU"
grep -q " build=gpu renderer=pixman egl=loadable$" "$RUN/gpu.log" || fail "gpu start line: $(grep WAYLAND_DISPLAY "$RUN/gpu.log") $(tail -3 "$RUN/gpu.log")"
grep -q " build=gpu renderer=pixman" "$RUN/gpu.stats" || fail "gpu ctl stats: $(cat "$RUN/gpu.stats")"
pid=$(cat "$RUN/gpu.pid")
libs=$(awk '$6 ~ /\.so(\.[0-9]+)*$/ {print $6}' "/proc/$pid/maps" | sort -u)
outside=$(echo "$libs" | grep -v "^$RUN/tree/lib/" || true)
[ -z "$outside" ] || fail "the gpu build maps libraries outside the tree: $outside"
echo "$libs" | grep -q "/libc.so.6$" || fail "no libc.so.6 among the gpu build's libraries: $libs"
echo "gpu build libraries: $(echo "$libs" | sed 's#.*/##' | tr '\n' ' ')"
[ "$(cat "/proc/$pid/comm")" = bitscomp ] || fail "the gpu build's command name is $(cat "/proc/$pid/comm")"
cmp -s "$RUN/rescue.ppm" "$RUN/gpu.ppm" || fail "the gpu build's picture differs from the rescue build's"
echo "ok: the same picture, byte for byte ($(stat -c %s "$RUN/gpu.ppm") bytes)"
# The gpu build's programs and the page windows start through its own file (the trampoline): still the tree only.
quit gpu

echo "== hand-over: the rescue build with --build gpu"
desktop over "$BIN" --build gpu --gpu-exe "$GPU"
pid=$(cat "$RUN/over.pid")
grep -q "build gpu (--build gpu): handing over to $GPU" "$RUN/over.log" || fail "no hand-over line: $(head -5 "$RUN/over.log")"
# WY-21: the gpu build runs as the child of the rescue build, which waits beside it as bitscomp-rescue.
gpid=$(sed -n 's/^WAYLAND_DISPLAY=.* pid=\([0-9]*\) backend=.*/\1/p' "$RUN/over.log" | head -1)
grep -q " pid=$gpid .* build=gpu renderer=pixman egl=loadable$" "$RUN/over.log" || fail "after the hand-over: $(grep WAYLAND_DISPLAY "$RUN/over.log")"
[ "$(awk '/^PPid:/ {print $2}' "/proc/$gpid/status")" = "$pid" ] || fail "the gpu build $gpid is not the child of the rescue build $pid"
[ "$(readlink "/proc/$gpid/exe")" = "$GPU" ] || fail "pid $gpid runs $(readlink "/proc/$gpid/exe"), not $GPU"
[ "$(readlink "/proc/$pid/exe")" = "$(realpath "$BIN")" ] || fail "the waiting pid $pid runs $(readlink "/proc/$pid/exe")"
[ "$(tr '\0' '\n' <"/proc/$gpid/cmdline" | head -1)" = "$BIN" ] || fail "argv[0] changed: $(tr '\0' ' ' <"/proc/$gpid/cmdline")"
cmp -s "$RUN/rescue.ppm" "$RUN/over.ppm" || fail "the handed-over picture differs from the rescue build's"
grep -q "check-build" "$RUN/over.log" && fail "the check said something on a good tree: $(grep check-build "$RUN/over.log")"
[ "$(cat "/proc/$gpid/comm")" = bitscomp ] || fail "after the hand-over the command name is $(cat "/proc/$gpid/comm")"
[ "$(cat "/proc/$pid/comm")" = bitscomp-rescue ] || fail "the waiting rescue build is named $(cat "/proc/$pid/comm")"
echo "ok: the gpu build $gpid ($(readlink "/proc/$gpid/exe"), argv[0] $BIN, comm bitscomp) under the waiting rescue build $pid, the same picture"
echo "== WY-21: the gpu build crashes (SIGABRT, Mesa's answer to a lost device): the rescue build takes over"
orphan=$("$BIN" ctl --socket "$(cat "$RUN/over.sock")" spawn sleep 300 | sed -n 's/^pid //p')
[ -n "$orphan" ] && kill -0 "$orphan" || fail "the gpu build started no program"
kill -ABRT "$gpid"
for _ in $(seq 100); do grep -q " pid=$pid .* build=rescue renderer=pixman" "$RUN/over.log" && break; sleep 0.1; done
grep -q "the gpu build crashed (signal 6): the rescue build takes the screen, with the session" "$RUN/over.log" || fail "no takeover line: $(tail -5 "$RUN/over.log")"
grep -q " pid=$pid .* build=rescue renderer=pixman egl=off$" "$RUN/over.log" || fail "the rescue build did not start: $(tail -5 "$RUN/over.log")"
[ "$(cat "/proc/$pid/comm")" = bitscomp ] || fail "the rescue build's command name is $(cat "/proc/$pid/comm") after the takeover"
"$BIN" ctl --socket "$(cat "$RUN/over.sock")" stats | grep -q " build=rescue renderer=pixman" || fail "ctl stats after the takeover"
kill -0 "$orphan" 2>/dev/null && fail "the gpu build's program $orphan outlived it: the session would start it a second time"
grep -q "the gpu build left 1 programs: ending them before the session comes back" "$RUN/over.log" || fail "no line for the left programs: $(tail -5 "$RUN/over.log")"
echo "ok: the rescue build $pid draws after the gpu build's crash; the program it left ($orphan) was ended"
quit over
echo "== WY-21: a gpu build that ends cleanly (ctl quit) ends the rescue build too"
desktop clean "$BIN" --build gpu --gpu-exe "$GPU"
quit clean
for _ in $(seq 50); do kill -0 "$(cat "$RUN/clean.pid")" 2>/dev/null || break; sleep 0.1; done
kill -0 "$(cat "$RUN/clean.pid")" 2>/dev/null && fail "the rescue build stays after the gpu build's clean end"
grep -q "takes the screen" "$RUN/clean.log" && fail "a takeover after a clean end: $(tail -5 "$RUN/clean.log")"
echo "ok: a clean end of the gpu build ends both"

echo "== no gpu build there: the rescue build runs on"
desktop missing "$BIN" --build gpu --gpu-exe "$RUN/no-such/bitscomp-gpu"
grep -q "the rescue build runs on" "$RUN/missing.log" || fail "no fallback line: $(head -5 "$RUN/missing.log")"
grep -q " build=rescue renderer=pixman egl=off$" "$RUN/missing.log" || fail "fallback start line: $(grep WAYLAND_DISPLAY "$RUN/missing.log")"
cmp -s "$RUN/rescue.ppm" "$RUN/missing.ppm" || fail "the fallback picture differs"
quit missing

# A case where the gpu build's check must fail: the rescue build names why and runs on, with today's picture.
fallback() { # name, what the log must say, then bitscomp's extra args
    local name=$1 why=$2
    shift 2
    desktop "$name" "$BIN" "$@"
    grep -q "the rescue build runs on" "$RUN/$name.log" || fail "$name: no fallback line: $(head -8 "$RUN/$name.log")"
    grep -q -- "$why" "$RUN/$name.log" || fail "$name: the log does not say '$why': $(head -8 "$RUN/$name.log")"
    grep -q "handing over" "$RUN/$name.log" && fail "$name: handed over: $(head -8 "$RUN/$name.log")"
    grep -q " build=rescue renderer=pixman egl=off$" "$RUN/$name.log" || fail "$name: start line: $(grep WAYLAND_DISPLAY "$RUN/$name.log")"
    [ "$(readlink "/proc/$(cat "$RUN/$name.pid")/exe")" = "$(realpath "$BIN")" ] || fail "$name: not the rescue build"
    cmp -s "$RUN/rescue.ppm" "$RUN/$name.ppm" || fail "$name: the fallback picture differs"
    echo "ok: $name: $(grep 'the rescue build runs on' "$RUN/$name.log" | head -1)"
    quit "$name"
}

echo "== an incomplete tree (libpixman-1.so.0 removed): the loader takes the system's, the check refuses the mix"
mv "$RUN/tree/lib/libpixman-1.so.0" "$RUN/libpixman-1.so.0.off"
fallback incomplete "libraries outside $RUN/tree/lib: .*libpixman-1" --build gpu --gpu-exe "$GPU"
mv "$RUN/libpixman-1.so.0.off" "$RUN/tree/lib/libpixman-1.so.0"

echo "== no tree at all (the gpu build's loader is missing)"
mv "$RUN/tree" "$RUN/tree.off"
fallback notree "check-build: does not start" --build gpu --gpu-exe "$GPU"
mv "$RUN/tree.off" "$RUN/tree"

echo "== a garbage file as the gpu build"
head -c 4096 /dev/urandom >"$RUN/garbage-gpu"
chmod +x "$RUN/garbage-gpu"
fallback garbage "check-build: does not start" --build gpu --gpu-exe "$RUN/garbage-gpu"
: >"$RUN/empty-gpu"
chmod +x "$RUN/empty-gpu"
fallback empty "check-build: " --build gpu --gpu-exe "$RUN/empty-gpu"

echo "== a program that exits 0 but is no gpu build"
fallback notgpu "exit 0 but no answer line" --build gpu --gpu-exe "$(type -P true)"

echo "== CONFIG.TXT written on Windows (CRLF): bitscomp=gpu hands over, a garbage value does not"
printf 'loglevel=warn\r\nbitscomp=gpu\r\n' >"$RUN/crlf.txt"
desktop crlf "$BIN" --config "$RUN/crlf.txt" --gpu-exe "$GPU"
grep -q "build gpu (CONFIG.TXT bitscomp=gpu): handing over to $GPU" "$RUN/crlf.log" || fail "crlf: $(head -5 "$RUN/crlf.log")"
grep -q " build=gpu renderer=pixman egl=loadable$" "$RUN/crlf.log" || fail "crlf start line: $(grep WAYLAND_DISPLAY "$RUN/crlf.log")"
quit crlf
printf 'bitscomp=fast\r\n' >"$RUN/odd.txt"
desktop odd "$BIN" --config "$RUN/odd.txt" --gpu-exe "$GPU"
grep -q 'build rescue (CONFIG.TXT bitscomp="fast" is neither rescue, gpu, gles nor auto)' "$RUN/odd.log" || fail "odd: $(head -5 "$RUN/odd.log")"
grep -q " build=rescue " "$RUN/odd.log" || fail "odd start line: $(grep WAYLAND_DISPLAY "$RUN/odd.log")"
quit odd
echo "ok: CRLF read as gpu, bitscomp=fast stays rescue"

echo "== the gpu build does not follow --build rescue"
desktop norescue "$GPU" --build rescue
grep -q "this is the gpu build: --build rescue is not followed" "$RUN/norescue.log" || fail "no note: $(head -5 "$RUN/norescue.log")"
grep -q " build=gpu " "$RUN/norescue.log" || fail "norescue start line: $(grep WAYLAND_DISPLAY "$RUN/norescue.log")"
quit norescue

if grep -q "KNOWN-GAP" "$RUN/tree.out"; then
    echo "NOTE: $(grep KNOWN-GAP "$RUN/tree.out" | sed 's/^gpu-tree: //')"
fi
echo "GREEN: the gpu build runs on the Mesa tree's glibc and draws what the rescue build draws; every broken case keeps the rescue build"
