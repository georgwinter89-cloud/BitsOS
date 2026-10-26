#!/bin/bash
# PK-4: builds the Brave package on the build host (CachyOS) from BR-2's stick tree:
#   make.sh [STAGE] [OUT]   STAGE = scripts/stage-v2.sh's tree (default /var/tmp/claude-v2/stage/brave),
#                           OUT gets tree/ (the import) and pkg/ (the package: bitspkg install OUT/pkg)
# The stick tree's fonts.conf names /boot/BITSOS2/brave and its prebuilt font cache carries a FAT checksum: in the
# package fonts.conf names /apps/brave/current, the cache is left out and fontconfig writes it once into the data
# folder (/data/appdata/brave/fontcache, uid 1000's), where it stays across boots and goes with the package.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../../.." && pwd)
STAGE=${1:-/var/tmp/claude-v2/stage/brave}
OUT=${2:-$REPO/target/pk/brave}
BITSPKG=${BITSPKG:-bitspkg}
LAUNCHER=$REPO/user/ports/brave-launcher/bitsos/brave
die() { echo "brave/make.sh: $*" >&2; exit 1; }
[ -x "$STAGE/brave" ] || die "$STAGE/brave is missing (run user/ports/brave-launcher/scripts/stage-v2.sh)"
want=$(sed -n 's/^version = "\(.*\)"/\1/p' "$HERE/package.toml")
if have=$(LC_ALL=C pacman -Q brave-bin 2>/dev/null); then
  case $have in *":$want-"* | *" $want-"*) ;; *) die "package.toml says $want, CachyOS has $have: new version in package.toml first" ;; esac
fi
rm -rf "$OUT"
mkdir -p "$OUT"
T=$OUT/tree
"$BITSPKG" import "$STAGE" "$T" --system "$REPO/user/packaging/libset.txt" --exclude fontcache > "$OUT/import.txt"
# Licence texts of the libraries the import added (the stage's own are in LICENSES/ already).
for f in $(sed -n 's/^lib [^ ]* <- //p' "$OUT/import.txt"); do
  p=$(LC_ALL=C pacman -Qoq "$f" 2>/dev/null) || continue
  [ -d "/usr/share/licenses/$p" ] && [ ! -e "$T/LICENSES/$p" ] && cp -rL "/usr/share/licenses/$p" "$T/LICENSES/$p"
done
sed -i 's|<cachedir>/boot/BITSOS2/brave/fontcache</cachedir>|<cachedir>/data/appdata/brave/fontcache</cachedir>|; s|/boot/BITSOS2/brave|/apps/brave/current|g' \
  "$T/etc/fonts/fonts.conf"
grep -q '<dir>/apps/brave/current/fonts</dir>' "$T/etc/fonts/fonts.conf" || die "fonts.conf: font folder not rewritten"
grep -q '/data/appdata/brave/fontcache' "$T/etc/fonts/fonts.conf" || die "fonts.conf: cache folder not rewritten"
mkdir -p "$T/bitsos"
install -m 0755 "$LAUNCHER" "$T/bitsos/brave"
install -m 0755 "$HERE/run" "$T/bitsos/run"
install -m 0644 "$HERE/package.toml" "$T/package.toml"
"$BITSPKG" build "$T" --out "$OUT/pkg"
echo "brave/make.sh: $(grep -c '^lib ' "$OUT/import.txt") libraries added; package in $OUT/pkg"
