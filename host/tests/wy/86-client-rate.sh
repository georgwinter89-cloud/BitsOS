#!/usr/bin/env bash
# WY-21 (Task B) on the host (headless, nothing on the screen): a client that draws for a few milliseconds after each
# frame callback keeps the refresh rate while the desktop draws every refresh too. `--fake-scanout vsync` is the
# Mini-PC's direct path at a simulated 60 Hz (each flip returns just past the next vblank); `theme.motion medium` at the
# gpu effect level draws every refresh; `wyclient --animate --draw-ms 4` waits 4 ms between callback and commit, as
# glxgears behind Xwayland does. When the frame callbacks left only after the flip, the client's buffer missed the
# frame drawn right after it and the client got every second refresh (33 ms between callbacks, the Mini-PC's 28.7 FPS
# of glxgears); they leave before the present now. Proves: with the desktop still and while it draws every refresh,
# the client's callbacks come at the refresh (median under 20 ms) and the desktop's own frames stay at the refresh.
# Needs the Mesa stage (RED without). A loaded host may make one window slow: three tries, the first pass counts.
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
SOCK=wy86-$$

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
(cd user/services/bitscomp && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)
GPU=$RUN/bitscomp-gpu
bash host/tests/wy/tools/gpu-tree.sh "$RUN/tree" "$GPU" "$RUN/tree" >"$RUN/tree.out" 2>&1 || fail "gpu-tree.sh: $(tail -5 "$RUN/tree.out")"

WYC=$CARGO_TARGET_DIR/release/wyclient
COMP=""
CL=""
cleanup() {
    for p in $CL $COMP; do kill "$p" 2>/dev/null || true; wait "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

ctl() { "$BIN" ctl --socket "$SOCK" "$@"; }
stat_of() { ctl stats | tr ' ' '\n' | sed -n "s/^$1=//p"; }
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
WAYLAND_DISPLAY=$SOCK "$WYC" --title rate --animate --draw-ms 4 >"$RUN/client.out" 2>&1 &
CL=$!
for _ in $(seq 100); do ctl list | grep -q 'title="rate"' && break; sleep 0.1; done
ctl list | grep -q 'title="rate"' || fail "the client did not map"

# The client's median callback interval over its next 2 s report.
median() {
    local n
    n=$(grep -c '^frames ' "$RUN/client.out" || true)
    for _ in $(seq 60); do [ "$(grep -c '^frames ' "$RUN/client.out" || true)" -ge $((n + 2)) ] && break; sleep 0.1; done
    grep '^frames ' "$RUN/client.out" | tail -1 | sed -n 's/.* p50_ms=\([0-9.]*\) .*/\1/p'
}
for motion in off medium; do
    echo "== theme.motion $motion"
    ctl set theme.motion "$motion" >/dev/null
    sleep 1
    good=0
    for i in 1 2 3; do
        p50=$(median)
        ctl stats reset >/dev/null
        sleep 2
        n=$(stat_of frames)
        echo "window $i: the client's callbacks p50 ${p50} ms; the desktop drew $n frames in 2 s, present_avg_ms=$(stat_of present_avg_ms)"
        python3 -I -c "import sys; sys.exit(0 if 0 < float('$p50') < 20 else 1)" || continue
        [ "$motion" = off ] || [ "$n" -gt 90 ] || continue
        good=1
        break
    done
    [ "$good" = 1 ] || fail "motion $motion: the client gets a callback every ${p50} ms (every second refresh gives 33), the desktop $n frames in 2 s"
done
ctl quit >/dev/null
wait "$COMP" 2>/dev/null || true
COMP=""
echo "GREEN: a client drawing 4 ms per frame keeps 60 a second at a simulated 60 Hz, also while the desktop draws every refresh"
