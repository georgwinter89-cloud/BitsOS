#!/usr/bin/env bash
# WY-17 on the host (headless back end, nothing on the screen): the GLES renderer drawing straight into two windows
# and flipping, with memfds standing in for fbdev's dma-bufs (`--fake-scanout`; llvmpipe maps both alike). The gpu
# build is linked for a copy of the staged Mesa tree (tools/gpu-tree.sh), as in 97 and 98.
# Proves: the choice is made at start and logged (`scanout: direct`, `scanout: readback: why`), `ctl stats` says it;
# the picture of the direct path equals the readback path's (byte for byte where both draw with the same GL) and
# Pixman's (within the screenshot tolerance), at the first frame, after twenty maximise toggles (damage drawn at
# age 2 into alternating windows: a window that missed a frame shows it here), after a software pointer crossed the
# screen frame by frame (small damage; a trail would show) and with a window floated and resized; a window EGL cannot import leaves exactly the readback path; a refused flip and a mode change go back to the readback path at
# run time with the whole picture; `--scanout readback` never tries. Needs the Mesa stage (RED when it cannot be made).
# What only V2 can show (the kernel's windows, the MCP picture, CONFIG.TXT, no node): 45-v2-scanout.sh.
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
            || { echo "RED: no Mesa stage ($(tail -1 "$RUN/stage.out"))"; exit 1; }
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

ctl() { "$BIN" ctl --socket "$(cat "$RUN/$1.sock")" "${@:2}"; }
stat_of() { ctl "$1" stats | tr ' ' '\n' | sed -n "s/^$2=//p"; }
# desktop NAME PROGRAM ARGS..: one compositor with one wyclient; a screenshot once the picture is still.
desktop() {
    local name=$1 sock=wy88-$1-$$
    shift
    "$@" --backend headless --size 1280x800 --pointer soft --socket "$sock" --fixed-status --mcp none --settings none --installer none \
        >"$RUN/$name.log" 2>&1 &
    local cp=$!
    PIDS+=("$cp")
    echo "$sock" >"$RUN/$name.sock"
    for _ in $(seq 300); do grep -q "^WAYLAND_DISPLAY=" "$RUN/$name.log" && break; sleep 0.1; done
    grep -q "^WAYLAND_DISPLAY=" "$RUN/$name.log" || fail "$name: no start line: $(tail -5 "$RUN/$name.log")"
    WAYLAND_DISPLAY=$sock "$WYC" --title one --pattern 0 >"$RUN/$name-client.out" 2>&1 &
    PIDS+=("$!")
    for _ in $(seq 100); do ctl "$name" list 2>/dev/null | grep -q 'title="one"' && break; sleep 0.1; done
    ctl "$name" list | grep -q 'title="one"' || fail "$name: the client did not map"
    sleep 3 # the start animation and the window's opening are over
    ctl "$name" shot "$RUN/$name.ppm" >/dev/null
}
quit() { ctl "$1" quit >/dev/null 2>&1 || true; sleep 0.3; }
near() { # A B what: within the screenshot tolerance
    $IMG diff "$1" "$2" >"$RUN/diff.out" 2>&1 || fail "$3: $(cat "$RUN/diff.out")"
    echo "ok: $3: $(cat "$RUN/diff.out")"
}
exact() { # A B what
    cmp -s "$1" "$2" || fail "$3: not byte for byte ($($IMG diff "$1" "$2" 2>&1 | tail -1))"
    echo "ok: $3: byte for byte"
}
# The same changes in every round, so the pictures afterwards must agree.
wiggle() { # name
    local i
    for i in $(seq 8); do ctl "$1" pointer $((i * 40 + 200)) 300 >/dev/null; sleep 0.1; done
    sleep 0.5
}
churn() { # name
    local i
    for i in $(seq 10); do ctl "$1" max >/dev/null; sleep 0.4; ctl "$1" max >/dev/null; sleep 0.4; done
    sleep 1
    ctl "$1" shot "$RUN/$1-churn.ppm" >/dev/null
    # A frame per pointer position, each a small damage in the other window.
    for i in $(seq 12); do ctl "$1" pointer $((i * 90)) $((i * 55)) >/dev/null; sleep 0.1; done
    sleep 1
    ctl "$1" shot "$RUN/$1-pointer.ppm" >/dev/null
    ctl "$1" float >/dev/null
    sleep 0.8
    ctl "$1" shrink >/dev/null
    ctl "$1" pointer 640 400 >/dev/null
    sleep 1
    ctl "$1" shot "$RUN/$1-back.ppm" >/dev/null
    ctl "$1" size 1024 768 >/dev/null
    sleep 1.5
    ctl "$1" shot "$RUN/$1-1024.ppm" >/dev/null
    ctl "$1" size 1280 800 >/dev/null
    sleep 1.5
}
said() { # log pattern what
    grep -q "$2" "$1" || fail "$3: $(grep -E 'scanout|renderer' "$1")"
}
none() { # log pattern what
    if grep -q "$2" "$1"; then fail "$3: $(grep "$2" "$1")"; fi
}

echo "== the reference pictures: Pixman, and GLES reading back"
desktop pixman "$BIN"
said "$RUN/pixman.log" "scanout: readback: renderer pixman draws by CPU" "pixman: the choice is logged"
[ "$(stat_of pixman scanout)" = readback ] || fail "pixman: $(ctl pixman stats)"
wiggle pixman
churn pixman
quit pixman
desktop readback "$GPU" --renderer gles --scanout readback --fake-scanout ok
said "$RUN/readback.log" "scanout: readback: --scanout readback" "--scanout readback never tries"
[ "$(stat_of readback scanout)" = readback ] || fail "readback: $(ctl readback stats)"
wiggle readback
churn readback
quit readback

echo "== GLES straight into the windows"
desktop direct "$GPU" --renderer gles --scanout direct --fake-scanout ok
said "$RUN/direct.log" "scanout: direct: gles draws into two dma-buf windows" "the choice is logged"
said "$RUN/direct.log" "renderer gles: GL_RENDERER=llvmpipe" "llvmpipe draws on the host"
[ "$(stat_of direct scanout)" = direct ] || fail "direct: $(ctl direct stats)"
exact "$RUN/readback.ppm" "$RUN/direct.ppm" "the first picture"
near "$RUN/pixman.ppm" "$RUN/direct.ppm" "against Pixman"
# Damage tracking survives the windows: a frame with a small change draws that (plus the frame before, age 2), not the
# whole window. The pictures below would look the same if it drew everything every time.
n1=$(stat_of direct frames); a1=$(stat_of direct damaged_mpx_avg)
wiggle direct
n2=$(stat_of direct frames); a2=$(stat_of direct damaged_mpx_avg)
python3 -I - "$n1" "$a1" "$n2" "$a2" <<'PY' || fail "direct: small changes drew big frames"
import sys
n1, a1, n2, a2 = int(sys.argv[1]), float(sys.argv[2]), int(sys.argv[3]), float(sys.argv[4])
if n2 <= n1:
    print(f"no frame for the pointer: {n1} -> {n2}")
    sys.exit(1)
per = (a2 * n2 - a1 * n1) / (n2 - n1)
print(f"  {n2 - n1} frames for a moving pointer, {per:.3f} Mpx each of 1.024")
sys.exit(0 if per < 0.2 else 1)
PY
churn direct
exact "$RUN/readback-churn.ppm" "$RUN/direct-churn.ppm" "after twenty maximise toggles"
exact "$RUN/readback-pointer.ppm" "$RUN/direct-pointer.ppm" "after a pointer crossed the screen"
exact "$RUN/readback-back.ppm" "$RUN/direct-back.ppm" "with a window floated and resized"
near "$RUN/pixman-back.ppm" "$RUN/direct-back.ppm" "against Pixman, after all of it"
# Only a host test can change the mode under the windows: back to readback, whole.
said "$RUN/direct.log" "scanout: back to readback: the mode changed" "the mode change is logged"
[ "$(stat_of direct scanout)" = readback ] || fail "after the mode: $(ctl direct stats)"
exact "$RUN/readback-1024.ppm" "$RUN/direct-1024.ppm" "after the mode change"
none "$RUN/direct.log" "drawing with pixman\|render:" "direct: a fallback or a skipped frame"
quit direct

echo "== a window that does not map, a window EGL refuses: exactly the readback path"
desktop badfd "$GPU" --renderer gles --scanout direct --fake-scanout badfd
said "$RUN/badfd.log" "scanout: readback: the window does not map" "the refusal is logged"
[ "$(stat_of badfd scanout)" = readback ] || fail "badfd: $(ctl badfd stats)"
exact "$RUN/readback.ppm" "$RUN/badfd.ppm" "the picture"
quit badfd
desktop nostride "$GPU" --renderer gles --scanout direct --fake-scanout nostride
said "$RUN/nostride.log" "scanout: readback: EGL refuses the window" "the refusal is logged"
[ "$(stat_of nostride scanout)" = readback ] || fail "nostride: $(ctl nostride stats)"
exact "$RUN/readback.ppm" "$RUN/nostride.ppm" "the picture"
quit nostride

echo "== a refused flip at run time: back to readback, the picture whole"
desktop refuse "$GPU" --renderer gles --scanout direct --fake-scanout refuse
said "$RUN/refuse.log" "scanout: direct" "it started direct"
said "$RUN/refuse.log" "scanout: back to readback: pan to the window refused" "the refusal is logged"
[ "$(stat_of refuse scanout)" = readback ] || fail "refuse: $(ctl refuse stats)"
exact "$RUN/readback.ppm" "$RUN/refuse.ppm" "the picture"
churn refuse
exact "$RUN/readback-back.ppm" "$RUN/refuse-back.ppm" "and the readback goes on"
quit refuse
none "$RUN/refuse.log" "render:" "refuse: a skipped frame"

echo "PASS: the direct path draws the readback path's picture, and every way out of it keeps the picture"
