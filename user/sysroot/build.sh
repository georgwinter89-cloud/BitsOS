#!/usr/bin/env bash
# Builds the V2 program sysroot into user/sysroot/out/sysroot (prefix /usr inside):
#   musl (libc.a, crt1.o/rcrt1.o/crti.o/crtn.o, headers), then libunwind.a, libc++abi.a and libc++.a
#   (libc++.a also carries libc++abi's objects, so `-lc++` is enough), all static and PIC for static-pie.
# Idempotent: sources are mirrored by mirror.py (LF endings, mtimes kept), musl's make and ninja rebuild only what changed.
#   user/sysroot/build.sh [--clean]
# Env: V2_MUSL_SRC, V2_LLVM_SRC (read-only source trees), V2_KHDR (Linux UAPI headers, default /usr/include), V2_JOBS (default and maximum 8).
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
OUT=$HERE/out
SYSROOT=$OUT/sysroot
SRC=$OUT/src
BLD=$OUT/build
MUSL_SRC=${V2_MUSL_SRC:-/home/georg/BitsOS/werkzeuge/plattform/clib/fremd/musl}
LLVM_SRC=${V2_LLVM_SRC:-/home/georg/BitsOS/werkzeuge/plattform/clib/fremd/llvm-project}
JOBS=${V2_JOBS:-8}
# The Gaming-PC stays usable while this runs: 1..8 jobs (ninja -j0 would mean unlimited).
[[ $JOBS =~ ^[0-9]+$ ]] && (( JOBS >= 1 )) || JOBS=8
(( JOBS > 8 )) && JOBS=8
TRIPLE=x86_64-unknown-linux-musl

if [[ ${1:-} == --clean ]]; then rm -rf "$OUT"; fi

for tool in clang clang++ llvm-ar llvm-ranlib ld.lld ninja rsync make python3; do
  command -v "$tool" >/dev/null || { echo "build.sh: $tool missing (pacman -S clang lld llvm ninja rsync make)" >&2; exit 1; }
done
[[ -f $MUSL_SRC/VERSION ]] || { echo "build.sh: no musl source at $MUSL_SRC" >&2; exit 1; }
[[ -d $LLVM_SRC/libcxx/src ]] || { echo "build.sh: no llvm-project source at $LLVM_SRC" >&2; exit 1; }

t0=$SECONDS
mkdir -p "$SRC" "$BLD" "$SYSROOT/usr/lib" "$SYSROOT/usr/include/c++/v1"

# --- mirror the sources (the originals are never written) ---
python3 -I "$HERE/mirror.py" "$MUSL_SRC" "$SRC/musl" .git
for d in libcxx/include libcxx/src libcxx/vendor libcxxabi/include libcxxabi/src libunwind/include libunwind/src; do
  python3 -I "$HERE/mirror.py" "$LLVM_SRC/$d" "$SRC/$d"
done

# --- musl ---
MUSL_BLD=$BLD/musl
MUSL_CONF=(--prefix=/usr --disable-shared --target=x86_64-linux-musl
  CC="clang --target=$TRIPLE" CFLAGS="-fPIC" AR=llvm-ar RANLIB=llvm-ranlib)
mkdir -p "$MUSL_BLD"
if [[ ! -f $MUSL_BLD/config.mak || "$(cat "$MUSL_BLD/.conf" 2>/dev/null)" != "${MUSL_CONF[*]}" ]]; then
  (cd "$MUSL_BLD" && "$SRC/musl/configure" "${MUSL_CONF[@]}" >configure.log)
  printf '%s' "${MUSL_CONF[*]}" >"$MUSL_BLD/.conf"
fi
make -s -C "$MUSL_BLD" -j"$JOBS"
make -s -C "$MUSL_BLD" install DESTDIR="$SYSROOT" >/dev/null
t_musl=$((SECONDS - t0))

# --- Linux UAPI headers (linux/futex.h for libc++, later busybox and V2's own C programs) ---
# From the host's linux-api-headers package; musl keeps its own scsi/ headers (as on Alpine).
KHDR=${V2_KHDR:-/usr/include}
for d in asm asm-generic cxl drm fwctl linux misc mtd rdma regulator scsi sound video xen; do
  [[ -d $KHDR/$d ]] && rsync -a --exclude scsi.h --exclude scsi_ioctl.h --exclude sg.h "$KHDR/$d" "$SYSROOT/usr/include/"
done
[[ -f $SYSROOT/usr/include/linux/futex.h ]] || { echo "build.sh: no Linux headers at $KHDR (pacman -S linux-api-headers)" >&2; exit 1; }
# These headers follow the host package, so record which ones went in (ABI tests compare against them).
khdr_ver=
[[ $KHDR == /usr/include ]] && khdr_ver=$(pacman -Q linux-api-headers 2>/dev/null | cut -d' ' -f2 || true)
if [[ -z $khdr_ver && -f $KHDR/linux/version.h ]]; then
  khdr_ver=code-$(awk '/define LINUX_VERSION_CODE/ {print $3}' "$KHDR/linux/version.h")
fi

# --- C++ headers into the sysroot (what a CMake runtimes install would place there) ---
CXXINC=$SYSROOT/usr/include/c++/v1
rsync -a --exclude CMakeLists.txt --exclude '*.in' "$SRC/libcxx/include/" "$CXXINC/"
install -C -m644 "$HERE/config/__config_site" "$CXXINC/__config_site"
install -C -m644 "$SRC/libcxx/vendor/llvm/default_assertion_handler.in" "$CXXINC/__assertion_handler"
install -C -m644 "$SRC/libcxxabi/include/cxxabi.h" "$SRC/libcxxabi/include/__cxxabi_config.h" "$CXXINC/"

# --- libunwind, libc++abi, libc++ with ninja ---
# The local llvm-project is a sparse, blob-less checkout without runtimes/ and cmake/, so the CMake runtimes
# build cannot be configured offline; this ninja file mirrors its source lists and flags (LLVM 23, Linux, musl).
LB=$BLD/llvm
mkdir -p "$LB"
BASE="--target=$TRIPLE --sysroot=$SYSROOT -O2 -fPIC -ffunction-sections -fdata-sections -funwind-tables -DNDEBUG -w"
CXXBASE="$BASE -nostdinc++ -isystem $CXXINC"
UNW="-D_LIBUNWIND_IS_NATIVE_ONLY -fvisibility=hidden -I$SRC/libunwind/include"
LIBCPP_DEFS="-D_LIBCPP_BUILDING_LIBRARY -D_LIBCPP_AVAILABILITY_MINIMUM_HEADER_VERSION=2"
F_UNW_C="$BASE -std=c99 -fexceptions $UNW"
F_UNW_CXX="$CXXBASE -std=c++17 -fno-exceptions -fno-rtti $UNW"
F_UNW_S="$BASE $UNW"
F_ABI="$CXXBASE -std=c++26 -fexceptions -frtti -fstrict-aliasing -fsized-deallocation -D_LIBCXXABI_BUILDING_LIBRARY
  $LIBCPP_DEFS -I$SRC/libcxxabi/include -I$SRC/libunwind/include -I$SRC/libcxx/src"
F_CXX="$CXXBASE -std=c++26 -fexceptions -frtti -faligned-allocation -fsized-deallocation -fvisibility-inlines-hidden
  -fvisibility=hidden $LIBCPP_DEFS -D_LIBCPP_REMOVE_TRANSITIVE_INCLUDES -DLIBCXX_BUILDING_LIBCXXABI
  -I$SRC/libcxx/src -I$SRC/libcxxabi/include"

UNW_C="UnwindLevel1.c UnwindLevel1-gcc-ext.c Unwind-sjlj.c Unwind-wasm.c"
UNW_CXX="libunwind.cpp Unwind-EHABI.cpp Unwind-seh.cpp"
UNW_S="UnwindRegistersRestore.S UnwindRegistersSave.S"
ABI_SRC="cxa_aux_runtime cxa_default_handlers cxa_demangle cxa_exception_storage cxa_guard cxa_handlers cxa_vector
  cxa_virtual stdlib_exception stdlib_stdexcept stdlib_typeinfo abort_message fallback_malloc private_typeinfo
  stdlib_new_delete cxa_exception cxa_personality cxa_thread_atexit"
CXX_SRC="algorithm any bind call_once chrono error_category exception expected filesystem/filesystem_clock
  filesystem/filesystem_error filesystem/path functional hash memory memory_resource new_handler new_helpers optional
  print random_shuffle ryu/d2fixed ryu/d2s ryu/f2s stdexcept string system_error typeinfo valarray variant vector
  verbose_abort atomic barrier condition_variable_destructor condition_variable future mutex_destructor mutex
  shared_mutex thread random fstream ios ios.instantiations iostream locale ostream regex strstream text_encoding
  filesystem/directory_entry filesystem/directory_iterator filesystem/operations filesystem/int128_builtins"

ninja_file() {
  cat <<EOF
# generated by user/sysroot/build.sh
rule cc
  command = clang \$flags -MD -MF \$out.d -c \$in -o \$out
  depfile = \$out.d
  deps = gcc
  description = CC \$out
rule cxx
  command = clang++ \$flags -MD -MF \$out.d -c \$in -o \$out
  depfile = \$out.d
  deps = gcc
  description = CXX \$out
rule ar
  command = rm -f \$out && llvm-ar crsD \$out \$in
  description = AR \$out
EOF
  local f unw=() abi=() cxx=()
  for f in $UNW_C; do echo "build unwind/${f%.*}.o: cc $SRC/libunwind/src/$f"; echo "  flags = ${F_UNW_C//$'\n'/ }"; unw+=("unwind/${f%.*}.o"); done
  for f in $UNW_S; do echo "build unwind/${f%.*}.o: cc $SRC/libunwind/src/$f"; echo "  flags = $F_UNW_S"; unw+=("unwind/${f%.*}.o"); done
  for f in $UNW_CXX; do echo "build unwind/${f%.*}.o: cxx $SRC/libunwind/src/$f"; echo "  flags = $F_UNW_CXX"; unw+=("unwind/${f%.*}.o"); done
  for f in $ABI_SRC; do echo "build abi/$f.o: cxx $SRC/libcxxabi/src/$f.cpp"; echo "  flags = ${F_ABI//$'\n'/ }"; abi+=("abi/$f.o"); done
  for f in $CXX_SRC; do echo "build cxx/$f.o: cxx $SRC/libcxx/src/$f.cpp"; echo "  flags = ${F_CXX//$'\n'/ }"; cxx+=("cxx/$f.o"); done
  # Upstream's from_chars needs LLVM libc's shared/ headers (not in the checkout): patched copy, see README.
  echo "build cxx/charconv.o: cxx $HERE/patches/libcxx/charconv.cpp"
  echo "  flags = ${F_CXX//$'\n'/ } -I$SRC/libcxx/src"
  cxx+=("cxx/charconv.o")
  echo "build lib/libunwind.a: ar ${unw[*]}"
  echo "build lib/libc++abi.a: ar ${abi[*]}"
  echo "build lib/libc++.a: ar ${cxx[*]} ${abi[*]}"
}
ninja_file >"$LB/build.ninja.new"
if cmp -s "$LB/build.ninja.new" "$LB/build.ninja"; then rm "$LB/build.ninja.new"; else mv "$LB/build.ninja.new" "$LB/build.ninja"; fi
ninja -C "$LB" -j"$JOBS" >"$LB/ninja.log" || { tail -40 "$LB/ninja.log" >&2; exit 1; }
install -C -m644 "$LB/lib/libunwind.a" "$LB/lib/libc++abi.a" "$LB/lib/libc++.a" "$SYSROOT/usr/lib/"

llvm_ver=$(sed -n 's/^# *define _LIBCPP_VERSION \([0-9]*\).*/\1/p' "$CXXINC/__config")
{
  echo "musl $(cat "$SRC/musl/VERSION")"
  echo "libcxx $llvm_ver"
  echo "linux-api-headers ${khdr_ver:-unknown} ($KHDR)"
  echo "host-clang $(clang -dumpversion)"
} >"$SYSROOT/VERSIONS.new"
if cmp -s "$SYSROOT/VERSIONS.new" "$SYSROOT/VERSIONS"; then rm "$SYSROOT/VERSIONS.new"; else mv "$SYSROOT/VERSIONS.new" "$SYSROOT/VERSIONS"; fi
echo "sysroot ready: $SYSROOT (musl $(cat "$SRC/musl/VERSION"), musl step ${t_musl}s, total $((SECONDS - t0))s, -j$JOBS)"
for l in libc.a libunwind.a libc++abi.a libc++.a; do
  printf '  %-12s %8d KB\n' "$l" $(( $(stat -c %s "$SYSROOT/usr/lib/$l") / 1024 ))
done
sed 's/^/  /' "$SYSROOT/VERSIONS"
