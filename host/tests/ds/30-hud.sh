#!/usr/bin/env bash
# DS-2 on the host (headless back end, nothing on the screen): the HUD kit at the cheap effect level at the Mini-PC's
# 2560x1440. theme.effects=gpu draws exactly like cheap, the wallpaper lies under the tiles, Claude's busy ring moves
# (only its own pixels, at most 10 steps a second), a waiting or idle ring and the still desktop draw no frame (idle:
# 10 s, `ctl stats`). Prints the host's cost of a full redraw (numbers only). Every check is a hard one, the wallpaper's
# too. BITSOS_RUN_DIR must be short: the path of the Wayland socket below it (<run>/hud/wayland-dshud, its control
# socket with ".ctl" after it) has to fit a unix socket's 108 bytes, or bitscomp starts without one.
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
[ "${#RUN}" -le 80 ] || { echo "RED: BITSOS_RUN_DIR too long for a unix socket under it (${#RUN} bytes): $RUN"; exit 1; }
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
unset DISPLAY WAYLAND_DISPLAY

echo "== build bitscomp, wyclient"
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)
SHOTS=$RUN/hud
rm -rf "$SHOTS"
mkdir -m 700 -p "$SHOTS"
python3 -I "$ROOT/host/tests/ds/ds_hud.py" "$CARGO_TARGET_DIR/release/bitscomp" "$CARGO_TARGET_DIR/release/wyclient" "$SHOTS" 2560x1440
echo "GREEN: HUD kit at the cheap level on the host"
