#!/usr/bin/env bash
# GM-8: builds the vkQuake tree for BitsOS V2 (\BITSOS2\vkquake\ on the stick, /var/tmp/claude-v2/stage/vkquake here):
#   bin/vkquake     vkQuake from its release tarball, SDL2 build, DO_USERDIRS (config and saves in ~/.vkquake, the stick
#                   stays untouched), patches/ applied. PT_INTERP on the Mesa tree's loader, RPATH on both trees' lib/ (as deqp-vk).
#   lib/            libSDL2 of the pinned release (Wayland, ALSA, Vulkan, the kernel's joystick interface = evdev; no X11, no
#                   udev, no D-Bus, no PipeWire, no Pulse, no hidapi) and what neither SDL nor the game finds in the Mesa
#                   tree: libasound, libwayland-cursor, the Ogg Vorbis libraries, from pacman's cache by their signature.
#   id1/            LibreQuake's free game data (never Georg's; never in git, only here and on the stick).
#   VERSION.txt     what was built, URLs and SHA-256; PACKAGES.txt the pacman packages; LICENSES/ all licence texts.
# Needs the Mesa stage (user/ports/mesa/scripts/stage-stick.sh): the tree runs on its glibc and takes the Vulkan loader,
# RADV, libwayland-client, libxkbcommon from it. Run this again after a Mesa update (the program is linked against the
# host's glibc, the same one the Mesa stage holds; `vkquake --check` and the stage's own check below see a mismatch).
# Takes about 30 s on 20 threads after the downloads (150 MB). Work below out/ (git ignores it); the downloads stay in out/dl,
# the rest goes.
set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
OUT=${VKQ_OUT:-$HERE/out}
STAGE=${V2_STAGE:-/var/tmp/claude-v2/stage}
DEST=${VKQ_STAGE:-$STAGE/vkquake}
MESA=${VKQ_MESA:-$STAGE/mesa}
CACHE=${MESA_PKG_CACHE:-/var/cache/pacman/pkg}
KEYRING=${MESA_KEYRING:-/etc/pacman.d/gnupg/pubring.kbx}
WORK=$OUT/work
DL=$OUT/dl
STICK_LIBS=/boot/BITSOS2/vkquake/lib:/boot/BITSOS2/mesa/lib
LOADER=/boot/BITSOS2/mesa/lib/ld-linux-x86-64.so.2
die() { echo "stage-vkquake: $*" >&2; exit 1; }
for t in cmake ninja gcc patch python3 pkg-config glslangValidator wayland-scanner bsdtar gpgv curl readelf pacman; do
  command -v "$t" >/dev/null || die "no $t on this host"
done
[ -x "$MESA/lib/ld-linux-x86-64.so.2" ] || die "no Mesa stage at $MESA: bash user/ports/mesa/scripts/stage-stick.sh"
[ -r "$KEYRING" ] || die "pacman's keyring $KEYRING cannot be read"

# The pins. A changed release means a new line here, never a moved tag: the SHA-256 is checked on every run.
VKQ_V=1.37.0 VKQ_SHA=955a7777ee9456973b1f3ea278d9f3b34538c788dc5938e42a734fad2e2d761a
VKQ_URL=https://github.com/Novum/vkQuake/archive/refs/tags/$VKQ_V.tar.gz
SDL_V=2.32.10 SDL_SHA=5f5993c530f084535c65a6879e9b26ad441169b3e25d789d83287040a9ca5165
SDL_URL=https://github.com/libsdl-org/SDL/releases/download/release-$SDL_V/SDL2-$SDL_V.tar.gz
LQ_V=v0.09-beta LQ_SHA=623e463b35811216244f9ba15e0c45abc1765288650b5e3d36a899b35e4bcf3d
LQ_URL=https://github.com/lavenderdotpet/LibreQuake/releases/download/$LQ_V/full.zip
# Build tools only, nothing of them lands in the tree: this host has no meson and no Vulkan headers installed.
MESON_V=1.12.1 MESON_SHA=ab0a6ca09f8ef70c564c8241fb5a23957886a0b53fb58412b5e07eaf07dba743
MESON_URL=https://github.com/mesonbuild/meson/releases/download/$MESON_V/meson-$MESON_V.tar.gz
VKH_V=1.4.363 VKH_SHA=cbaf687d3c59b9666fe080e7f8396b6b4f4d344a768ee6b337c59852f4526b68
VKH_URL=https://github.com/KhronosGroup/Vulkan-Headers/archive/refs/tags/v$VKH_V.tar.gz

fetch() { # name url sha: the file in out/dl, checked; prints its path
  local f=$DL/$1
  [ -f "$f" ] || { curl -fsSL -o "$f.part" "$2" || die "download of $2 failed"; mv "$f.part" "$f"; }
  [ "$(sha256sum "$f" | cut -d' ' -f1)" = "$3" ] || die "$f is not the pinned file (SHA-256 $3)"
  echo "$f"
}

NEW=$DEST.new
rm -rf "$NEW" "$DEST.old" "$WORK"
mkdir -p "$DL" "$WORK/pkg" "$NEW/bin" "$NEW/lib" "$NEW/LICENSES/vkquake" "$NEW/LICENSES/sdl2" "$NEW/LICENSES/librequake"
trap 'rm -rf "$NEW"' EXIT
: >"$NEW/PACKAGES.txt"

tar -xf "$(fetch "vkQuake-$VKQ_V.tar.gz" "$VKQ_URL" "$VKQ_SHA")" -C "$WORK"
patch -d "$WORK/vkQuake-$VKQ_V" -p1 -s <"$HERE/patches/sdl2-highdpi.patch" || die "the patch does not fit vkQuake $VKQ_V"
tar -xf "$(fetch "SDL2-$SDL_V.tar.gz" "$SDL_URL" "$SDL_SHA")" -C "$WORK"
tar -xf "$(fetch "meson-$MESON_V.tar.gz" "$MESON_URL" "$MESON_SHA")" -C "$WORK"
tar -xf "$(fetch "Vulkan-Headers-$VKH_V.tar.gz" "$VKH_URL" "$VKH_SHA")" -C "$WORK"
LQ_ZIP=$(fetch "librequake-$LQ_V-full.zip" "$LQ_URL" "$LQ_SHA")
PREFIX=$WORK/sdl-prefix

# SDL2: only what V2 has. Keyboard and mouse come from the compositor (wl_seat), a pad from /dev/input/event* by the
# kernel's joystick interface; without udev SDL finds pads by watching /dev/input (SDL_JOYSTICK_LINUX, inotify).
cmake -S "$WORK/SDL2-$SDL_V" -B "$WORK/sdl-build" -G Ninja -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$PREFIX" \
  -DCMAKE_INSTALL_LIBDIR=lib -DCMAKE_C_FLAGS="-march=x86-64-v3" -DSDL_SHARED=ON -DSDL_STATIC=OFF -DSDL_TEST=OFF -DSDL_RPATH=OFF \
  -DSDL_ALSA=ON -DSDL_ALSA_SHARED=ON -DSDL_JACK=OFF -DSDL_ESD=OFF -DSDL_PIPEWIRE=OFF -DSDL_PULSEAUDIO=OFF -DSDL_ARTS=OFF \
  -DSDL_NAS=OFF -DSDL_SNDIO=OFF -DSDL_LIBSAMPLERATE=OFF -DSDL_X11=OFF -DSDL_WAYLAND=ON -DSDL_WAYLAND_SHARED=ON \
  -DSDL_WAYLAND_LIBDECOR=OFF -DSDL_KMSDRM=OFF -DSDL_VULKAN=ON -DSDL_DBUS=OFF -DSDL_IBUS=OFF -DSDL_FCITX=OFF -DSDL_LIBUDEV=OFF \
  -DSDL_HIDAPI=OFF >"$OUT/sdl-configure.log" 2>&1 || { tail -20 "$OUT/sdl-configure.log"; die "SDL2's configure failed"; }
cmake --build "$WORK/sdl-build" --target install >"$OUT/sdl-build.log" 2>&1 || { tail -20 "$OUT/sdl-build.log"; die "SDL2's build failed"; }
CONF=$PREFIX/include/SDL2/SDL_config.h
for d in SDL_AUDIO_DRIVER_ALSA SDL_VIDEO_DRIVER_WAYLAND SDL_VIDEO_VULKAN SDL_JOYSTICK_LINUX; do
  grep -q "#define $d 1" "$CONF" || die "SDL2 was built without $d"
done
if grep -qE '#define (SDL_USE_LIBUDEV|SDL_VIDEO_DRIVER_X11|SDL_AUDIO_DRIVER_PULSEAUDIO|SDL_AUDIO_DRIVER_PIPEWIRE) 1' "$CONF"; then
  die "SDL2 was built with a part V2 does not have"
fi

# vkQuake: SDL2, music as Ogg Vorbis (what LibreQuake has), no other codec. The tree's loader and search paths go in by a
# last link of the program alone, because meson and the build's own helper (bintoc) must start their programs here, and
# the stick's loader is not here.
BUILD=$WORK/vkq-build
MESON() { PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" python3 "$WORK/meson-$MESON_V/meson.py" "$@"; }
MESON setup "$BUILD" "$WORK/vkQuake-$VKQ_V" --buildtype=release --wrap-mode=nodownload -Duse_sdl3=disabled \
  -Duse_codec_wave=enabled -Duse_codec_vorbis=enabled -Dvorbis_lib=vorbis -Duse_codec_mp3=disabled -Duse_codec_flac=disabled \
  -Duse_codec_opus=disabled -Ddo_userdirs=enabled -Dc_args="-march=x86-64-v3 -I$WORK/Vulkan-Headers-$VKH_V/include" \
  >"$OUT/vkq-configure.log" 2>&1 || { tail -30 "$OUT/vkq-configure.log"; die "vkQuake's configure failed"; }
ninja -C "$BUILD" >"$OUT/vkq-build.log" 2>&1 || { tail -30 "$OUT/vkq-build.log"; die "vkQuake's build failed"; }
# Without meson's own rpath on the work folder's SDL2.
LINK=$(ninja -C "$BUILD" -t commands vkquake | tail -n 1 | sed "s/-Wl,-rpath,[^ ']*//g; s/'-Wl,-rpath,[^']*'//g; s/-Wl,--\(dis\|en\)able-new-dtags//g")
(cd "$BUILD" && rm -f vkquake && eval "$LINK -Wl,--dynamic-linker=$LOADER -Wl,--disable-new-dtags -Wl,-rpath,$STICK_LIBS") >>"$OUT/vkq-build.log" 2>&1 \
  || { tail -30 "$OUT/vkq-build.log"; die "vkQuake's last link failed"; }
cp "$BUILD/vkquake" "$NEW/bin/vkquake"
readelf -l "$NEW/bin/vkquake" | grep -q "interpreter: $LOADER" || die "vkquake's PT_INTERP is not the Mesa tree's loader"
readelf -d "$NEW/bin/vkquake" | grep -q "(RPATH).*\[$STICK_LIBS\]" || die "vkquake's RPATH is not on the trees"
readelf -d "$NEW/bin/vkquake" | grep -q "(NEEDED).*\[libSDL2-2.0.so.0\]" || die "vkquake does not link SDL2"
cp -L "$PREFIX/lib/libSDL2-2.0.so.0" "$NEW/lib/libSDL2-2.0.so.0"
chmod 755 "$NEW/bin/vkquake"

# pacman prints a complaint about an unrelated broken repository on stderr; only stdout is read.
q() { LC_ALL=C pacman "$@" 2>/dev/null; }
# The installed version's package file from the cache or the mirror, checked by its signature (as the Mesa stage).
pkg_file() {
  local ver arch base f url conf
  ver=$(q -Q "$1" | cut -d' ' -f2) || die "package $1 is not installed here (pacman -S $1)"
  arch=$(q -Qi "$1" | sed -n 's/^Architecture *: //p')
  base=$1-$ver-$arch.pkg.tar.zst
  f=$CACHE/$base
  if [ ! -f "$f" ] || [ ! -f "$f.sig" ]; then
    f=$DL/$base
    if [ ! -f "$f" ] || [ ! -f "$f.sig" ]; then
      conf=$WORK/pacman.conf
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
# The file of soname $1 from its owner's package; the package's licence texts and version go with it.
take_lib() {
  local so=$1 owner f id
  owner=$(q -Qoq "/usr/lib/$so") || die "no package here provides /usr/lib/$so"
  if [ ! -d "$WORK/pkg/$owner" ]; then
    f=$(pkg_file "$owner")
    mkdir -p "$WORK/pkg/$owner" "$NEW/LICENSES/$owner"
    bsdtar -xf "$f" -C "$WORK/pkg/$owner" --exclude '.PKGINFO' --exclude '.BUILDINFO' --exclude '.MTREE' --exclude '.INSTALL'
    echo "${f##*/}" >>"$NEW/PACKAGES.txt"
    [ ! -d "$WORK/pkg/$owner/usr/share/licenses/$owner" ] || cp -L "$WORK/pkg/$owner/usr/share/licenses/$owner"/* "$NEW/LICENSES/$owner/"
    for id in $(q -Qi "$owner" | sed -n 's/^Licenses *: //p' | tr '()' '  '); do
      case $id in AND|OR|WITH|custom|custom:*) continue ;; esac
      [ ! -f "/usr/share/licenses/spdx/$id.txt" ] || cp "/usr/share/licenses/spdx/$id.txt" "$NEW/LICENSES/$owner/$id.txt"
    done
    [ -n "$(ls "$NEW/LICENSES/$owner")" ] || die "no licence text for package $owner"
  fi
  [ -e "$WORK/pkg/$owner/usr/lib/$so" ] || die "$owner has no usr/lib/$so"
  cp -L "$WORK/pkg/$owner/usr/lib/$so" "$NEW/lib/$so"
}

# What SDL loads with dlopen (its build wrote the sonames into SDL_config.h) and every DT_NEEDED of the tree's files, until
# nothing new comes: a soname of the Mesa tree stays there, anything else comes into lib/.
QUEUE=$WORK/queue
printf '%s\n' "$NEW/bin/vkquake" "$NEW/lib/libSDL2-2.0.so.0" >"$QUEUE"
for so in $(sed -n 's/^#define SDL_[A-Z_]*_DYNAMIC[A-Z_]* "\(lib[^"]*\)"$/\1/p' "$CONF") libvulkan.so.1; do
  [ -e "$MESA/lib/$so" ] && continue
  take_lib "$so"
  echo "$NEW/lib/$so" >>"$QUEUE"
done
i=1
while f=$(sed -n "${i}p" "$QUEUE") && [ -n "$f" ]; do
  i=$((i + 1))
  for so in $(readelf -d "$f" 2>/dev/null | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
    [ -e "$MESA/lib/$so" ] || [ -e "$NEW/lib/$so" ] && continue
    take_lib "$so"
    echo "$NEW/lib/$so" >>"$QUEUE"
  done
done

# LibreQuake: the game data without its documents (the licence texts go to LICENSES/).
bsdtar -xf "$LQ_ZIP" -C "$WORK" full/id1
cp -r "$WORK/full/id1" "$NEW/id1"
D=$NEW/id1/docs
cp "$D/COPYING" "$NEW/LICENSES/librequake/COPYING-BSD3"
cp "$D/misc-docs/COPYING" "$NEW/LICENSES/librequake/COPYING-GPL2"
cp "$D/README-IMPORTANT-LICENCE-INFO" "$D/CREDITS" "$NEW/LICENSES/librequake/"
rm -rf "$D"
# GPL-2.0: the release is named in VERSION.txt, the one change to it goes along.
cp "$WORK/vkQuake-$VKQ_V/LICENSE.txt" "$HERE"/patches/*.patch "$NEW/LICENSES/vkquake/"
cp "$WORK/SDL2-$SDL_V/LICENSE.txt" "$NEW/LICENSES/sdl2/"
cat >"$NEW/VERSION.txt" <<EOF
vkQuake $VKQ_V GPL-2.0-or-later $VKQ_URL sha256 $VKQ_SHA, patched: $(cd "$HERE/patches" && echo *)
SDL2 $SDL_V Zlib $SDL_URL sha256 $SDL_SHA
LibreQuake $LQ_V BSD-3-Clause, progs GPL-2.0 $LQ_URL sha256 $LQ_SHA
EOF

# The tree resolves inside the two trees: the Mesa tree's loader lists every library from their lib/ only.
LD=$MESA/lib/ld-linux-x86-64.so.2
for f in "$NEW"/bin/* "$NEW"/lib/*; do
  out=$("$LD" --inhibit-cache --library-path "$NEW/lib:$MESA/lib" --list "$f" 2>&1) || die "the loader fails on ${f#"$NEW"/}: $out"
  bad=$(echo "$out" | grep -v -e "=> $NEW/lib/" -e "=> $MESA/lib/" -e 'linux-vdso' -e "^[[:space:]]*$MESA/lib/ld-linux" -e '^[[:space:]]*/lib64/ld-linux' -e "^[[:space:]]*$LOADER" -e 'statically linked' || true)
  [ -z "$bad" ] || die "${f#"$NEW"/} resolves outside the trees: $bad"
done
# FAT: no links, no file of 4 GiB, no names that differ only in case.
[ -z "$(find "$NEW" -type l)" ] || die "the tree holds symbolic links"
[ -z "$(find "$NEW" -type f -size +4095M)" ] || die "a file is too big for FAT"
dup=$(cd "$NEW" && find . | tr '[:upper:]' '[:lower:]' | sort | uniq -d)
[ -z "$dup" ] || die "names that FAT folds together: $dup"

if [ -e "$DEST" ]; then mv "$DEST" "$DEST.old"; fi
mv "$NEW" "$DEST"
rm -rf "$DEST.old" "$WORK"
trap - EXIT
echo "stage-vkquake: $DEST: $(find "$DEST" -type f | wc -l) files, $(du -sm "$DEST" | cut -f1) MB; downloads stay in $DL"
