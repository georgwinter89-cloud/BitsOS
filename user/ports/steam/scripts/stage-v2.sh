#!/usr/bin/env bash
# GM-2a: builds the tree that `cargo xtask image` puts into \BITSOS2\steam\ on the stick (manifest line
# `tree steam host:/var/tmp/claude-v2/stage/steam`), from Valve's own files, pinned by pins.txt. Steam is not started
# here and never is on this machine (Georg 30.10.): files are only fetched, checked, copied and read.
#   bootstraplinux_ubuntu12_32.tar.xz   Valve's bootstrap from the steam-launcher deb, as published
#   package/                            the client's manifest (publicbeta channel) and every zip it names, as published;
#                                       the launcher seeds Steam's own package/ folder with them on /data
#   gfx.tar.gz                          the graphics provider of user/ports/steamrt (out/gfx: Mesa + glibc as an Arch-shaped
#                                       sysroot); a tar because the sysroot has links and FAT has none
#   bin/realpath                        files/realpath, for pv-run.sh on the host side
#   LICENSES/, VERSION.txt              the deb's copyright file and subscriber agreement; what the tree was made from
# The stage is replaced as a whole (new dir, rename). V2_DOWNLOADS overrides the download cache, V2_OFFLINE=1 refuses to
# download, STEAM_REPIN=1 takes Valve's newest manifest and rewrites the manifest line of pins.txt, STEAM_STAGE moves the stage.
set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
PINS=$HERE/pins.txt
DEST=${STEAM_STAGE:-/var/tmp/claude-v2/stage/steam}
CACHE=${V2_DOWNLOADS:-$HOME/.cache/bitsos-v2/downloads}/steam
STEAMRT=$HERE/../steamrt
GFX=$STEAMRT/out/gfx
CHANNEL=publicbeta
MANIFEST=steam_client_${CHANNEL}_ubuntu12
LAUNCHER_URL=https://repo.steampowered.com/steam/archive/stable
CDN=https://client-update.steamstatic.com
die() { echo "stage-v2: $*" >&2; exit 1; }
for t in curl sha256sum bsdtar tar gzip python3; do command -v $t >/dev/null || die "no $t on this host"; done

mkdir -p "$CACHE"
fetch() { # URL FILE: into the cache unless it is there
  [ -f "$2" ] && return 0
  [ "${V2_OFFLINE:-0}" != 1 ] || die "$2 is not in the cache and V2_OFFLINE=1"
  curl -fsSL --max-time 1800 -o "$2.part" "$1" || die "cannot fetch $1"
  mv "$2.part" "$2"
}
pin() { sed -n "s/^$1 //p" "$PINS"; }
checked() { # FILE SHA
  [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$2" ] || die "$1: its SHA256 is not the pinned $2"
}

read -r deb deb_sha <<<"$(pin launcher)"
[ -n "${deb:-}" ] || die "pins.txt has no launcher line"
fetch "$LAUNCHER_URL/$deb" "$CACHE/$deb"
checked "$CACHE/$deb" "$deb_sha"

if [ "${STEAM_REPIN:-0}" = 1 ]; then
  curl -fsSL --max-time 300 -o "$CACHE/manifest.new" "$CDN/$MANIFEST" || die "cannot fetch the manifest"
  ver=$(python3 -I "$HERE/scripts/manifest.py" version "$CACHE/manifest.new")
  sha=$(sha256sum "$CACHE/manifest.new" | cut -d' ' -f1)
  mv "$CACHE/manifest.new" "$CACHE/$MANIFEST.$ver"
  sed -i '/^manifest /d' "$PINS"
  echo "manifest $MANIFEST $ver $sha" >>"$PINS"
fi
read -r m_name m_ver m_sha <<<"$(pin manifest)"
[ -n "${m_name:-}" ] || die "pins.txt has no manifest line: STEAM_REPIN=1 takes the newest"
M=$CACHE/$m_name.$m_ver
if [ ! -f "$M" ]; then
  [ "${V2_OFFLINE:-0}" != 1 ] || die "$M is not in the cache and V2_OFFLINE=1"
  # The URL has no version: only a manifest that is still the pinned one is taken.
  fetch "$CDN/$m_name" "$M.try"
  [ "$(sha256sum "$M.try" | cut -d' ' -f1)" = "$m_sha" ] \
    || { rm -f "$M.try"; die "Valve's manifest is newer than the pinned $m_ver and the pinned copy is not in $CACHE: STEAM_REPIN=1 pins the newest"; }
  mv "$M.try" "$M"
fi
checked "$M" "$m_sha"

# Every zip by its manifest's sha2.
python3 -I "$HERE/scripts/manifest.py" packages "$M" >"$CACHE/packages.txt"
[ -s "$CACHE/packages.txt" ] || die "$M names no packages"
while read -r name file size sha; do
  fetch "$CDN/$file" "$CACHE/$file"
  [ "$(stat -c %s "$CACHE/$file")" = "$size" ] || die "$file ($name): not the manifest's $size bytes"
  checked "$CACHE/$file" "$sha"
done <"$CACHE/packages.txt"

[ -f "$GFX/etc/ld.so.cache" ] || sh "$STEAMRT/build.sh" || die "user/ports/steamrt/build.sh failed"
[ -f "$GFX/etc/ld.so.cache" ] || die "no graphics provider at $GFX"

NEW=$DEST.new X=$DEST.x
rm -rf "$NEW" "$X"
mkdir -p "$NEW/package" "$NEW/bin" "$NEW/LICENSES" "$X"
trap 'rm -rf "$NEW" "$X"' EXIT
bsdtar -xf "$CACHE/$deb" -C "$X" data.tar.xz
bsdtar -xf "$X/data.tar.xz" -C "$X" usr/lib/steam/bootstraplinux_ubuntu12_32.tar.xz usr/share/doc/steam-launcher/copyright usr/share/doc/steam/steam_subscriber_agreement.txt.gz
cp "$X/usr/lib/steam/bootstraplinux_ubuntu12_32.tar.xz" "$NEW/"
cp "$X/usr/share/doc/steam-launcher/copyright" "$NEW/LICENSES/steam-launcher.copyright"
cp "$X/usr/share/doc/steam/steam_subscriber_agreement.txt.gz" "$NEW/LICENSES/"
cp "$M" "$NEW/package/$m_name.manifest"
while read -r _ file _ _; do
  ln "$CACHE/$file" "$NEW/package/$file" 2>/dev/null || cp "$CACHE/$file" "$NEW/package/$file"
done <"$CACHE/packages.txt"
cp "$HERE/files/realpath" "$NEW/bin/realpath"
chmod 755 "$NEW/bin/realpath"
# The session bus: srt-client-container's launcher service will not run without one. This host's dbus-daemon, its libraries
# the Mesa tree lacks, real files because FAT has no links; glibc comes from the Mesa tree next to this one.
mkdir -p "$NEW/dbus"
cp /usr/bin/dbus-daemon "$NEW/dbus/dbus-daemon"
for l in libdbus-1.so.3 libaudit.so.1 libcap-ng.so.0 libsystemd.so.0; do cp -L "/usr/lib/$l" "$NEW/dbus/$l"; done
chmod 755 "$NEW/dbus/dbus-daemon"
cp "$HERE/files/session.conf" "$NEW/dbus/session.conf"
cp /usr/share/licenses/dbus/COPYING "$NEW/LICENSES/dbus.COPYING"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 -C "$(dirname "$GFX")" -cf - gfx | gzip -n -6 >"$NEW/gfx.tar.gz"
rtver=$(bsdtar -xOf "$CACHE/$(awk '$1 == "runtime_steamrt_ubuntu12" { print $2 }' "$CACHE/packages.txt")" steamrt64/pv-runtime/steam-runtime-steamrt.version.txt | awk '$1 == "depot" { print $2 }')
{
  echo "steam-launcher ${deb} ${deb_sha}"
  echo "client ${CHANNEL} ${m_ver} ${m_sha}"
  echo "runtime steamrt3c ${rtver:-unknown}"
  echo "dbus host $(pacman -Q dbus 2>/dev/null | cut -d' ' -f2)"
  echo "gfx user/ports/steamrt $(sha256sum "$NEW/gfx.tar.gz" | cut -d' ' -f1)"
} >"$NEW/VERSION.txt"
[ -z "$(find "$NEW" -type l -print -quit)" ] || die "a link in the tree: FAT has none"
rm -rf "$DEST.old"
[ ! -e "$DEST" ] || mv "$DEST" "$DEST.old"
mv "$NEW" "$DEST"
rm -rf "$DEST.old"
echo "stage-v2: $DEST: $(find "$DEST" -type f | wc -l) files, $(du -sh "$DEST" | cut -f1), client $m_ver"
