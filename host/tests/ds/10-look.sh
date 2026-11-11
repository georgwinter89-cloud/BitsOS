#!/usr/bin/env bash
# DS-1 on the host (headless back end, nothing on the screen): the design v2 look at the Mini-PC's 2560x1440 with
# bitscomp's built-in defaults (= settingsd's after CR DS-1-1): Absicht dunkel, Absicht hell, a user's accent (the
# first look's cyan) over hell, Exo 2 chosen as the interface font, the start page and the circle menu in both modes.
# Each picture must show its mode's ground colour and match host/tests/ds/ref/<name>.png (tolerance only for
# rounding). Afterwards the still desktop must draw no frame for 2 s (WY-13, cheap effect level: `ctl stats`).
# DS_UPDATE_REF=1 writes the references instead of comparing (only after a look change that was meant).
# DS-2: the references show the wallpaper (dots, glow, vignette) under everything.
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
WYC=$CARGO_TARGET_DIR/release/wyclient
REF=$ROOT/host/tests/ds/ref
IMG="python3 -I $ROOT/host/tests/ds/ds_img.py"
unset DISPLAY WAYLAND_DISPLAY

fail() { echo "FAIL: $*"; exit 1; }

echo "== build bitscomp, wyclient"
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)

SHOTS=$RUN/shots
rm -rf "$SHOTS"
mkdir -m 700 -p "$SHOTS"
python3 -I "$ROOT/host/tests/ds/ds_reference.py" "$BIN" "$WYC" "$SHOTS" 2560x1440 \
    "dark:theme.layout.preset=classic" "light:theme.mode=light" "accent:theme.accent=#00e5ff" \
    "exo:theme.mode=dark;theme.accent=#7fcfa0;theme.font.ui=Exo 2" \
    "start:theme.font.ui=Figtree;!start" "start-light:theme.mode=light" \
    "menu-light:!home;!menu" "menu-dark:theme.mode=dark" | tee "$RUN/shots.txt"

want() { # shot, pattern of its `ctl theme` line
    grep -q "^reference $1: .*$2" "$RUN/shots.txt" || fail "$1: theme is not '$2'"
    echo "ok: $1 has $2"
}
want dark "mode=dark accent=#7fcfa0 bg=#121a16"
want light "mode=light accent=#2f6b4f bg=#e3e7e3"
want accent "mode=light accent=#00e5ff bg=#e3e7e3"
want exo "mode=dark accent=#7fcfa0 bg=#121a16"
want start "mode=dark accent=#7fcfa0 bg=#121a16 .*shows=\[start\]"
want start-light "mode=light accent=#2f6b4f bg=#e3e7e3 .*shows=\[start\]"
want menu-light "mode=light accent=#2f6b4f bg=#e3e7e3 .*shows=\[menu\]"
want menu-dark "mode=dark accent=#7fcfa0 bg=#121a16 .*shows=\[menu\]"

ground() { # shot, r g b: the mode's ground colour shows between and around the tiles
    local share
    share=$(python3 -I -c '
import sys
data = open(sys.argv[1], "rb").read()
parts = data.split(maxsplit=4)
w, h = int(parts[1]), int(parts[2])
px = data[-w * h * 3:]
want = [int(v) for v in sys.argv[2:5]]
# DS-2: the wallpaper dithers and lights the ground; its colour within 10 levels counts.
n = sum(1 for i in range(0, w * h * 3, 3) if all(abs(px[i + k] - want[k]) <= 10 for k in range(3)))
print(f"{n / (w * h):.4f}")' "$SHOTS/$1.ppm" "$2" "$3" "$4")
    python3 -I -c "import sys; sys.exit(0 if float('$share') >= 0.01 else 1)" || fail "$1: ground $2 $3 $4 on $share of the screen"
    echo "ok: $1 shows its ground ($share of the pixels)"
}
ground dark 18 26 22
ground light 227 231 227

for s in dark light accent exo start start-light menu-light menu-dark; do
    if [ "${DS_UPDATE_REF:-0}" = 1 ]; then
        mkdir -p "$REF"
        $IMG png "$SHOTS/$s.ppm" "$REF/$s.png"
        echo "reference $s.png written"
    else
        $IMG diff "$SHOTS/$s.ppm" "$REF/$s.png" || fail "$s differs from host/tests/ds/ref/$s.png"
    fi
done
$IMG diff "$SHOTS/dark.ppm" "$SHOTS/exo.ppm" >/dev/null && fail "Exo 2 draws the same as Figtree"
echo "ok: Exo 2 draws differently from Figtree"
$IMG diff "$SHOTS/start.ppm" "$SHOTS/start-light.ppm" >/dev/null && fail "the start page looks the same in both modes"
$IMG diff "$SHOTS/menu-light.ppm" "$SHOTS/menu-dark.ppm" >/dev/null && fail "the circle menu looks the same in both modes"
echo "ok: start page and circle menu follow the mode"
idle=$(sed -n 's/^idle frames: \([0-9]*\) -> \([0-9]*\)$/\1 \2/p' "$RUN/shots.txt")
[ -n "$idle" ] || fail "no idle frame count"
set -- $idle
[ "$1" = "$2" ] || fail "the still desktop drew $(($2 - $1)) frames in 2 s"
echo "ok: the still desktop drew 0 frames in 2 s (frames=$1)"
echo "GREEN: design v2 tokens and fonts on the host"
