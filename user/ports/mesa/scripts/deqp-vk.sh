#!/usr/bin/env bash
# GM-1d: builds Khronos's Vulkan conformance runner deqp-vk from source for the stick tree \BITSOS2\deqp\ (no Linux
# distribution packages it). The source is the pinned tag below with the pinned revisions of its fetch_sources.py;
# everything lands in user/ports/mesa/out/deqp/ (git ignores it). A first build takes about ten minutes on 20 threads
# and 4 GB of disk; a second run only checks and stages.
#   tree/deqp-vk     the program, stripped; it runs on the Mesa tree's glibc (PT_INTERP and RPATH point to
#                    /boot/BITSOS2/mesa/lib; always start it through `mesa/run`, on any machine)
#   tree/vulkan/     the data files the subset reads (`DATA` below; a case of a bigger subset that needs another fails
#                    with "Failed to open file"); --deqp-archive-dir=<tree> points here. The full 17 MB are 1600 files
#                    that would all go into the stick's package list for nothing.
#   tree/subset.txt  the small subset the Mini-PC run plays (host/tests/gm/ref-780m/deqp-subset.txt)
#   tree/LICENSES/   Apache-2.0 of VK-GL-CTS and the licences of the external projects linked in
#   tree/VERSION.txt the tag and its commit
# No windowing system is linked (the vulkan_headless target): this subset has no WSI test and the program must need
# nothing beyond the Mesa tree. zlib and libpng come from the CTS's pinned sources, static. Vulkan comes by dlopen of
# libvulkan.so.1, which `run` finds in the Mesa tree, so `MESA_VK_DRIVERS` chooses the driver.
# Run it after the Mesa stage; `DEQP_JOBS=<n>` limits the build's threads.
set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
OUT=${MESA_OUT:-$HERE/out}/deqp
TAG=vulkan-cts-1.4.7.1
COMMIT=0d18b347943e5e843529a05c95220e27fe30c538
MESA_LIB=/boot/BITSOS2/mesa/lib
# What the subset opens below vulkan/ (strace of a whole run).
DATA="draw/VertexFetch.frag draw/VertexFetch.vert draw/VertexFetchInstancedFirstInstance.vert"
die() { echo "deqp-vk: $*" >&2; exit 1; }
for t in git cmake ninja python3 readelf strip; do command -v "$t" >/dev/null || die "no $t on this host"; done

SRC=$OUT/src
if [ ! -d "$SRC/.git" ]; then
  git clone -q --depth 1 --branch "$TAG" https://github.com/KhronosGroup/VK-GL-CTS "$SRC" || die "clone of $TAG failed"
fi
[ "$(git -C "$SRC" rev-parse HEAD)" = "$COMMIT" ] || die "$SRC is not $TAG ($COMMIT)"
[ -e "$SRC/external/.fetched-$COMMIT" ] || {
  (cd "$SRC" && python3 external/fetch_sources.py --protocol https) || die "fetch_sources.py failed"
  touch "$SRC/external/.fetched-$COMMIT"
}

BUILD=$OUT/build
cmake -S "$SRC" -B "$BUILD" -G Ninja -Wno-dev \
  -DCMAKE_BUILD_TYPE=Release -DDEQP_TARGET=vulkan_headless \
  -DCMAKE_DISABLE_FIND_PACKAGE_PNG=ON -DCMAKE_DISABLE_FIND_PACKAGE_ZLIB=ON \
  -DCMAKE_C_FLAGS="-march=x86-64-v3 -w" -DCMAKE_CXX_FLAGS="-march=x86-64-v3 -w" \
  -DCMAKE_EXE_LINKER_FLAGS="-Wl,--dynamic-linker=$MESA_LIB/ld-linux-x86-64.so.2 -Wl,--disable-new-dtags -Wl,-rpath,$MESA_LIB" \
  >"$OUT/cmake.log" 2>&1 || { tail -30 "$OUT/cmake.log"; die "cmake failed ($OUT/cmake.log)"; }
ninja -C "$BUILD" -j "${DEQP_JOBS:-$(nproc)}" deqp-vk >"$OUT/build.log" 2>&1 || { tail -30 "$OUT/build.log"; die "the build failed ($OUT/build.log)"; }

BIN=$BUILD/external/vulkancts/modules/vulkan/deqp-vk
[ -x "$BIN" ] || die "no $BIN after the build"
NEW=$OUT/tree.new
rm -rf "$NEW"
mkdir -p "$NEW/LICENSES/vk-gl-cts"
cp "$BIN" "$NEW/deqp-vk"
strip "$NEW/deqp-vk"
for f in $DATA; do
  mkdir -p "$NEW/vulkan/$(dirname "$f")"
  cp "$(dirname "$BIN")/vulkan/$f" "$NEW/vulkan/$f"
done
cp "$REPO/host/tests/gm/ref-780m/deqp-subset.txt" "$NEW/subset.txt"
cp "$SRC/LICENSE" "$NEW/LICENSES/vk-gl-cts/LICENSE"
# What is linked in: each external project's licence texts.
for p in amber glslang jsoncpp libpng spirv-headers spirv-tools vulkan-video-samples zlib; do
  mkdir -p "$NEW/LICENSES/$p"
  n=0
  for f in "$SRC/external/$p/src"/LICENSE*; do
    [ -f "$f" ] || continue
    cp "$f" "$NEW/LICENSES/$p/"
    n=$((n + 1))
  done
  [ "$n" != 0 ] || die "no licence file in external/$p/src"
done
printf '%s %s\n' "$TAG" "$COMMIT" >"$NEW/VERSION.txt"

# It may only need what the Mesa tree has (a new library here would be missing on the stick).
ALLOWED="libc.so.6 libm.so.6 libstdc++.so.6 libgcc_s.so.1 ld-linux-x86-64.so.2 libdl.so.2 libpthread.so.0 librt.so.1"
for so in $(readelf -d "$NEW/deqp-vk" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
  case " $ALLOWED " in *" $so "*) ;; *) die "deqp-vk needs $so, which the Mesa tree does not promise" ;; esac
done
readelf -l "$NEW/deqp-vk" | grep -q "interpreter: $MESA_LIB/ld-linux-x86-64.so.2" || die "deqp-vk's PT_INTERP is not the Mesa tree's"
dup=$(cd "$NEW" && find . | tr '[:upper:]' '[:lower:]' | sort | uniq -d)
[ -z "$dup" ] || die "names that FAT folds together: $dup"

rm -rf "$OUT/tree"
mv "$NEW" "$OUT/tree"
echo "deqp-vk: $OUT/tree: $(du -sm "$OUT/tree" | cut -f1) MB ($TAG)"
