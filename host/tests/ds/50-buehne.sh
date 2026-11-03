#!/usr/bin/env bash
# DS-3 on the host (headless back end, nothing on the screen): the Bühne preset (theme.layout.preset=buehne, the
# default) at the Mini-PC's 2560x1440. ds_buehne.py checks the behaviour (chips, read-only context address, the
# Absicht headline, the right column's fold, Startseite, minimised windows, the sun with CR DS-3-1, 0 frames when
# still, classic as a preset); ds_reference.py draws the pictures, compared with host/tests/ds/ref/buehne-*.png
# (DS_UPDATE_REF=1 writes them). The stage under the tiles needs CR DS-2-1 (render.rs draws the wallpaper layer):
# without it the picture comparison says SKIP, every behaviour check still runs. DS_REQUIRE_CRS=1 (the desk, once
# CR DS-2-1 and CR DS-3-1 are applied) turns both skips (pictures, the sun's click) into failures.
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

B=$RUN/buehne
rm -rf "$B"
mkdir -m 700 -p "$B"
python3 -I "$ROOT/host/tests/ds/ds_buehne.py" "$BIN" "$WYC" "$B" 2560x1440

SHOTS=$RUN/buehne-shots
rm -rf "$SHOTS"
mkdir -m 700 -p "$SHOTS"
python3 -I "$ROOT/host/tests/ds/ds_reference.py" "$BIN" "$WYC" "$SHOTS" 2560x1440 \
    buehne-dark "buehne-absicht:desktop.absicht.arbeitsraum-1=Steuer für Oktober fertig machen" \
    "buehne-light:theme.mode=light" "buehne-classic:theme.mode=dark;theme.layout.preset=classic" \
    "buehne-start:theme.layout.preset=buehne;!start" "buehne-menu:!home;!menu" | tee "$RUN/buehne-shots.txt"
grep -q "^reference buehne-start: .*shows=\[start\]" "$RUN/buehne-shots.txt" || fail "no start page on the stage"
grep -q "^reference buehne-menu: .*shows=\[menu\]" "$RUN/buehne-shots.txt" || fail "no circle menu in the Bühne"
echo "ok: start page and circle menu in the Bühne"
idle=$(sed -n 's/^idle frames: \([0-9]*\) -> \([0-9]*\)$/\1 \2/p' "$RUN/buehne-shots.txt")
set -- $idle
[ "${1:-x}" = "${2:-y}" ] || fail "the still Bühne desktop drew frames"
$IMG diff "$SHOTS/buehne-dark.ppm" "$SHOTS/buehne-light.ppm" >/dev/null && fail "the Bühne looks the same in both modes"
$IMG diff "$SHOTS/buehne-dark.ppm" "$SHOTS/buehne-classic.ppm" >/dev/null && fail "classic looks like the Bühne"
echo "ok: the Bühne follows the mode and differs from classic"

if ! grep -q "wallpaper::elements" "$ROOT/user/services/bitscomp/src/render.rs"; then
    [ "${DS_REQUIRE_CRS:-0}" = 1 ] && fail "DS_REQUIRE_CRS=1 but CR DS-2-1 is not applied (render.rs draws no stage)"
    echo "SKIP: reference pictures (CR DS-2-1 not applied: render.rs draws no wallpaper and no stage)"
else
    for s in buehne-dark buehne-absicht buehne-light buehne-start buehne-menu; do
        if [ "${DS_UPDATE_REF:-0}" = 1 ]; then
            mkdir -p "$REF"
            $IMG png "$SHOTS/$s.ppm" "$REF/$s.png"
            echo "reference $s.png written"
        else
            $IMG diff "$SHOTS/$s.ppm" "$REF/$s.png" || fail "$s differs from host/tests/ds/ref/$s.png"
        fi
    done
    # Classic keeps today's look exactly: the same picture as 10-look's classic dark reference.
    [ "${DS_UPDATE_REF:-0}" = 1 ] || $IMG diff "$SHOTS/buehne-classic.ppm" "$REF/dark.png" || fail "classic differs from host/tests/ds/ref/dark.png"
    echo "ok: reference pictures"
fi
echo "GREEN: the Bühne preset on the host"
