#!/bin/sh
# SX-3c: the Steam Linux Runtime 3.0 (sniper) with pressure-vessel for the V2 scenario, plus the software graphics
# provider pressure-vessel takes the drivers from. Output in out/ (git ignores it):
#   out/SteamLinuxRuntime_sniper/   Valve's tree as published (the arm64 pressure-vessel left out)
#   out/gfx/                        a small Arch-shaped sysroot: the GM-1a Mesa tree (llvmpipe, lavapipe, RADV,
#                                   radeonsi, glvnd, the Vulkan loader) and the whole glibc of the same package
#                                   (pressure-vessel takes the newer glibc of runtime and provider, and with it every
#                                   glibc library and ldconfig), usr/lib + lib, lib64 -> usr/lib, an ld.so.cache
# The runtime tarball comes from repo.steampowered.com (pinned snapshot, SHA256 = Valve's SHA256SUMS), the Mesa tree
# from user/ports/mesa/scripts/stage-v2.sh (pacman's cache, each package signature-checked), glibc from the same
# signature-checked package the Mesa tree took its libc from.
# Idempotent: does nothing when out/ is newer than this script. V2_DOWNLOADS overrides the download cache,
# V2_OFFLINE=1 refuses to download.
set -eu
SLR_VER=3.0.20261021.266025
SLR_SHA=86956ad0ee62e89134f5e30359cd1019d1ecb614c7b677dad82d4d00f51a2696
SLR_URL=https://repo.steampowered.com/steamrt-images-sniper/snapshots/$SLR_VER/SteamLinuxRuntime_sniper.tar.xz
SLR_FILE=SteamLinuxRuntime_sniper-$SLR_VER.tar.xz

here=$(cd "$(dirname "$0")" && pwd)
cache=${V2_DOWNLOADS:-$HOME/.cache/bitsos-v2/downloads}
out=$here/out
slr=$out/SteamLinuxRuntime_sniper
gfx=$out/gfx
die() { echo "steamrt: $*" >&2; exit 1; }

if [ -x "$slr/run" ] && [ -f "$gfx/etc/ld.so.cache" ] && [ "$gfx/etc/ld.so.cache" -nt "$0" ] \
    && grep -q "$SLR_VER" "$slr/VERSIONS.txt"; then
  echo "steamrt: up to date ($out)"
  exit 0
fi
for t in curl sha256sum tar xz bsdtar gpgv ldconfig; do command -v $t >/dev/null || die "no $t on this host"; done

mkdir -p "$cache"
if [ ! -f "$cache/$SLR_FILE" ]; then
  [ "${V2_OFFLINE:-0}" = 1 ] && die "$cache/$SLR_FILE missing and V2_OFFLINE=1"
  echo "steamrt: downloading $SLR_URL (about 190 MB)"
  curl -fsSL --max-time 1800 -o "$cache/$SLR_FILE.part" "$SLR_URL"
  mv "$cache/$SLR_FILE.part" "$cache/$SLR_FILE"
fi
echo "$SLR_SHA  $cache/$SLR_FILE" | sha256sum -c --quiet - || die "SHA256 mismatch for $cache/$SLR_FILE (expected $SLR_SHA)"

rm -rf "$slr" "$gfx"
mkdir -p "$out"
tar -C "$out" -xJf "$cache/$SLR_FILE"
rm -rf "$slr/pressure-vessel-arm64"

# The Mesa tree (GM-1a), staged into our out/ so user/ports/mesa/out stays the GM lane's.
mesa=$out/mesa-stage/mesa
if [ ! -f "$mesa/PACKAGES.txt" ]; then
  MESA_OUT=$out/mesa-stage bash "$here/../mesa/scripts/stage-v2.sh" > "$out/mesa-stage.log" 2>&1 \
    || { tail -5 "$out/mesa-stage.log" >&2; die "Mesa stage failed (log $out/mesa-stage.log)"; }
fi
glibc_pkg=$(grep '^glibc-' "$mesa/PACKAGES.txt" | head -1)
[ -n "$glibc_pkg" ] || die "the Mesa tree names no glibc package"
pkg=${MESA_PKG_CACHE:-/var/cache/pacman/pkg}/$glibc_pkg
[ -f "$pkg" ] || pkg=$out/mesa-stage/work/pkg/$glibc_pkg
[ -f "$pkg" ] && [ -f "$pkg.sig" ] || die "$glibc_pkg is neither in pacman's cache nor in the Mesa stage's work directory"
gpgv --keyring "${MESA_KEYRING:-/etc/pacman.d/gnupg/pubring.kbx}" "$pkg.sig" "$pkg" >/dev/null 2>&1 \
  || die "$pkg: its signature does not check out"

mkdir -p "$gfx/usr/lib" "$gfx/usr/bin" "$gfx/usr/share/vulkan/icd.d" "$gfx/etc" "$out/glibc.x"
cp -a "$mesa/lib/." "$gfx/usr/lib/"
cp -a "$mesa/share/drirc.d" "$mesa/share/glvnd" "$gfx/usr/share/"
cp -a "$mesa/LICENSES" "$gfx/usr/share/licenses"
# Arch's manifests name the driver by absolute path; the tree's by soname (its run script sets the paths).
for f in "$mesa"/share/vulkan/icd.d/*.json; do
  sed 's#"library_path": "\([^/"]*\)"#"library_path": "/usr/lib/\1"#' "$f" > "$gfx/usr/share/vulkan/icd.d/${f##*/}"
done
rm -rf "$out/glibc.x"; mkdir -p "$out/glibc.x"
bsdtar -xf "$pkg" -C "$out/glibc.x" --exclude usr/lib/getconf usr/lib usr/bin/ldconfig usr/bin/ldd usr/bin/locale usr/bin/localedef
for f in "$out"/glibc.x/usr/lib/*.so.* "$out"/glibc.x/usr/lib/ld-linux-x86-64.so.2; do
  [ -e "$gfx/usr/lib/${f##*/}" ] || cp -a "$f" "$gfx/usr/lib/"
done
cmp -s "$out/glibc.x/usr/lib/libc.so.6" "$gfx/usr/lib/libc.so.6" || die "the Mesa tree's libc.so.6 is not $glibc_pkg's"
cp -a "$out"/glibc.x/usr/bin/* "$gfx/usr/bin/"
rm -rf "$out/glibc.x"
ln -s usr/lib "$gfx/lib"
ln -s usr/lib "$gfx/lib64"
ln -s lib "$gfx/usr/lib64"
ln -s usr/bin "$gfx/bin"
ln -s usr/bin "$gfx/sbin"
ln -s bin "$gfx/usr/sbin"
# Mesa's GLX vendor library lives in the Xwayland tree (WY-3b), not in the Mesa tree. Without it the runtime's own older one stays
# in the container, and that cannot load this provider's drivers ("image driver extension not found").
xw=${XWAYLAND_STAGE:-/var/tmp/claude-v2/stage/xwayland}/lib
for l in libGLX_mesa.so.0 libxcb-glx.so.0 libXext.so.6 libXxf86vm.so.1; do
  cp -L "$xw/$l" "$gfx/usr/lib/$l" || die "no $l in $xw: stage the Xwayland tree first"
done
: > "$gfx/etc/ld.so.conf"
ldconfig -r "$gfx" -X
echo "steamrt: $out: sniper $SLR_VER, provider $(du -sm "$gfx" | cut -f1) MB, runtime $(du -sm "$slr" | cut -f1) MB"
