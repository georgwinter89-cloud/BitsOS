#!/usr/bin/env bash
# GM-1a: builds the Mesa tree for BitsOS V2 (\BITSOS2\mesa\ on the stick) from the host's pacman packages: Mesa
# (llvmpipe, the radeonsi/RADV drivers, EGL, GBM), lavapipe and RADV (vulkan-swrast, vulkan-radeon), libglvnd, the
# Vulkan loader, vulkan-tools (vulkaninfo, vkcube), mesa-utils (eglinfo, the Wayland and pbuffer demos, vkgears) and every
# library they and their drivers link, glibc and its loader included. The tree carries its own glibc, so it never
# mixes with the system's libraries; `run` (bitsos/run) starts a program with the tree's loader.
#   bin/            the programs
#   lib/            ld-linux-x86-64.so.2, glibc, every library by its soname (no links: FAT has none)
#   lib/gbm/        dri_gbm.so (libgbm's backend, dlopened)
#   lib/dri/        swrast, kms_swrast and radeonsi _dri.so (libdril, for loaders that still look there)
#   share/vulkan/icd.d/  lavapipe's and RADV's manifests (library_path is a bare soname: found in lib/)
#   share/glvnd/egl_vendor.d/50_mesa.json   glvnd's EGL vendor file (libEGL_mesa.so.0)
#   share/drirc.d/  Mesa's driver defaults for these drivers
#   share/libdrm/amdgpu.ids   libdrm_amdgpu's table of AMD product names (run points AMDGPU_ASIC_ID_TABLE_PATHS here)
#   run             the start script; PACKAGES.txt the package versions; LICENSES/ each package's licence texts
# The packages come from pacman's cache (the versions installed here), each checked against its signature with
# pacman's keyring; one missing from the cache is downloaded from the configured mirror into the work directory.
# Nothing is installed and nothing needs root. The packages are CachyOS's x86-64-v3 builds of Arch's (AVX2).
# Run it again after a Mesa update (pacman -Syu). Output: user/ports/mesa/out/ (git ignores it).
set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
OUT=${MESA_OUT:-$HERE/out}
DEST=${MESA_STAGE:-$OUT/mesa}
WORK=$OUT/work
CACHE=${MESA_PKG_CACHE:-/var/cache/pacman/pkg}
KEYRING=${MESA_KEYRING:-/etc/pacman.d/gnupg/pubring.kbx}
die() { echo "stage-v2: $*" >&2; exit 1; }
for t in pacman readelf bsdtar gpgv curl; do command -v $t >/dev/null || die "no $t on this host"; done
[ -r "$KEYRING" ] || die "pacman's keyring $KEYRING cannot be read"

# WY-14: pixman (libpixman-1.so.0) and libxkbcommon (libxkbcommon.so.0) for the gpu build of bitscomp, named on their own
# (libxkbcommon came only as a dependency of vulkan-tools before).
SEED="mesa vulkan-swrast vulkan-radeon libglvnd vulkan-icd-loader vulkan-tools mesa-utils libdrm wayland glibc gcc-libs pixman libxkbcommon"
# GM-1d: of mesa-utils everything that needs no KMS (eglkms) and, since WY-3b, the X11 programs of glxgears and glxinfo.
PROGS="vulkaninfo vkcube vkgears eglinfo eglgears_wayland egltri_wayland es2gears_wayland peglgears glxgears glxinfo"
# Loaded with dlopen, so no program names them: the Vulkan ICDs (by the loader), Mesa's EGL vendor (by glvnd),
# GBM's backend, the GLES library eglinfo probes, Mesa's GLX vendor (WY-3b: any X11 GL program needs it next to libGLX).
DLOPEN="libvulkan.so.1 libvulkan_lvp.so libvulkan_radeon.so libEGL_mesa.so.0 libGLX_mesa.so.0 libGLESv2.so.2 libGL.so.1 libwayland-egl.so.1"
# WY-14 (INT-W24): what bitscomp's gpu build links beyond the programs above (no program here names them), resolved
# the same way.
EXTRA_SONAMES="libpixman-1.so.0 libxkbcommon.so.0"

NEW=$DEST.new
rm -rf "$NEW" "$DEST.old"
mkdir -p "$WORK/pkg" "$WORK/dl" "$NEW/bin" "$NEW/lib/gbm" "$NEW/lib/dri" "$NEW/share/vulkan/icd.d" \
  "$NEW/share/glvnd/egl_vendor.d" "$NEW/share/drirc.d" "$NEW/share/libdrm" "$NEW/LICENSES"
trap 'rm -rf "$NEW"' EXIT

# pacman prints a complaint about an unrelated broken repository on stderr; only stdout is read.
q() { LC_ALL=C pacman "$@" 2>/dev/null; }

# The package file of the installed version of $1: from the cache, else downloaded (with a pacman.conf that has
# the configured repositories, so -Sp works without root), checked by its signature either way. Prints the path.
pkg_file() {
  local ver arch base f url conf
  ver=$(q -Q "$1" | cut -d' ' -f2) || die "package $1 is not installed here (pacman -S $1)"
  arch=$(q -Qi "$1" | sed -n 's/^Architecture *: //p')
  base=$1-$ver-$arch.pkg.tar.zst
  f=$CACHE/$base
  if [ ! -f "$f" ] || [ ! -f "$f.sig" ]; then
    f=$WORK/dl/$base
    if [ ! -f "$f" ] || [ ! -f "$f.sig" ]; then
      conf=$WORK/pacman.conf
      # Only the repositories whose sync database is valid; the cache is the read-only host one.
      sed '/^Include *= *\/etc\/pacman.d\/openai/d' /etc/pacman.conf >"$conf"
      url=$(LC_ALL=C pacman --config "$conf" -Sp --print-format '%l' "$1" 2>/dev/null | tail -n 1)
      [ "${url##*/}" = "$base" ] || die "$1: the mirror offers ${url##*/}, the host has $base (pacman -Syu first)"
      curl -fsSL -o "$f.part" "$url" && curl -fsSL -o "$f.sig" "$url.sig" || die "download of $url failed"
      mv "$f.part" "$f"
    fi
  fi
  gpgv --keyring "$KEYRING" "$f.sig" "$f" >/dev/null 2>&1 || die "$f: its signature does not check out"
  echo "$f"
}

# Each package in its own directory (to know which package a file came from).
have_pkg() { [ -d "$WORK/pkg/$1" ]; }
add_pkg() {
  have_pkg "$1" && return 0
  local f
  f=$(pkg_file "$1")
  mkdir -p "$WORK/pkg/$1.x"
  bsdtar -xf "$f" -C "$WORK/pkg/$1.x" --exclude '.PKGINFO' --exclude '.BUILDINFO' --exclude '.MTREE' --exclude '.INSTALL'
  mv "$WORK/pkg/$1.x" "$WORK/pkg/$1"
  echo "$1 ${f##*/}" >>"$WORK/used"
}
# The package directory holding usr/lib/$1 (a link inside it counts), else the owner on the host, fetched.
find_lib() {
  local d owner
  for d in "$WORK"/pkg/*/; do
    [ -e "$d/usr/lib/$1" ] && { echo "$d/usr/lib/$1"; return 0; }
  done
  owner=$(q -Qoq "/usr/lib/$1") || die "no package here provides /usr/lib/$1"
  add_pkg "$owner"
  [ -e "$WORK/pkg/$owner/usr/lib/$1" ] || die "$owner has no usr/lib/$1"
  echo "$WORK/pkg/$owner/usr/lib/$1"
}

# The file $1 (below the package root) from the first package that has it.
one() {
  local d
  for d in "$WORK"/pkg/*/; do [ ! -e "$d$1" ] || { echo "$d$1"; return 0; }; done
  die "no package has $1"
}

rm -rf "$WORK/pkg"
mkdir -p "$WORK/pkg"
: >"$WORK/used"
for p in $SEED; do add_pkg "$p"; done

QUEUE=$WORK/queue
: >"$QUEUE"
for p in $PROGS; do
  cp -L "$(one "usr/bin/$p")" "$NEW/bin/$p"
  echo "$NEW/bin/$p" >>"$QUEUE"
done
cp -L "$(find_lib ld-linux-x86-64.so.2)" "$NEW/lib/ld-linux-x86-64.so.2"
for so in $DLOPEN $EXTRA_SONAMES; do
  cp -L "$(find_lib "$so")" "$NEW/lib/$so"
  echo "$NEW/lib/$so" >>"$QUEUE"
done
cp -L "$(find_lib gbm/dri_gbm.so)" "$NEW/lib/gbm/dri_gbm.so"
echo "$NEW/lib/gbm/dri_gbm.so" >>"$QUEUE"
for d in swrast kms_swrast radeonsi; do
  cp -L "$(find_lib "dri/${d}_dri.so")" "$NEW/lib/dri/${d}_dri.so"
done
echo "$NEW/lib/dri/swrast_dri.so" >>"$QUEUE"

# Every DT_NEEDED of every ELF in the queue, by soname into lib/, until nothing new comes.
i=1
while f=$(sed -n "${i}p" "$QUEUE") && [ -n "$f" ]; do
  i=$((i + 1))
  for so in $(readelf -d "$f" 2>/dev/null | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
    [ -e "$NEW/lib/$so" ] && continue
    cp -L "$(find_lib "$so")" "$NEW/lib/$so"
    echo "$NEW/lib/$so" >>"$QUEUE"
  done
done

for j in lvp_icd radeon_icd; do
  f=$(one "usr/share/vulkan/icd.d/$j.json")
  grep -q '"library_path": *"[^"/]*"' "$f" || die "$f names its driver by a path: the tree needs a bare soname"
  cp "$f" "$NEW/share/vulkan/icd.d/"
done
f=$(one usr/share/glvnd/egl_vendor.d/50_mesa.json)
grep -q '"library_path" *: *"libEGL_mesa.so.0"' "$f" || die "$f does not name libEGL_mesa.so.0"
cp "$f" "$NEW/share/glvnd/egl_vendor.d/"
for c in 00-mesa-defaults 00-radv-defaults 00-lavapipe-defaults 00-radeonsi-defaults; do
  cp "$(one "usr/share/drirc.d/$c.conf")" "$NEW/share/drirc.d/"
done
# GM-1c: without it RADV and radeonsi say "/usr/share/libdrm/amdgpu.ids: No such file" and name no product.
cp "$(one usr/share/libdrm/amdgpu.ids)" "$NEW/share/libdrm/"
cp "$HERE/bitsos/run" "$NEW/run"
chmod 755 "$NEW/run" "$NEW/bin/"* "$NEW/lib/ld-linux-x86-64.so.2"

# The tree resolves inside itself: the loader lists every library of every ELF from lib/ only.
LD=$NEW/lib/ld-linux-x86-64.so.2
for f in "$NEW"/bin/* "$NEW"/lib/*.so* "$NEW"/lib/gbm/* "$NEW"/lib/dri/*; do
  [ "$f" != "$LD" ] || continue
  out=$("$LD" --inhibit-cache --library-path "$NEW/lib" --list "$f" 2>&1) || die "the loader fails on ${f#$NEW/}: $out"
  bad=$(echo "$out" | grep -v -e "=> $NEW/lib/" -e 'linux-vdso' -e "^[[:space:]]*$NEW/lib/ld-linux" -e '^[[:space:]]*/lib64/ld-linux' -e 'statically linked' || true)
  [ -z "$bad" ] || die "${f#$NEW/} resolves outside the tree: $bad"
done
# FAT: names that differ only in case are one name there.
dup=$(cd "$NEW" && find . | tr '[:upper:]' '[:lower:]' | sort | uniq -d)
[ -z "$dup" ] || die "names that FAT folds together: $dup"

# Package list and licences: each package that gave a file, its own texts and the SPDX texts pacman names.
used=""
for f in $(find "$NEW/bin" "$NEW/lib" -type f) $(find "$NEW/share" -type f); do
  rel=${f#$NEW/}
  case $rel in
    bin/*) src=usr/$rel ;;
    lib/*) src=usr/$rel ;;
    share/*) src=usr/$rel ;;
  esac
  for d in "$WORK"/pkg/*/; do
    if [ -e "$d$src" ]; then used="$used $(basename "$d")"; break; fi
  done
done
used=$(echo "$used" | tr ' ' '\n' | grep . | sort -u)
for p in $used; do
  mkdir "$NEW/LICENSES/$p"
  [ ! -d "$WORK/pkg/$p/usr/share/licenses/$p" ] || cp -L "$WORK/pkg/$p/usr/share/licenses/$p"/* "$NEW/LICENSES/$p/"
  for id in $(q -Qi "$p" | sed -n 's/^Licenses *: //p' | tr '()' '  '); do
    case $id in AND|OR|WITH|custom|custom:*) continue ;; esac
    [ ! -f "/usr/share/licenses/spdx/$id.txt" ] || cp "/usr/share/licenses/spdx/$id.txt" "$NEW/LICENSES/$p/$id.txt"
  done
  [ -n "$(ls "$NEW/LICENSES/$p")" ] || die "no licence text for package $p"
  grep "^$p " "$WORK/used" | cut -d' ' -f2 >>"$NEW/PACKAGES.txt"
done

if [ -e "$DEST" ]; then mv "$DEST" "$DEST.old"; fi
mv "$NEW" "$DEST"
rm -rf "$DEST.old"
trap - EXIT
echo "stage-v2: $DEST: $(echo "$used" | wc -l) packages, $(find "$DEST" -type f | wc -l) files, $(du -sm "$DEST" | cut -f1) MB"
