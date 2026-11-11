#!/usr/bin/env bash
# WY-21 on the host (headless, nothing on the screen): the in-process switch away from a lost GPU keeps its clients,
# dma-buf ones included. The gpu build draws with GLES (llvmpipe from the Mesa stage) and watches a file in place of the
# render node (`--watch-node`); removing it is the lost device's ENODEV. A wl_shm window and a dma-buf window
# (`wyclient --dmabuf --animate`, /dev/udmabuf) animate. Proves: the switch is said and Pixman draws; the dma-buf
# client hears the new feedback on the default feedback it kept bound (main device 0:0 and Pixman's own, shorter
# format table), and both clients keep drawing (more frames after) and their windows stay. Needs the Mesa stage and
# /dev/udmabuf (RED without them).
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
SOCK=wy82-$$

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
[ -r /dev/udmabuf ] && [ -w /dev/udmabuf ] || fail "no usable /dev/udmabuf"
COMP=""
CLS=""
cleanup() {
    for p in $CLS $COMP; do kill "$p" 2>/dev/null || true; wait "$p" 2>/dev/null || true; done
}
trap cleanup EXIT
ctl() { "$BIN" ctl --socket "$SOCK" "$@"; }
stat_of() { ctl stats | tr ' ' '\n' | sed -n "s/^$1=//p"; }
: >"$RUN/node"
"$GPU" --renderer gles --watch-node "$RUN/node" --backend headless --size 640x400 \
    --socket "$SOCK" --fixed-status --mcp none --settings none --installer none >"$RUN/comp.log" 2>&1 &
COMP=$!
for _ in $(seq 100); do grep -q "^WAYLAND_DISPLAY=" "$RUN/comp.log" && break; sleep 0.1; done
grep -q " build=gpu renderer=gles " "$RUN/comp.log" || fail "not on GLES: $(grep WAYLAND_DISPLAY "$RUN/comp.log")"
grep -q "gpu: watching $RUN/node" "$RUN/comp.log" || fail "the watch does not look at the file"
WAYLAND_DISPLAY=$SOCK "$WYC" --title shm --animate >"$RUN/shm.out" 2>&1 &
CLS="$CLS $!"
WAYLAND_DISPLAY=$SOCK "$WYC" --title dma --dmabuf --animate >"$RUN/dma.out" 2>&1 &
CLS="$CLS $!"
for _ in $(seq 100); do [ "$(ctl list | grep -c 'title="\(shm\|dma\)"')" = 2 ] && break; sleep 0.1; done
[ "$(ctl list | grep -c 'title="\(shm\|dma\)"')" = 2 ] || fail "the windows did not map: $(ctl list)"
first=$(grep -m1 "^dmabuf feedback main_device" "$RUN/dma.out") || fail "no feedback: $(head -5 "$RUN/dma.out")"
sleep 3
count() { grep -c "^frames " "$RUN/$1.out" || true; }
before_dma=$(count dma); before_shm=$(count shm)
rm "$RUN/node"
for _ in $(seq 100); do grep -q "renderer=pixman from now on" "$RUN/comp.log" && break; sleep 0.1; done
grep -q "gpu: a new open of $RUN/node answers .*: the desktop goes on with pixman, the windows stay" "$RUN/comp.log" || fail "no switch: $(tail -5 "$RUN/comp.log")"
for _ in $(seq 50); do grep -q "^dmabuf feedback again" "$RUN/dma.out" && break; sleep 0.1; done
again=$(grep -m1 "^dmabuf feedback again" "$RUN/dma.out") || fail "the bound dma-buf client heard no new feedback (it had: $first)"
grep -q "dmabuf: the feedback now for renderer pixman" "$RUN/comp.log" || fail "the feedback was not renewed: $(grep dmabuf: "$RUN/comp.log")"
[ "$(stat_of renderer)" = pixman ] || fail "ctl stats: $(ctl stats)"
sleep 5
for c in $CLS; do kill -0 "$c" 2>/dev/null || fail "a client ended with the switch: $(tail -3 "$RUN/shm.out" "$RUN/dma.out")"; done
[ "$(ctl list | grep -c 'title="\(shm\|dma\)"')" = 2 ] || fail "a window went: $(ctl list)"
[ "$(count dma)" -gt $((before_dma + 1)) ] || fail "the dma-buf client stopped drawing: $(tail -3 "$RUN/dma.out")"
[ "$(count shm)" -gt $((before_shm + 1)) ] || fail "the wl_shm client stopped drawing: $(tail -3 "$RUN/shm.out")"
grep -q "dmabuf failed" "$RUN/dma.out" && fail "a dma-buf was refused after the switch: $(grep 'dmabuf failed' "$RUN/dma.out" | head -3)"
echo "ok: $first -> ${again#dmabuf feedback again }"
ctl quit >/dev/null
wait "$COMP" 2>/dev/null || true
COMP=""
echo "GREEN: the switch from a lost GPU to Pixman keeps both windows; the dma-buf client hears the new feedback and draws on"
