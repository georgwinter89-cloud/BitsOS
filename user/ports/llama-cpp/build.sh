#!/bin/sh
# CX-5: llama.cpp's llama-server (release b11303, CPU only, x86-64-v3) for BitsOS V2 as out/ctxd-llama: the
# embedding back end of the context service ctxd (user/services/ctxd), started by ctxd as a child process on a
# Unix socket (no TCP port, --offline). Static-pie against the UL-1 musl sysroot with libc++ (v2-toolchain.cmake),
# so the same file runs on CachyOS and on V2. No curl, no OpenSSL, no OpenMP (ggml's own threads), no GPU.
# Two upstream calls in src/llama-sampler.cpp drop a value libc++ marks nodiscard; they get a (void) below (a
# -Wno-unused-result in CMAKE_CXX_FLAGS was undone by llama.cpp's own later -Wall: two warnings in every build).
# The tarball comes from the download cache (Georg's OK for llama.cpp, user/services/ctxd/DESIGN.md 14.1), SHA256
# checked on every build. Idempotent: rebuilds only when out/ctxd-llama is older than this script.
# V2_DOWNLOADS overrides the download cache, V2_OFFLINE=1 refuses to download, V2_JOBS caps the jobs (8).
set -eu
VER=b11303
SHA=aff0ea04d2ba1bc7ef4c3ee44f020bd85973cbcfa0f585b5caef266de18b1b12
URL=https://github.com/ggml-org/llama.cpp/archive/refs/tags/$VER.tar.gz
FILE=llama.cpp-$VER.tar.gz

here=$(cd "$(dirname "$0")" && pwd)
cache=${V2_DOWNLOADS:-$HOME/.cache/bitsos-v2/downloads}
out=$here/out
work=$here/build
jobs=${V2_JOBS:-8}
sysdir=$(cd "$here/../../sysroot" && pwd)
sysroot=${V2_SYSROOT:-$sysdir/out/sysroot}

if [ -x "$out/ctxd-llama" ] && [ "$out/ctxd-llama" -nt "$0" ]; then
  echo "llama-cpp: up to date ($out/ctxd-llama)"
  exit 0
fi
[ -f "$sysroot/usr/lib/libc++.a" ] || { echo "llama-cpp: no musl sysroot with libc++ at $sysroot (user/sysroot/build.sh, or V2_SYSROOT)" >&2; exit 1; }
mkdir -p "$cache"
if [ ! -f "$cache/$FILE" ]; then
  [ "${V2_OFFLINE:-0}" = 1 ] && { echo "llama-cpp: $cache/$FILE missing and V2_OFFLINE=1" >&2; exit 1; }
  echo "llama-cpp: downloading $URL"
  curl -fsSL --max-time 600 -o "$cache/$FILE.part" "$URL"
  mv "$cache/$FILE.part" "$cache/$FILE"
fi
echo "$SHA  $cache/$FILE" | sha256sum -c --quiet - || { echo "llama-cpp: SHA256 mismatch for $cache/$FILE (expected $SHA)" >&2; exit 1; }

rm -rf "$work"
mkdir -p "$work" "$out"
tar -xzf "$cache/$FILE" -C "$work" --no-same-owner --no-same-permissions
src=$work/llama.cpp-$VER
sed -i -E 's/^([[:space:]]*)dist\((s?ctx)->rng\);$/\1(void) dist(\2->rng);/' "$src/src/llama-sampler.cpp"
[ "$(grep -c '(void) dist(' "$src/src/llama-sampler.cpp")" = 2 ] || { echo "llama-cpp: the two discarded draws in llama-sampler.cpp moved" >&2; exit 1; }
# The toolchain file finds its sysroot beside itself: a copy whose out/sysroot is the one in use (V2_SYSROOT).
mkdir -p "$work/tc/out"
cp "$sysdir/v2-toolchain.cmake" "$work/tc/"
ln -s "$sysdir/bin" "$work/tc/bin"
ln -s "$sysroot" "$work/tc/out/sysroot"
toolchain=$work/tc/v2-toolchain.cmake
# The release tarball has no .git: give cmake the version by hand so it does not call git.
cmake -S "$src" -B "$work/b" -G Ninja -DCMAKE_TOOLCHAIN_FILE="$toolchain" -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_FLAGS="-march=x86-64-v3" -DCMAKE_CXX_FLAGS="-march=x86-64-v3" \
  -DBUILD_SHARED_LIBS=OFF -DGGML_NATIVE=OFF -DGGML_AVX=ON -DGGML_AVX2=ON -DGGML_FMA=ON -DGGML_F16C=ON -DGGML_BMI2=ON \
  -DGGML_OPENMP=OFF -DGGML_BACKEND_DL=OFF -DGGML_CPU_ALL_VARIANTS=OFF -DLLAMA_CURL=OFF -DLLAMA_OPENSSL=OFF \
  -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_EXAMPLES=OFF -DLLAMA_BUILD_SERVER=ON -DLLAMA_BUILD_NUMBER=11303 \
  -DLLAMA_BUILD_COMMIT=$VER > "$work/cmake.log" 2>&1 || { tail -30 "$work/cmake.log" >&2; echo "llama-cpp: cmake failed" >&2; exit 1; }
ninja -C "$work/b" -j "$jobs" llama-server > "$work/ninja.log" 2>&1 || { grep -m5 -A5 'error' "$work/ninja.log" >&2; echo "llama-cpp: build failed" >&2; exit 1; }
bin=$work/b/bin/llama-server
file "$bin" | grep -q 'static-pie linked' || { file "$bin" >&2; echo "llama-cpp: not static-pie" >&2; exit 1; }
llvm-strip -o "$out/ctxd-llama.tmp" "$bin"
cp "$src/LICENSE" "$out/LICENSE.llama.cpp"
mv "$out/ctxd-llama.tmp" "$out/ctxd-llama"
echo "llama-cpp: $out/ctxd-llama ($(stat -c %s "$out/ctxd-llama") bytes)"
