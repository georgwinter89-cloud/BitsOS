#!/bin/bash
# DR-4: libdrm 2.4.134 and libdrm_amdgpu as static archives for V2 programs (musl, static-pie, built with
# user/sysroot/bin/v2-cc), then host/tests/dr/libdrm/info.c (DR-6: and bo.c) linked against them.
# Usage: libdrm-static.sh <out dir>   ->  <out>/lib/libdrm.a, <out>/lib/libdrm_amdgpu.a, <out>/include, <out>/libdrm-info
# Source: https://dri.freedesktop.org/libdrm/libdrm-2.4.134.tar.xz (MIT), kept in $V2_DOWNLOADS
# (default ~/.cache/bitsos-v2/downloads), checked by SHA256 on every build; V2_OFFLINE=1 refuses to download.
# libdrm's meson build is not used (no meson here): the same files with a config.h of musl's answers.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT=$PWD
OUT=$(mkdir -p "$1" && realpath "$1")
VER=2.4.134
TAR=libdrm-$VER.tar.xz
URL=https://dri.freedesktop.org/libdrm/$TAR
SHA=ac5e74d157830eb8bee44c6a6bf3ad49774ef0dd2a72bdad74a8f20308b52a95
DL=${V2_DOWNLOADS:-$HOME/.cache/bitsos-v2/downloads}
CC="$ROOT/user/sysroot/bin/v2-cc"
[ -x "$CC" ] || { echo "RED: no $CC"; exit 1; }

mkdir -p "$DL"
if [ ! -f "$DL/$TAR" ]; then
    [ "${V2_OFFLINE:-0}" = 1 ] && { echo "SKIP: $TAR not in $DL and V2_OFFLINE=1"; exit 2; }
    curl -sSfL -o "$DL/$TAR.part" "$URL" && mv "$DL/$TAR.part" "$DL/$TAR"
fi
echo "$SHA  $DL/$TAR" | sha256sum -c --quiet - || { echo "RED: $TAR does not have the pinned SHA256"; exit 1; }

# A fresh, empty directory for the unpacked source; nothing in it is run except through the compiler.
rm -rf "$OUT/src" "$OUT/obj" "$OUT/lib" "$OUT/include"
mkdir -p "$OUT/src" "$OUT/obj" "$OUT/lib" "$OUT/include/libdrm"
tar -xJf "$DL/$TAR" -C "$OUT/src"
S=$OUT/src/libdrm-$VER

cat > "$OUT/obj/config.h" <<'EOF'
#define _GNU_SOURCE 1
#define UDEV 0
#define HAVE_SECURE_GETENV 1
#define HAVE_LIBDRM_ATOMIC_PRIMITIVES 1
#define HAVE_LIB_ATOMIC_OPS 0
#define HAVE_SYS_SYSCTL_H 0
#define HAVE_SYS_SELECT_H 1
#define HAVE_ALLOCA_H 1
#define MAJOR_IN_SYSMACROS 1
#define HAVE_OPEN_MEMSTREAM 1
#define HAVE_VISIBILITY 1
#define HAVE_VALGRIND 0
EOF
# drm_fourcc's modifier name table, made by libdrm's own generator (run isolated, paths as arguments).
python3 -I "$S/gen_table_fourcc.py" "$S/include/drm/drm_fourcc.h" "$OUT/obj/generated_static_table_fourcc.h"

FLAGS=(-O2 -w -include "$OUT/obj/config.h" -I "$OUT/obj" -I "$S" -I "$S/include/drm" -march=x86-64-v3
       '-DAMDGPU_ASIC_ID_TABLE="/usr/share/libdrm/amdgpu.ids"')
for f in xf86drm.c xf86drmHash.c xf86drmRandom.c xf86drmSL.c xf86drmMode.c; do
    "$CC" "${FLAGS[@]}" -c "$S/$f" -o "$OUT/obj/${f%.c}.o"
done
for f in amdgpu_asic_id amdgpu_bo amdgpu_cs amdgpu_device amdgpu_gpu_info amdgpu_vamgr amdgpu_vm handle_table amdgpu_userq; do
    "$CC" "${FLAGS[@]}" -I "$S/amdgpu" -c "$S/amdgpu/$f.c" -o "$OUT/obj/amdgpu-$f.o"
done
ar rcs "$OUT/lib/libdrm.a" "$OUT"/obj/xf86drm*.o
ar rcs "$OUT/lib/libdrm_amdgpu.a" "$OUT"/obj/amdgpu-*.o
cp "$S"/xf86drm.h "$S"/xf86drmMode.h "$S"/amdgpu/amdgpu.h "$S"/include/drm/*.h "$OUT/include/libdrm/"

"$CC" -O2 -Wall -Wextra -Werror -march=x86-64-v3 -I "$OUT/include/libdrm" -o "$OUT/libdrm-info" \
    host/tests/dr/libdrm/info.c -L "$OUT/lib" -ldrm_amdgpu -ldrm -lpthread
# DR-6: the buffer calls (bo.c).
"$CC" -O2 -Wall -Wextra -Werror -march=x86-64-v3 -I "$OUT/include/libdrm" -o "$OUT/libdrm-bo" \
    host/tests/dr/libdrm/bo.c -L "$OUT/lib" -ldrm_amdgpu -ldrm -lpthread
# DR-8: the command-submission calls (cs.c), none of which reaches a GPU.
"$CC" -O2 -Wall -Wextra -Werror -march=x86-64-v3 -I "$OUT/include/libdrm" -o "$OUT/libdrm-cs" \
    host/tests/dr/libdrm/cs.c -L "$OUT/lib" -ldrm_amdgpu -ldrm -lpthread
echo "built $OUT/libdrm-info, $OUT/libdrm-bo and $OUT/libdrm-cs (libdrm $VER static, musl static-pie)"
