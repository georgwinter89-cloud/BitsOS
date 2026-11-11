#!/usr/bin/env bash
# WY-19 on the host (headless, nothing on the screen): the full effect level of theme.effects, llvmpipe from the Mesa
# stage, 2560x1440. Frozen clocks make every picture exact. Proves: the level switches live both ways and the cheap
# picture comes back to the bit; the glass blurs (the dots under it go, beside it they stay), shadows lie under the
# panels; beam and radar move with the clock and nowhere else; the idle rules (0 frames at the cheap level, with
# motion off, with frozen clocks, under a fullscreen window; frames while it moves); the transitions show the old
# picture and the new scene in the right places, and leave the plain new scene; reference pictures per level (TOL 24,
# 0.3 %); `ctl fx` refuses bad words and goes on; a frame past the GPU wait gets its next frame and leaves no idle frames;
# the textures go back with what used them. Pixman and a software rasteriser without the switch stay at the cheap
# level and say why. Timing is not
# judged here: frame times per level are the Mini-PC's (plan/notes/wy-WY-19.md). Needs the Mesa stage (RED without).
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
cd "$ROOT"
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
export XDG_RUNTIME_DIR=$RUN/xdg
mkdir -m 700 -p "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY

STAGE=${MESA_STAGE:-$ROOT/user/ports/mesa/out/mesa}
if [ ! -f "$STAGE/lib/ld-linux-x86-64.so.2" ]; then
    STAGE=$CARGO_TARGET_DIR/wy-mesa-out/mesa
    if [ ! -f "$STAGE/lib/ld-linux-x86-64.so.2" ]; then
        echo "== stage the Mesa tree into $STAGE"
        MESA_OUT=$CARGO_TARGET_DIR/wy-mesa-out bash user/ports/mesa/scripts/stage-v2.sh >"$RUN/stage.out" 2>&1 \
            || { echo "RED: no Mesa stage ($(tail -1 "$RUN/stage.out"))"; exit 1; }
    fi
fi
export MESA_STAGE=$STAGE

echo "== build the rescue build, wyclient, and the gpu build for a tree at $RUN/tree"
(cd user/services/bitscomp && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)
GPU=$RUN/bitscomp-gpu
bash host/tests/wy/tools/gpu-tree.sh "$RUN/tree" "$GPU" "$RUN/tree" >"$RUN/tree.out" 2>&1 || { echo "RED: gpu-tree.sh: $(tail -5 "$RUN/tree.out")"; exit 1; }

python3 -I host/tests/wy/fx_host.py "$CARGO_TARGET_DIR/release/bitscomp" "$GPU" "$CARGO_TARGET_DIR/release/wyclient" "$RUN" host/tests/wy/ref
echo "GREEN: the effect level on the host"
