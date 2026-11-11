#!/usr/bin/env bash
# slow
# Run: `cargo xtask fullrun`, on the build host (nothing boots: programs run on the Mini-PC only).
# TS-2: Cobalt's tree carries PKGLIST.TXT as the other stick trees do: an image with V2_STICK_TREES=cobalt writes it,
# made from what was copied, with what ul.pkg.cobalt demands of it. On the Mini-PC: host/tests/minipc/README.md.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
NAME=yt-tree-list
X="${CARGO_TARGET_DIR:-target}/xtask"
trap 'rm -rf "$X/esp-$NAME" "$X/$NAME.img"' EXIT
V2_STICK_TREES=cobalt cargo xtask image --name "$NAME" > "${BITSOS_RUN_DIR:-target}/image.out" 2>&1 \
  || { tail -5 "${BITSOS_RUN_DIR:-target}/image.out"; echo "RED: no image with Cobalt's tree (user/ports/cobalt-launcher/scripts/stage-v2.sh)"; exit 1; }
T="$X/esp-$NAME/BITSOS2/cobalt"
L="$T/PKGLIST.TXT"
[ -f "$L" ] || { echo "RED: $L is missing"; exit 1; }
files=$(find "$T" -type f ! -name PKGLIST.TXT | wc -l)
lines=$(wc -l < "$L")
[ "$files" -gt 0 ] && [ "$files" = "$lines" ] || { echo "RED: $files files in the tree, $lines lines in its list"; exit 1; }
if grep -vq '^stick /boot/BITSOS2/cobalt/' "$L"; then echo "RED: a line of the list is not under the tree"; exit 1; fi
for f in LICENSES/README.txt LICENSES/libcobalt.txt LICENSES/loader_app.txt LICENSES/fonts.txt LICENSES/ffmpeg.txt loader_app; do
    grep -q "^stick /boot/BITSOS2/cobalt/$f " "$L" || { echo "RED: $f is not in the list"; exit 1; }
done
echo "yt tree-list: green ($lines files listed)"
