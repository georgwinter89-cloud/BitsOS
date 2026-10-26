#!/usr/bin/env bash
# WY-11: the pointer from the Adwaita theme and on the (fake) hardware cursor, headless, no window.
# 1. `--pointer fake`: wyclient names shapes through wp_cursor_shape_v1 (text, then wait), hides the pointer, sets
#    its own cursor surface; `ctl cursor` must show the theme's image at the chosen size (theme.cursor.size 32 at
#    125 % = 40 px -> Adwaita's 48 px), the surface at the output scale, the move made at input time (one fake
#    move per motion event, no frame drawn for pure motion; the input-to-move delay is printed, not judged).
#    Also (WY-11 fixer): bitscomp's own shape over its header wins over the client's; a new hotspot on the same
#    surface without a commit re-uploads; a cursor surface too big for the hardware (300x300) goes to software
#    with the fake hidden; an animating client does not couple the moves to its frames; the "starting" mark draws
#    frames beside the hardware pointer; injected door errors: EBUSY on a move and EIO on an image go to software
#    (one hide, frames carry the pointer) and come back after the retry, ENODEV ends the hardware cursor; at exit
#    the cursor is hidden.
# 2. `--pointer soft` (fbdev's ENODEV path): the same shape drawn into the frames (a shot differs at the spot).
# 3. Started programs get XCURSOR_THEME/XCURSOR_SIZE.
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
CLIENT=$CARGO_TARGET_DIR/release/wyclient
SOCK=wayland-wy-cursor-$$
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-$RUN}
# The theme CachyOS ships at the path the image copies it to; nothing from $HOME.
export XCURSOR_PATH=/usr/share/icons
unset DISPLAY WAYLAND_DISPLAY XCURSOR_THEME XCURSOR_SIZE

if [ ! -f /usr/share/icons/Adwaita/cursors/text ]; then
    echo "SKIP: no Adwaita cursor theme on this host (adwaita-cursors)"
    exit 0
fi

echo "== build"
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q --workspace)

COMP=""
CL=""
cleanup() {
    [ -n "$CL" ] && kill "$CL" 2>/dev/null || true
    [ -n "$COMP" ] && kill "$COMP" 2>/dev/null || true
}
trap cleanup EXIT

ctl() { "$BIN" ctl --socket "$SOCK" "$@"; }
fail() { echo "FAIL: $*"; ctl cursor || true; ctl list || true; exit 1; }
field() { ctl "$1" | tr ' ' '\n' | sed -n "s/^$2=//p"; }
need() { # what, pattern on `ctl cursor`
    for _ in $(seq 50); do
        ctl cursor | grep -q -- "$2" && return 0
        sleep 0.1
    done
    fail "$1 (want '$2')"
}
settle() { # until no frame is drawn for 0.4 s (the window's opening animation is over)
    local a b
    for _ in $(seq 25); do
        a=$(field stats frames)
        sleep 0.4
        b=$(field stats frames)
        [ "$a" = "$b" ] && return 0
    done
    fail "bitscomp never went idle"
}
start() { # pointer mode
    "$BIN" --backend headless --size 1600x1000 --socket "$SOCK" --pointer "$1" --mcp none --settings none \
        --fixed-status >"$RUN/bitscomp-cursor-$1.log" 2>&1 &
    COMP=$!
    for _ in $(seq 50); do ctl list >/dev/null 2>&1 && return 0; sleep 0.1; done
    fail "bitscomp did not start"
}
client() { # --cursor value, more wyclient flags
    [ -n "$CL" ] && kill "$CL" 2>/dev/null && wait "$CL" 2>/dev/null || true
    WAYLAND_DISPLAY=$SOCK "$CLIENT" --cursor "$@" >"$RUN/wyclient-$1.log" 2>&1 &
    CL=$!
    # This client's window (by pid: the one before may still be listed for a moment).
    for _ in $(seq 50); do ctl list | grep -q "app_id=wyclient .*pid=$CL " && break; sleep 0.1; done
    ctl list | grep -q "app_id=wyclient .*pid=$CL " || fail "no wyclient window"
    # Out and back in: the enter is where the client sets its pointer.
    ctl pointer 5 5 >/dev/null
    ctl pointer 600 400 >/dev/null
    for _ in $(seq 50); do grep -q "cursor set $1" "$RUN/wyclient-$1.log" && return 0; sleep 0.1; done
    fail "wyclient did not set its cursor $1"
}
reenter() { # out of the window and back in: the client sets its pointer again
    ctl pointer 5 5 >/dev/null
    ctl pointer 600 400 >/dev/null
}
hot_of() { # wyclient's last hotspot for $1 -> the expected one at 125 % (Rust's round: half away from zero)
    grep "cursor set $1 " "$RUN/wyclient-$1.log" | tail -1 | awk '{split($4, h, ","); printf "%d,%d", h[1] * 1.25 + 0.5, h[2] * 1.25 + 0.5}'
}
moves_frames() { # n motion events from x0,y0: prints "moves frames"
    local m0 f0 i
    m0=$(field cursor moves); f0=$(field stats frames)
    for i in $(seq 1 "$1"); do ctl pointer $(($2 + i * 3)) $(($3 + i)) >/dev/null; sleep 0.02; done
    echo "$(($(field cursor moves) - m0)) $(($(field stats frames) - f0))"
}

echo "== 1. fake hardware cursor"
start fake
need "bitscomp's own default shape on the hardware cursor" "mode=hw hw=true shape=default image=48x48"
client text
need "the named shape text from Adwaita at 48 px" "mode=hw hw=true shape=text image=48x48 "
settle
m0=$(field cursor fake_moves)
f0=$(field stats frames)
for i in $(seq 1 20); do
    ctl pointer $((600 + i * 3)) $((400 + i)) >/dev/null
    sleep 0.02
done
m1=$(field cursor fake_moves)
f1=$(field stats frames)
[ $((m1 - m0)) -eq 20 ] || fail "20 motion events made $((m1 - m0)) hardware moves"
[ $((f1 - f0)) -eq 0 ] || fail "pure motion on the hardware cursor drew $((f1 - f0)) frames"
need "the cursor is where the pointer is" "fake_at=660,420 "
echo "   20 moves at input time, 0 frames; input-to-move delay last=$(field cursor move_us_last) us max=$(field cursor move_us_max) us (indicative)"
read -r hx hy <<<"$(ctl hot | grep 'title="wyclient"' | head -1 | sed -n 's/.* header=\([-0-9.]*\),\([-0-9.]*\).*/\1 \2/p')"
[ -n "$hx" ] || fail "no header spot for wyclient in ctl hot"
ctl pointer "${hx%.*}" "${hy%.*}" >/dev/null
need "bitscomp's own shape over its header wins over the client's text" "mode=hw hw=true shape=default image=48x48"
ctl pointer 600 400 >/dev/null
need "back in the window: the client's text again" "shape=text image=48x48 "

echo "== 1b. the starting mark beside the hardware pointer"
ctl spawn sleep 3 >/dev/null
for _ in $(seq 50); do ctl stats | grep -q " launching=1 " && break; sleep 0.1; done
ctl stats | grep -q " launching=1 " || fail "a windowless start does not carry the starting mark"
read -r m f <<<"$(moves_frames 5 600 400)"
[ "$m" -eq 5 ] && [ "$f" -ge 1 ] || fail "with the mark showing, 5 motions made $m moves and $f frames (want 5 and >= 1)"
echo "   mark showing: 5 moves on the hardware, $f frames for the mark"
for _ in $(seq 60); do ctl stats | grep -q " launching=0 " && break; sleep 0.1; done

echo "== 1c. an animating client: every motion moves the cursor itself, frames or not"
client text --animate
read -r m f <<<"$(moves_frames 20 600 400)"
[ "$m" -eq 20 ] || fail "20 motions beside an animation made $m hardware moves"
echo "   20 motions: 20 moves at input time, $f frames of the animation meanwhile; move_us_max=$(field cursor move_us_max) (indicative)"

echo "== 1d. shapes and surfaces"
client wait
need "an animated shape shows its first frame" "shape=wait image=48x48"
client hidden
need "a hidden pointer hides the hardware cursor" "mode=hw hw=true shape=hidden .*fake_visible=false"
client surface
# 32x32 at buffer scale 1, shown at 125 %: 40x40, hotspot 3,5 -> 4,6 (or 4,6 -> 5,8 on an odd enter).
h1=$(hot_of surface)
need "the client's cursor surface at the output scale" "mode=hw hw=true shape=surface image=40x40 hot=$h1 "
n=$(grep -c "cursor set surface" "$RUN/wyclient-surface.log")
reenter
for _ in $(seq 50); do [ "$(grep -c "cursor set surface" "$RUN/wyclient-surface.log")" -gt "$n" ] && break; sleep 0.1; done
h2=$(hot_of surface)
[ "$h1" != "$h2" ] || fail "wyclient did not change its hotspot ($h1)"
need "a new hotspot on the same surface (no commit) is uploaded" "mode=hw hw=true shape=surface image=40x40 hot=$h2 "
echo "   hotspot $h1 -> $h2 without a new commit"
client big
need "a 300x300 cursor surface goes to software with the hardware cursor hidden" "mode=software hw=true shape=surface .*fake_visible=false"
read -r m f <<<"$(moves_frames 3 600 400)"
[ "$m" -eq 0 ] && [ "$f" -ge 1 ] || fail "the big surface in software: $m moves, $f frames (want 0 and >= 1)"
grep -q "XCURSOR_THEME=Adwaita XCURSOR_SIZE=32" "$RUN/bitscomp-cursor-fake.log" || fail "programs do not get XCURSOR_THEME/XCURSOR_SIZE"

echo "== 1e. door errors: EBUSY on a move, EIO on an image (retried), ENODEV (for good)"
client text
need "text on the hardware" "mode=hw hw=true shape=text "
settle
h0=$(field cursor fake_hides)
ctl cursor fail 16 1 >/dev/null
read -r m f <<<"$(moves_frames 1 600 400)"
need "EBUSY on a move: software, one hide" "mode=software hw=true shape=text .*fake_hides=$((h0 + 1)) .*fake_visible=false"
[ "$f" -ge 1 ] || fail "after the refused move the frames carry no pointer ($f frames)"
need "the retry puts the pointer back on the hardware" "mode=hw hw=true shape=text .*retries=1 .*fake_visible=true"
ctl cursor fail 5 1 >/dev/null
ctl pointer "${hx%.*}" "${hy%.*}" >/dev/null
need "EIO on an image: software" "mode=software hw=true shape=default "
need "the retry uploads the image again" "mode=hw hw=true shape=default .*retries=2 .*fake_visible=true"
ctl pointer 600 400 >/dev/null
need "text again" "mode=hw hw=true shape=text "
ctl cursor fail 19 1 >/dev/null
read -r m f <<<"$(moves_frames 1 600 400)"
need "ENODEV ends the hardware cursor" "mode=software hw=false shape=text "
grep "no hardware cursor (a move: errno 19)" "$RUN/bitscomp-cursor-fake.log" | grep -q "fake_hides=$((h0 + 3)) .*fake_visible=false" \
    || fail "ENODEV did not hide the hardware cursor first"
read -r m f <<<"$(moves_frames 2 600 400)"
[ "$f" -ge 1 ] || fail "after ENODEV the frames carry no pointer"
echo "   EBUSY/EIO: software, then back after the retry; ENODEV: software for good, hidden first"
kill "$CL" 2>/dev/null || true
wait "$CL" 2>/dev/null || true
CL=""
kill "$COMP" 2>/dev/null || true
wait "$COMP" 2>/dev/null || true
COMP=""

echo "== 1f. hidden at exit"
start fake
need "on the hardware" "mode=hw hw=true "
kill "$COMP"
wait "$COMP" 2>/dev/null || true
COMP=""
grep -q "cursor: hidden at exit (ok): .*fake_hides=1 .*fake_visible=false" "$RUN/bitscomp-cursor-fake.log" || fail "the cursor was not hidden at exit"

echo "== 2. software pointer (fbdev's ENODEV path)"
start soft
client text
need "the named shape drawn into the frames" "mode=software hw=false shape=text image=48x48"
f0=$(field stats frames)
ctl pointer 700 450 >/dev/null
sleep 0.3
ctl shot "$RUN/a.ppm" >/dev/null
ctl pointer 900 600 >/dev/null
sleep 0.3
ctl shot "$RUN/b.ppm" >/dev/null
f1=$(field stats frames)
[ $((f1 - f0)) -ge 2 ] || fail "the software pointer drew $((f1 - f0)) frames for two moves"
python3 - "$RUN/a.ppm" "$RUN/b.ppm" <<'EOF' || fail "the shot shows no pointer at its spot"
import sys
def ppm(p):
    d = open(p, 'rb').read()
    magic, size, _, rest = d.split(b'\n', 3)
    return int(size.split()[0]), rest
w, a = ppm(sys.argv[1])
_, b = ppm(sys.argv[2])
px = lambda d, x, y: d[(y * w + x) * 3:(y * w + x) * 3 + 3]
# The I-beam's hotspot (22,24 in its 48 px image) is at 700,450: some pixel of its box differs between the shots.
diff = sum(px(a, x, y) != px(b, x, y) for y in range(430, 470) for x in range(680, 720))
print(f"   pixels changed at the old spot: {diff}")
sys.exit(0 if diff > 20 else 1)
EOF

echo "PASS: 92-cursor"
