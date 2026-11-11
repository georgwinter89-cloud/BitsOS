#!/usr/bin/env bash
# WY-3: builds the Xwayland tree for BitsOS V2 (\BITSOS2\xwayland\ on the stick) from the X.Org source releases.
# WY-3b: with glamor, DRI3 and GLX, on the Mesa tree's libraries; bitscomp starts it with XWAYLAND_NO_GLAMOR=1 when it
# cannot use them (user/services/bitscomp/src/xwayland.rs).
#   bin/Xwayland, bin/xkbcomp   built here; PT_INTERP is the Mesa tree's loader and DT_RPATH
#                               $XW_ROOT/lib:$MESA_ROOT/lib, so they run with the Mesa tree's glibc as bitscomp's
#                               gpu build does (never the initramfs glibc)
#   lib/                        the libraries they link that the Mesa tree lacks (the host's files, by soname), and
#                               libGLX_mesa.so.0, which an X11 GL client's libGLX loads by name
#   clients/                    glxgears, glxinfo, xdpyinfo from the distribution's signed packages, for the Mini-PC's
#                               x11gpu.sh (they would be on nobody's stick otherwise); started by `run`
#   run                         starts a program of clients/ or a path with both trees' loader, libraries and drivers
#   share/X11/xkb/              xkeyboard-config's data (xkbcomp compiles the keymaps bitscomp sends)
#   LICENSES/                   the licence texts; SOURCES.txt what was built from what
#   test/bin, test/lib          XW_TEST=1 only: the test clients (xterm, xeyes, xclock, xdpyinfo, xinput, xrandr,
#                               xclip, and wl-copy/wl-paste for the Wayland side of the clipboard) from the
#                               distribution's signed packages; never on the stick
# Sources (each checked by its SHA256 as published in the project's announcement, see ../README.md):
# xwayland, xtrans and xkbcomp from xorg.freedesktop.org, wayland-protocols from gitlab.freedesktop.org, meson from
# its GitHub release. xorgproto and xkeyboard-config come from pacman's cache, checked by their signature.
# Output: $XW_OUT (default $CARGO_TARGET_DIR/wy-xwayland), the tree in $XW_OUT/xwayland. V2_OFFLINE=1: no download.
set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
OUT=${XW_OUT:-${CARGO_TARGET_DIR:-$ROOT/target}/wy-xwayland}
DL=${V2_DOWNLOADS:-$HOME/.cache/bitsos-v2/downloads}
XW_ROOT=${XW_ROOT:-/boot/BITSOS2/xwayland}
MESA_ROOT=${MESA_ROOT:-/boot/BITSOS2/mesa}
STAGE=${MESA_STAGE:-$ROOT/user/ports/mesa/out/mesa}
CACHE=/var/cache/pacman/pkg
KEYRING=/etc/pacman.d/gnupg/pubring.kbx
WORK=$OUT/work
die() { echo "xwayland build: $*" >&2; exit 1; }
for t in readelf bsdtar gpgv curl gcc make ninja python3 pkg-config wayland-scanner; do
  command -v $t >/dev/null || die "no $t on this host"
done
[ -f "$STAGE/lib/ld-linux-x86-64.so.2" ] || die "no Mesa stage in $STAGE (user/ports/mesa/scripts/stage-v2.sh)"

# name|url|sha256 (as published by the project; see ../README.md)
SOURCES=(
  "xwayland-24.1.14.tar.xz|https://xorg.freedesktop.org/releases/individual/xserver/xwayland-24.1.14.tar.xz|4eb6b98d678299a4b96138d886345b294130695760345ef84596225a8224078a"
  "xtrans-1.6.0.tar.xz|https://xorg.freedesktop.org/releases/individual/lib/xtrans-1.6.0.tar.xz|faafea166bf2451a173d9d593352940ec6404145c5d1da5c213423ce4d359e92"
  "xkbcomp-1.5.0.tar.xz|https://xorg.freedesktop.org/releases/individual/app/xkbcomp-1.5.0.tar.xz|2ac31f26600776db6d9cd79b3fcd272263faebac7eb85fb2f33c7141b8486060"
  "wayland-protocols-1.47.tar.xz|https://gitlab.freedesktop.org/wayland/wayland-protocols/-/releases/1.47/downloads/wayland-protocols-1.47.tar.xz|5fd4349bcbc9bab9a46f8cf77d1f434296a7a052c87440a094f63fcf62a58e20"
  "meson-1.12.1.tar.gz|https://github.com/mesonbuild/meson/releases/download/1.12.1/meson-1.12.1.tar.gz|ab0a6ca09f8ef70c564c8241fb5a23957886a0b53fb58412b5e07eaf07dba743"
)
TEST_PKGS="xterm xorg-xeyes xorg-xclock xorg-xdpyinfo xorg-xinput xorg-xrandr xclip wl-clipboard"
TEST_BINS="xterm xeyes xclock xdpyinfo xinput xrandr xclip wl-copy wl-paste"
# WY-3b: on the stick (x11gpu.sh needs them there): package|program...
CLIENTS=("mesa-utils|glxgears glxinfo" "xorg-xdpyinfo|xdpyinfo")
# What an X11 GL client's libGLX loads by name, so nothing lists it as needed.
DLOPEN="libGLX_mesa.so.0"

mkdir -p "$DL" "$WORK/src" "$WORK/pkg" "$WORK/dl"
fetch() {
  local name=$1 url=$2 sum=$3 f=$DL/$1
  if [ ! -f "$f" ]; then
    [ "${V2_OFFLINE:-0}" = 1 ] && die "$name is not in $DL and V2_OFFLINE=1"
    curl -fsSL --max-time 300 -o "$f.part" "$url" || die "download of $url failed"
    mv "$f.part" "$f"
  fi
  echo "$sum  $f" | sha256sum -c --quiet - || die "$f: SHA256 differs from the published one"
}
unpack() {
  local name=$1 dir=$WORK/src/${1%.tar.*}
  [ -d "$dir" ] || { mkdir -p "$dir.x"; tar -xf "$DL/$name" -C "$dir.x" --strip-components=1; mv "$dir.x" "$dir"; }
}
for s in "${SOURCES[@]}"; do IFS='|' read -r n u h <<<"$s"; fetch "$n" "$u" "$h"; unpack "$n"; done
MESON="python3 -I $WORK/src/meson-1.12.1/meson.py"

# A signed package (pacman's cache, else the configured mirror), unpacked in its own directory. Prints the directory.
q() { LC_ALL=C pacman "$@" 2>/dev/null; }
pkg() {
  local name=$1 url base f
  [ -d "$WORK/pkg/$name" ] && { echo "$WORK/pkg/$name"; return; }
  url=$(LC_ALL=C pacman -Sp --print-format '%l' "$name" 2>/dev/null | tail -n 1)
  [ -n "$url" ] || die "pacman knows no package $name"
  base=${url##*/}
  f=$CACHE/$base
  if [ ! -f "$f" ] || [ ! -f "$f.sig" ]; then
    f=$WORK/dl/$base
    if [ ! -f "$f" ] || [ ! -f "$f.sig" ]; then
      [ "${V2_OFFLINE:-0}" = 1 ] && die "$base is not cached and V2_OFFLINE=1"
      curl -fsSL -o "$f.part" "$url" && curl -fsSL -o "$f.sig" "$url.sig" || die "download of $url failed"
      mv "$f.part" "$f"
    fi
  fi
  gpgv --keyring "$KEYRING" "$f.sig" "$f" >/dev/null 2>&1 || die "$f: its signature does not check out"
  mkdir -p "$WORK/pkg/$name.x"
  bsdtar -xf "$f" -C "$WORK/pkg/$name.x"
  mv "$WORK/pkg/$name.x" "$WORK/pkg/$name"
  echo "$base" >>"$WORK/packages.txt"
  echo "$WORK/pkg/$name"
}

# Build-only headers: xorgproto (package), xtrans and wayland-protocols (sources) into $WORK/deps.
DEPS=$WORK/deps
if [ ! -f "$DEPS/.done" ]; then
  rm -rf "$DEPS"; mkdir -p "$DEPS/share/pkgconfig" "$DEPS/include"
  p=$(pkg xorgproto)
  cp -r "$p/usr/include/." "$DEPS/include/"
  for pc in "$p"/usr/share/pkgconfig/*.pc; do sed "s|^prefix=.*|prefix=$DEPS|" "$pc" >"$DEPS/share/pkgconfig/${pc##*/}"; done
  (cd "$WORK/src/xtrans-1.6.0" && ./configure -q --prefix="$DEPS" >/dev/null 2>&1 && make -s install >/dev/null)
  (cd "$WORK/src/wayland-protocols-1.47" && $MESON setup --prefix="$DEPS" -Dtests=false "$WORK/b-wp" >/dev/null \
    && ninja -C "$WORK/b-wp" install >/dev/null)
  touch "$DEPS/.done"
fi
export PKG_CONFIG_PATH=$DEPS/share/pkgconfig:$DEPS/lib/pkgconfig

LINK="-Wl,--dynamic-linker=$MESA_ROOT/lib/ld-linux-x86-64.so.2 -Wl,--disable-new-dtags -Wl,-rpath,$XW_ROOT/lib:$MESA_ROOT/lib"
B=$WORK/b-xwayland
# A build dir of the software tree (WY-3) would give the old binary without a word: the option that changes marks it.
if [ ! -f "$B/hw/xwayland/Xwayland" ] || [ ! -f "$B/wy3b-glamor" ]; then
  rm -rf "$B"
  $MESON setup "$B" "$WORK/src/xwayland-24.1.14" --buildtype=release --prefix="$XW_ROOT" \
    -Dglamor=true -Ddri3=true -Ddrm=true -Dglx=true -Dlibdecor=false -Dxwayland_ei=false -Dxvfb=false \
    -Dsecure-rpc=false -Dxdmcp=false -Dxdm-auth-1=false -Dlisten_tcp=false -Dsystemd_notify=false \
    -Dxselinux=false -Dlibunwind=false -Ddocs=false -Ddevel-docs=false -Ddocs-pdf=false -Dsha1=libnettle \
    -Dxkb_dir="$XW_ROOT/share/X11/xkb" -Dxkb_bin_dir="$XW_ROOT/bin" -Dxkb_output_dir=/tmp \
    -Ddefault_font_path=built-ins -Dxkb_default_layout=de >"$WORK/meson-xwayland.log" 2>&1 \
    || { tail -30 "$WORK/meson-xwayland.log"; die "meson setup of Xwayland failed"; }
  ninja -C "$B" hw/xwayland/Xwayland >"$WORK/ninja-xwayland.log" 2>&1 || { tail -30 "$WORK/ninja-xwayland.log"; die "Xwayland build failed"; }
  # Linked again with the tree's loader and search path: only the last step, since meson and configure run their
  # test programs on the host, where the tree's loader does not exist.
  cmd=$(ninja -C "$B" -t commands hw/xwayland/Xwayland | tail -n 1)
  (cd "$B" && eval "$cmd $LINK") || die "linking Xwayland for the tree failed"
  touch "$B/wy3b-glamor"
fi
K=$WORK/src/xkbcomp-1.5.0
if [ ! -f "$K/xkbcomp" ]; then
  (cd "$K" && ./configure -q --prefix="$XW_ROOT" --with-xkb-config-root="$XW_ROOT/share/X11/xkb" >/dev/null \
    && make -s >/dev/null 2>"$WORK/make-xkbcomp.log" && rm xkbcomp && make -s LDFLAGS="$LINK" >/dev/null 2>>"$WORK/make-xkbcomp.log") \
    || { tail -20 "$WORK/make-xkbcomp.log"; die "xkbcomp build failed"; }
fi

T=$OUT/xwayland
NEW=$T.new
rm -rf "$NEW"; mkdir -p "$NEW/bin" "$NEW/lib" "$NEW/share/X11" "$NEW/LICENSES"
trap 'rm -rf "$NEW"' EXIT
cp "$B/hw/xwayland/Xwayland" "$K/xkbcomp" "$NEW/bin/"
strip "$NEW/bin/Xwayland" "$NEW/bin/xkbcomp"
kb=$(pkg xkeyboard-config)
cp -rL "$kb/usr/share/xkeyboard-config-2" "$NEW/share/X11/xkb"
mkdir -p "$NEW/LICENSES/xkeyboard-config"; cp "$kb"/usr/share/licenses/xkeyboard-config/* "$NEW/LICENSES/xkeyboard-config/" 2>/dev/null || true
for s in xwayland-24.1.14 xkbcomp-1.5.0; do mkdir -p "$NEW/LICENSES/$s"; cp "$WORK/src/$s/COPYING" "$NEW/LICENSES/$s/"; done

# Every library of $2.. that neither the Mesa stage nor a directory of $1 has goes into $1 (the host's file, by
# soname, no links), with its licence; walked to the end.
needed() { readelf -d "$1" 2>/dev/null | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'; }
walk() {
  local dest=$1; shift
  local todo=("$@") f so src owner
  while [ ${#todo[@]} -gt 0 ]; do
    f=${todo[0]}; todo=("${todo[@]:1}")
    for so in $(needed "$f"); do
      [ -f "$STAGE/lib/$so" ] || [ -f "$NEW/lib/$so" ] || [ -f "$dest/$so" ] && continue
      src=/usr/lib/$so
      [ -f "$src" ] || die "$so (for ${f##*/}): neither in the Mesa stage nor on the host"
      cp -L "$src" "$dest/$so"
      owner=$(q -Qoq "$src" || echo unknown)
      if [ -d "/usr/share/licenses/$owner" ] && [ ! -d "$NEW/LICENSES/$owner" ]; then cp -rL "/usr/share/licenses/$owner" "$NEW/LICENSES/"; fi
      echo "$so $owner" >>"$NEW/LIBS.txt"
      todo+=("$dest/$so")
    done
  done
}
walk "$NEW/lib" "$NEW/bin/Xwayland" "$NEW/bin/xkbcomp"
# WY-3b: libepoxy dlopens the Mesa tree's libEGL and libGL by name, so Xwayland's RPATH finds them.
for so in $DLOPEN; do
  [ -f "$STAGE/lib/$so" ] && continue
  src=/usr/lib/$so
  [ -f "$src" ] || die "$so: neither in the Mesa stage nor on the host"
  # The host's Mesa must be the stage's: else `walk` would add the host's whole driver next to the tree's.
  for g in $(needed "$src" | grep '^libgallium'); do
    [ -f "$STAGE/lib/$g" ] || die "$so needs $g, the stage has another Mesa (user/ports/mesa/scripts/stage-stick.sh again)"
  done
  cp -L "$src" "$NEW/lib/$so"
  owner=$(q -Qoq "$src" || echo unknown)
  [ -d "/usr/share/licenses/$owner" ] && [ ! -d "$NEW/LICENSES/$owner" ] && cp -rL "/usr/share/licenses/$owner" "$NEW/LICENSES/"
  echo "$so $owner" >>"$NEW/LIBS.txt"
  walk "$NEW/lib" "$NEW/lib/$so"
done
mkdir -p "$NEW/clients"
for c in "${CLIENTS[@]}"; do
  d=$(pkg "${c%%|*}")
  for b in ${c#*|}; do
    [ -f "$d/usr/bin/$b" ] || die "no $b in the package ${c%%|*}"
    cp "$d/usr/bin/$b" "$NEW/clients/"
  done
  [ -d "$d/usr/share/licenses/${c%%|*}" ] && cp -r "$d/usr/share/licenses/${c%%|*}" "$NEW/LICENSES/"
done
walk "$NEW/lib" "$NEW"/clients/*
cp "$HERE/bitsos/run" "$NEW/run"

if [ "${XW_TEST:-0}" = 1 ]; then
  mkdir -p "$NEW/test/bin" "$NEW/test/lib"
  for p in $TEST_PKGS; do
    d=$(pkg "$p")
    for b in $TEST_BINS; do [ -f "$d/usr/bin/$b" ] && cp "$d/usr/bin/$b" "$NEW/test/bin/"; done
    [ -d "$d/usr/share/licenses/$p" ] && cp -r "$d/usr/share/licenses/$p" "$NEW/LICENSES/"
  done
  for b in $TEST_BINS; do [ -f "$NEW/test/bin/$b" ] || die "no $b in the test packages"; done
  walk "$NEW/test/lib" "$NEW"/test/bin/*
fi

# Checks: the tree's loader resolves both programs inside the two trees; no two names fold together on FAT.
LD=$STAGE/lib/ld-linux-x86-64.so.2
for b in "$NEW"/bin/Xwayland "$NEW"/bin/xkbcomp "$NEW"/lib/libGLX_mesa.so.0 "$NEW"/clients/* "$NEW"/test/bin/*; do
  [ -f "$b" ] || continue
  lp=$NEW/lib:$STAGE/lib; case $b in */test/*) lp=$NEW/test/lib:$lp ;; esac
  out=$("$LD" --inhibit-cache --library-path "$lp" --list "$b" 2>&1) || die "the tree's loader cannot load $b: $out"
  miss=$(echo "$out" | grep -v -e "=> $NEW/" -e "=> $STAGE/lib/" -e "linux-vdso" -e "^[[:space:]]*/" || true)
  [ -z "$miss" ] || die "libraries of ${b##*/} outside the trees: $miss"
done
dup=$(cd "$NEW" && find . | tr '[:upper:]' '[:lower:]' | sort | uniq -d | head -3)
[ -z "$dup" ] || die "names that fold together on FAT: $dup"
[ -z "$(find "$NEW" -type l)" ] || die "links in the tree (FAT has none)"
{
  for s in "${SOURCES[@]}"; do IFS='|' read -r n u h <<<"$s"; echo "$n $u sha256:$h"; done
  sort -u "$WORK/packages.txt" | sed 's/^/package /'
  echo "built with: $(gcc --version | head -1); PT_INTERP $MESA_ROOT/lib/ld-linux-x86-64.so.2; RPATH $XW_ROOT/lib:$MESA_ROOT/lib"
} >"$NEW/SOURCES.txt"
rm -rf "$T"; mv "$NEW" "$T"; trap - EXIT
echo "xwayland build: $T ($(du -sh "$T" | cut -f1), $(ls "$T/lib" | wc -l) libraries$([ -d "$T/test" ] && echo ", test clients: $(ls "$T/test/bin" | tr '\n' ' ')"))"
