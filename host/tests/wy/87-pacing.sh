#!/usr/bin/env bash
# WY-20 on the host (headless, nothing on the screen): the beam at `theme.motion medium` draws one frame per refresh.
# `--fake-scanout vsync` is the Mini-PC's shape at a simulated 60 Hz: the back end names the grid, and each flip
# returns just past the next vblank, as V2's FBIOPAN_DISPLAY does. Counts frames with `ctl stats` and proves: the
# medium gets more than 45 frames a second, where the old loop never gets past 30 however fast the renderer is (it
# counted the vblank it had just waited for as missed); each frame starts at its vblank, so the flip's wait takes the
# rest of the refresh (a step counted from the moment of the plan slid frames later: present 7 ms instead of 14); calm
# keeps its own slow step; motion off and a still desktop draw nothing. A window counts as proof at its first pass:
# the fault draws every second refresh in every window, while a loaded machine only makes single windows slow, so
# three windows that all stay below are FAIL, not a retry. Needs the Mesa stage (RED without).
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
cd "$ROOT"
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
export XDG_RUNTIME_DIR=$RUN/xdg
mkdir -m 700 -p "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY
SOCK=wy87-$$

fail() { echo "RED: $*"; tail -5 "$RUN/comp.log" 2>/dev/null || true; exit 1; }

STAGE=${MESA_STAGE:-$ROOT/user/ports/mesa/out/mesa}
if [ ! -f "$STAGE/lib/ld-linux-x86-64.so.2" ]; then
    STAGE=$CARGO_TARGET_DIR/wy-mesa-out/mesa
    if [ ! -f "$STAGE/lib/ld-linux-x86-64.so.2" ]; then
        echo "== stage the Mesa tree into $STAGE"
        MESA_OUT=$CARGO_TARGET_DIR/wy-mesa-out bash user/ports/mesa/scripts/stage-v2.sh >"$RUN/stage.out" 2>&1 \
            || fail "no Mesa stage ($(tail -1 "$RUN/stage.out"))"
    fi
fi
export MESA_STAGE=$STAGE

echo "== build the rescue build and the gpu build for a tree at $RUN/tree"
(cd user/services/bitscomp && cargo build --release -q --offline)
GPU=$RUN/bitscomp-gpu
bash host/tests/wy/tools/gpu-tree.sh "$RUN/tree" "$GPU" "$RUN/tree" >"$RUN/tree.out" 2>&1 || fail "gpu-tree.sh: $(tail -5 "$RUN/tree.out")"

COMP=""
cleanup() { [ -z "$COMP" ] || { kill "$COMP" 2>/dev/null || true; wait "$COMP" 2>/dev/null || true; }; }
trap cleanup EXIT

ctl() { "$BIN" ctl --socket "$SOCK" "$@"; }
stat_of() { ctl stats | tr ' ' '\n' | sed -n "s/^$1=//p"; }
# Frames in a window of $1 seconds.
frames_over() { ctl stats reset >/dev/null; sleep "$1"; stat_of frames; }
# A small picture: the renderer stays well inside a frame here, as the chip's 3 ms do; only the loop is judged.
"$GPU" --renderer gles --fx-on-software --scanout direct --fake-scanout vsync --backend headless --size 640x400 \
    --socket "$SOCK" --fixed-status --mcp none --settings none --installer none >"$RUN/comp.log" 2>&1 &
COMP=$!
for _ in $(seq 100); do grep -q "^WAYLAND_DISPLAY=" "$RUN/comp.log" && break; sleep 0.1; done
grep -q " refresh_us=16666 " "$RUN/comp.log" || fail "the fake's 60 Hz grid is not the output's: $(grep WAYLAND_DISPLAY "$RUN/comp.log")"
grep -q "scanout: direct" "$RUN/comp.log" || fail "not on the direct path"
sleep 3 # the start animation is over

ctl set theme.effects gpu >/dev/null
for _ in $(seq 50); do ctl fx | grep -q "level=gpu" && break; sleep 0.1; done
ctl fx | grep -q "level=gpu" || fail "level gpu not reached: $(ctl fx)"
ctl set theme.motion medium >/dev/null
sleep 1

echo "== medium: one frame per refresh"
good=0
for i in 1 2 3; do
    n=$(frames_over 2)
    p=$(stat_of present_avg_ms)
    echo "window $i: $n frames in 2 s, present_avg_ms=$p render_avg_ms=$(stat_of render_avg_ms)"
    [ "$n" -le 125 ] || fail "medium: $n frames in 2 s, more than the refresh gives"
    python3 -I -c "import sys; sys.exit(0 if $n > 90 and $p > 10 else 1)" && { good=1; break; }
done
[ "$good" = 1 ] || fail "medium at a simulated 60 Hz: last window $n frames in 2 s (every second refresh gives 60), present_avg_ms=$p"

echo "== calm: its own slow step"
ctl set theme.motion calm >/dev/null
sleep 1
n=$(frames_over 2)
echo "calm: $n frames in 2 s"
[ "$n" -ge 15 ] && [ "$n" -le 40 ] || fail "calm: $n frames in 2 s (one per 66 ms gives about 30)"

echo "== off: nothing"
ctl set theme.motion off >/dev/null
sleep 1
n=$(frames_over 2)
[ "$n" -eq 0 ] || fail "motion off: $n frames on a still desktop"
ctl quit >/dev/null
wait "$COMP" 2>/dev/null || true
COMP=""
echo "GREEN: one frame per refresh at a simulated 60 Hz"
