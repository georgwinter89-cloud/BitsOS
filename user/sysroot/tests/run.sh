#!/bin/sh
# Toolchain check for UL-1: builds the sysroot (twice: the second run must do no work), builds
# tests/hello.c, tests/hello.cpp and user/smoke/hello-rs, checks every binary is a self-contained
# static-pie, and runs it on the host (CachyOS). Exit code 0 = all good.
# V2_SKIP_BUILD=1 skips the build.sh runs (sysroot must already exist).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
sysroot_dir=$(cd "$here/.." && pwd)
user_dir=$(cd "$sysroot_dir/.." && pwd)
bin=$sysroot_dir/bin
out=$here/out
mkdir -p "$out"
fail=0

if [ "${V2_SKIP_BUILD:-0}" != 1 ]; then
  "$sysroot_dir/build.sh" >"$out/build1.log"
  lib=$sysroot_dir/out/sysroot/usr/lib
  before=$(stat -c '%n %Y' "$lib"/*.a "$lib"/*.o)
  "$sysroot_dir/build.sh" >"$out/build2.log"
  after=$(stat -c '%n %Y' "$lib"/*.a "$lib"/*.o)
  if grep -q "no work to do" "$sysroot_dir/out/build/llvm/ninja.log" && [ "$before" = "$after" ]; then
    echo "ok   build.sh rerun does no work"
  else echo "FAIL build.sh rerun rebuilt something"; fail=1; fi
fi

"$bin/v2-cc" -O2 -Wall -Werror -o "$out/hello-c" "$here/hello.c"
"$bin/v2-c++" -O2 -std=c++20 -Wall -Werror -o "$out/hello-cpp" "$here/hello.cpp"
# Separate compile + link must work too (how build systems call it).
"$bin/v2-c++" -O2 -std=c++20 -c -o "$out/hello-cpp.o" "$here/hello.cpp"
"$bin/v2-c++" -o "$out/hello-cpp2" "$out/hello-cpp.o"
# The CMake toolchain file, as ports will use it.
cmake -S "$here/cmake" -B "$out/cmake" -G Ninja -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_TOOLCHAIN_FILE="$sysroot_dir/v2-toolchain.cmake" >"$out/cmake.log" 2>&1
ninja -C "$out/cmake" -j8 >>"$out/cmake.log" 2>&1
cp "$out/cmake/hello-c" "$out/cmake-hello-c"
cp "$out/cmake/hello-cpp" "$out/cmake-hello-cpp"
# Rust side: stable toolchain, musl target, same output kind.
target_dir=${CARGO_TARGET_DIR:-$(cd "$user_dir/.." && pwd)/target}
CARGO_TARGET_DIR=$target_dir cargo build --offline --release --target x86_64-unknown-linux-musl \
  --manifest-path "$user_dir/Cargo.toml" -q
cp "$target_dir/x86_64-unknown-linux-musl/release/hello-rs" "$out/hello-rs"

# Wrapper modes build systems use: dependency-only runs, partial links, and -shared (refused).
if "$bin/v2-cc" -MM -Werror "$here/hello.c" >"$out/deps.txt" 2>"$out/deps.err" && [ ! -s "$out/deps.err" ]
then echo "ok   v2-cc -MM is compile-only"; else echo "FAIL v2-cc -MM: $(cat "$out/deps.err")"; fail=1; fi
"$bin/v2-cc" -c -o "$out/r1.o" "$here/hello.c"
if "$bin/v2-cc" -r -o "$out/r.o" "$out/r1.o" && file -b "$out/r.o" | grep -q relocatable
then echo "ok   v2-cc -r gives a relocatable object"; else echo "FAIL v2-cc -r"; fail=1; fi
if "$bin/v2-cc" -shared -o "$out/x.so" "$out/r1.o" 2>/dev/null
then echo "FAIL v2-cc -shared was accepted"; fail=1; else echo "ok   v2-cc -shared is refused"; fi

check() { # name expected-output
  f=$out/$1
  kind=$(file -b "$f")
  case $kind in *static-pie*) ;; *) echo "FAIL $1: not static-pie: $kind"; fail=1 ;; esac
  if readelf -lW "$f" | grep -q INTERP; then echo "FAIL $1: has an interpreter"; fail=1; fi
  if readelf -dW "$f" | grep -q NEEDED; then echo "FAIL $1: needs shared libraries"; fail=1; fi
  if strings -a "$f" | grep -qE 'GLIBC_|GLIBCXX_|libstdc\+\+|/usr/lib/gcc'; then
    echo "FAIL $1: contains host glibc/libstdc++ traces"; fail=1; fi
  got=$("$f" a b)
  if [ "$got" = "$2" ]; then echo "ok   $1 ($(stat -c %s "$f") bytes)"; else
    echo "FAIL $1: got"; echo "$got"; fail=1; fi
}
c_expect="hello from C on musl, argc=3, len=28, pid>0=1"
cpp_expect="caught St12out_of_range (main)
hello from C++ on musl: threads=6 sum=46 caught=1:three from_chars=2500 ok
cv=2 deep=deep dtors=6 bad_input=rejected"
check hello-c "$c_expect"
check hello-cpp "$cpp_expect"
check hello-cpp2 "$cpp_expect"
check cmake-hello-c "$c_expect"
check cmake-hello-cpp "$cpp_expect"
check hello-rs "hello from Rust std on BitsOS V2, sum=10"
exit $fail
