#!/bin/sh
# SX-3b: bubblewrap 0.13.1 for BitsOS V2 (static-pie, UL-1 musl sysroot, x86-64-v3) plus what its own test suite
# needs and V2 lacks: GNU bash 5.3 (static) and small helpers of our own (tools/). Pinned tarballs from the download
# cache, SHA256 checked on every build. Output in out/: bwrap, bash, getpcaps, findmnt, df, bwrap-lockf,
# bwrap-pdeath-probe and out/tests/ (the suite with patches/*.patch applied, see README.md).
# Idempotent: rebuilds only when out/ is older than this script, a patch, the shim or a tool.
# V2_DOWNLOADS overrides the download cache, V2_OFFLINE=1 refuses to download, V2_JOBS caps the jobs.
set -eu
BW_VER=0.13.1
BW_SHA=0e85c8702248b8dd64144f594147553650cdd0e4c951690345faace79c217c4d
BW_URL=https://github.com/containers/bubblewrap/releases/download/v$BW_VER/bubblewrap-$BW_VER.tar.xz
MESON_VER=1.12.1
MESON_SHA=ab0a6ca09f8ef70c564c8241fb5a23957886a0b53fb58412b5e07eaf07dba743
MESON_URL=https://github.com/mesonbuild/meson/releases/download/$MESON_VER/meson-$MESON_VER.tar.gz
BASH_VER=5.3
BASH_SHA=0d5cd86965f869a26cf64f4b71be7b96f90a3ba8b3d74e27e8e9d9d5550f31ba
BASH_URL=https://ftp.gnu.org/gnu/bash/bash-$BASH_VER.tar.gz

here=$(cd "$(dirname "$0")" && pwd)
cache=${V2_DOWNLOADS:-$HOME/.cache/bitsos-v2/downloads}
out=$here/out
work=$here/build
cc=$here/../../sysroot/bin/v2-cc
jobs=${V2_JOBS:-8}

newest=$(ls -t "$0" "$here"/patches/* "$here"/libcap-shim/* "$here"/libcap-shim/sys/* "$here"/tools/* | head -1)
if [ -x "$out/bwrap" ] && [ -x "$out/bash" ] && [ -f "$out/tests/test-run.sh" ] && [ "$out/bwrap" -nt "$newest" ]; then
  echo "bubblewrap: up to date ($out/bwrap)"
  exit 0
fi

fetch() { # file url sha
  mkdir -p "$cache"
  if [ ! -f "$cache/$1" ]; then
    [ "${V2_OFFLINE:-0}" = 1 ] && { echo "bubblewrap: $cache/$1 missing and V2_OFFLINE=1" >&2; exit 1; }
    echo "bubblewrap: downloading $2"
    curl -fsSL --max-time 300 -o "$cache/$1.part" "$2"
    mv "$cache/$1.part" "$cache/$1"
  fi
  echo "$3  $cache/$1" | sha256sum -c --quiet - || { echo "bubblewrap: SHA256 mismatch for $cache/$1 (expected $3)" >&2; exit 1; }
}
fetch bubblewrap-$BW_VER.tar.xz "$BW_URL" $BW_SHA
fetch meson-$MESON_VER.tar.gz "$MESON_URL" $MESON_SHA
fetch bash-$BASH_VER.tar.gz "$BASH_URL" $BASH_SHA

# Fresh trees every time: the tarballs' contents only ever run through meson/ninja and configure/make.
rm -rf "$work"
mkdir -p "$work" "$out"
for t in bubblewrap-$BW_VER.tar.xz meson-$MESON_VER.tar.gz bash-$BASH_VER.tar.gz; do
  tar -xf "$cache/$t" -C "$work" --no-same-owner --no-same-permissions
done
src=$work/bubblewrap-$BW_VER
CFLAGS="-O2 -march=x86-64-v3 -Wno-unused-command-line-argument"

# libcap: only cap_from_name is used (libcap-shim/); a pkg-config file makes meson's dependency('libcap') find it.
shim=$work/libcap-shim
mkdir -p "$shim/include/sys" "$shim/lib/pkgconfig"
cp "$here/libcap-shim/sys/capability.h" "$shim/include/sys/"
"$cc" $CFLAGS -I"$shim/include" -c -o "$shim/cap_names.o" "$here/libcap-shim/cap_names.c"
llvm-ar rcs "$shim/lib/libcap.a" "$shim/cap_names.o"
cat > "$shim/lib/pkgconfig/libcap.pc" <<EOF
Name: libcap
Description: cap_from_name shim for the V2 bubblewrap port
Version: 2.76
Cflags: -I$shim/include
Libs: -L$shim/lib -lcap
EOF
cat > "$work/native.ini" <<EOF
[binaries]
c = '$cc'
ar = 'llvm-ar'
strip = 'llvm-strip'
EOF
PKG_CONFIG_LIBDIR=$shim/lib/pkgconfig CFLAGS="$CFLAGS" python3 "$work/meson-$MESON_VER/meson.py" setup \
  --native-file "$work/native.ini" --buildtype=release -Ddefault_library=static -Dprefer_static=true \
  -Dselinux=disabled -Dman=disabled -Dbash_completion=disabled -Dzsh_completion=disabled -Dtests=true \
  "$work/bw-build" "$src" > "$work/meson.log" 2>&1 || { tail -30 "$work/meson.log" >&2; exit 1; }
ninja -C "$work/bw-build" -j"$jobs" > "$work/ninja.log" 2>&1 || { tail -40 "$work/ninja.log" >&2; exit 1; }

# bash: static, its own malloc off (musl's), no readline history file games needed by the suite.
bsrc=$work/bash-$BASH_VER
( cd "$bsrc" && CC="$cc" CFLAGS="$CFLAGS -std=gnu17" ./configure --without-bash-malloc --enable-static-link \
    --disable-nls --host=x86_64-linux-musl > "$work/bash-configure.log" 2>&1 ) || { tail -30 "$work/bash-configure.log" >&2; exit 1; }
make -C "$bsrc" -j"$jobs" > "$work/bash-make.log" 2>&1 || { tail -40 "$work/bash-make.log" >&2; exit 1; }

for t in getpcaps bwrap-lockf bwrap-pdeath-probe findmnt df; do
  "$cc" $CFLAGS -Wall -Werror -o "$out/$t.tmp" "$here/tools/$t.c"
  llvm-strip "$out/$t.tmp"; mv "$out/$t.tmp" "$out/$t"
done


rm -rf "$out/tests"
mkdir -p "$out/tests"
cp "$src"/tests/libtest.sh "$src"/tests/libtest-core.sh "$src"/tests/test-run.sh "$src"/tests/test-specifying-pidns.sh \
  "$src"/tests/test-specifying-userns.sh "$out/tests/"
for p in "$here"/patches/*.patch; do
  [ -f "$p" ] && patch -s -d "$out/tests" -p1 < "$p"
done
for t in test-utils try-syscall; do
  cp "$work/bw-build/tests/$t" "$out/tests/$t"; llvm-strip "$out/tests/$t"
done
cp "$bsrc/bash" "$out/bash.tmp"; llvm-strip "$out/bash.tmp"; mv "$out/bash.tmp" "$out/bash"
cp "$work/bw-build/bwrap" "$out/bwrap.tmp"; llvm-strip "$out/bwrap.tmp"
cp "$src/COPYING" "$out/COPYING.bubblewrap"; cp "$bsrc/COPYING" "$out/COPYING.bash"
mv "$out/bwrap.tmp" "$out/bwrap"
file "$out/bwrap" "$out/bash" | sed 's/, BuildID[^,]*//'
echo "bubblewrap: built $out/bwrap ($(wc -c < "$out/bwrap") bytes), bash $(wc -c < "$out/bash") bytes"
