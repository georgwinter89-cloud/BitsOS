#!/bin/bash
# GM-3v: VA-API in the staged Mesa tree (user/ports/mesa/scripts/stage-v2.sh), on the host, no QEMU and no device.
# The files (vainfo, libva and its DRM/X11/Wayland parts, radeonsi_drv_video.so, their licences and packages); every
# new ELF resolves inside the tree; the H.264/H.265 signs in libgallium (Georg's stick has every codec); then, in a user
# namespace with an empty tmpfs over /dev/dri (the host's GPU drives the desk's display, so no node is ever opened),
# `run vainfo --display drm` and `--device /dev/null` answer plainly with exit 1, `run --check` names the VA driver, and
# vaprobe.c finds the driver through run's LIBVA_* the way libva does and calls its __vaDriverInit without a device.
# Everything starts from the static BusyBox sh (as on V2), so the loader's LD_DEBUG lines are the tree's alone: each
# library tried or started must lie in the tree's lib/.
# The host needs (red without them, never skipped): bash, unshare with user namespaces (-rm), gcc and libva's headers
# (/usr/include/va, pacman libva, which the stage needs too), and the BusyBox build of user/ports/busybox.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT=$PWD
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/gm-80-vainfo-stage}"
rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
red() { echo "RED: $*"; exit 1; }

STAGE=${MESA_STAGE:-$ROOT/user/ports/mesa/out/mesa}
if [ -z "${MESA_STAGE:-}" ] && [ ! -d "$STAGE" ]; then
    # A worktree has no out/: the manifest's `tree mesa`; an empty answer keeps the path above.
    _m=$(awk '$1 == "tree" && $2 == "mesa" { sub(/^host:/, "", $3); print $3 }' "$ROOT/user/packaging/manifest.txt")
    [ -z "$_m" ] || STAGE=$_m
fi
[ -x "$STAGE/run" ] && [ -f "$STAGE/lib/ld-linux-x86-64.so.2" ] \
  || red "no staged Mesa in $STAGE (bash user/ports/mesa/scripts/stage-v2.sh)"
cmp -s "$STAGE/run" user/ports/mesa/bitsos/run || red "$STAGE/run is older than user/ports/mesa/bitsos/run: stage again"
L=$STAGE/lib
LD=$L/ld-linux-x86-64.so.2
echo "stage: $STAGE"

BB=$ROOT/user/ports/busybox/out/busybox
if [ ! -x "$BB" ]; then
    # BusyBox is built against the program sysroot (musl); a fresh checkout has neither yet.
    [ -d "$ROOT/user/sysroot/out/sysroot" ] || bash "$ROOT/user/sysroot/build.sh" >/dev/null 2>&1 || true
    V2_OFFLINE=1 sh "$ROOT/user/ports/busybox/build.sh" >/dev/null 2>&1 || true
fi
[ -x "$BB" ] || red "no BusyBox build ($BB): user/sysroot/build.sh or user/ports/busybox/build.sh failed"
UNSHARE=$(command -v unshare) || red "no unshare on this host"
"$UNSHARE" -rm true 2>/dev/null || red "no user namespace on this host (unshare -rm)"
command -v gcc >/dev/null || red "no gcc on this host (it builds host/tests/gm/vaprobe.c)"
[ -f /usr/include/va/va_backend.h ] || red "no libva headers on this host (pacman -S libva: the stage needs it too)"
# V2's /bin: BusyBox applets only, so no host program runs between the namespace and the tree's loader.
mkdir -p "$RUN_DIR/bin" "$RUN_DIR/home"
for a in sh mount dirname sed grep ls tr env; do ln -s "$BB" "$RUN_DIR/bin/$a"; done

# 1. The files, their packages and licences.
for f in bin/vainfo lib/libva.so.2 lib/libva-drm.so.2 lib/libva-x11.so.2 lib/libva-wayland.so.2 \
         lib/dri/radeonsi_drv_video.so LICENSES/libva/COPYING LICENSES/libva-utils/COPYING; do
    [ -f "$STAGE/$f" ] || red "the stage has no $f"
done
for p in libva libva-utils; do
    grep -q "^$p-[0-9]" "$STAGE/PACKAGES.txt" || red "PACKAGES.txt does not name $p"
done
echo "ok   files, PACKAGES.txt and LICENSES of libva and libva-utils"

# 2. Each new ELF resolves inside the tree (the stage script's own rule).
for f in bin/vainfo lib/libva.so.2 lib/libva-drm.so.2 lib/libva-x11.so.2 lib/libva-wayland.so.2 lib/dri/radeonsi_drv_video.so; do
    out=$("$LD" --inhibit-cache --library-path "$L" --list "$STAGE/$f" 2>&1) || red "the loader fails on $f: $out"
    bad=$(echo "$out" | grep -v -e "=> $L/" -e 'linux-vdso' -e "^[[:space:]]*$L/ld-linux" -e '^[[:space:]]*/lib64/ld-linux' || true)
    [ -z "$bad" ] || red "$f resolves outside the tree: $bad"
done
echo "$("$LD" --inhibit-cache --library-path "$L" --list "$STAGE/lib/dri/radeonsi_drv_video.so")" | grep -q "libgallium-[^ ]* => $L/libgallium-" \
  || red "radeonsi_drv_video.so does not reach the tree's libgallium"
echo "ok   vainfo, libva* and radeonsi_drv_video.so resolve inside the tree (the driver through libgallium)"

# 3. The codecs of Georg's stick: the signs stage-v2.sh checks, read again from the staged file.
gal=$(ls "$L"/libgallium-*.so)
for sign in "Failed to create h264 decoder" "Failed to create h265 decoder" d3d12_video_bitstream_builder_h264 d3d12_video_bitstream_builder_hevc; do
    grep -qaF "$sign" "$gal" || red "${gal##*/} lacks '$sign': no H.264/H.265 in this stage"
done
echo "ok   ${gal##*/} carries the H.264/H.265 codecs"

# Runs "$@" in a user namespace with an empty /dev/dri, from BusyBox sh with V2's PATH; LD_DEBUG=libs only for the
# command itself. A host without /dev/dri has no node to hide (and no place to mount on). Output in
# $RUN_DIR/$1.txt, the exit status in $rc.
ns() {
    local tag=$1; shift
    set +e
    env -i PATH="$RUN_DIR/bin" HOME="$RUN_DIR/home" "$UNSHARE" -rm "$BB" sh -c \
        '[ ! -e /dev/dri ] || mount -t tmpfs none /dev/dri || { echo "ns: cannot hide /dev/dri" >&2; exit 125; }
         LD_DEBUG=libs exec "$@"' _ "$@" >"$RUN_DIR/$tag.txt" 2>&1
    rc=$?
    set -e
    ! grep -q "^ns: cannot hide /dev/dri" "$RUN_DIR/$tag.txt" \
      || { cat "$RUN_DIR/$tag.txt"; red "$tag: the namespace could not put an empty tmpfs over /dev/dri"; }
}
# No crash, and every library the loader tried or started lies in the tree.
clean() {
    local f=$RUN_DIR/$1.txt
    if grep -q -e Segmentation -e Aborted -e 'core dumped' "$f" || [ "$rc" -ge 128 ]; then cat "$f"; red "$1: a crash (exit $rc)"; fi
    local out
    out=$(sed -n 's/.*\(trying file=\|calling init: \)\(.*\)/\2/p' "$f" | grep -v "^$L/" || true)
    [ -z "$out" ] || red "$1: libraries from outside the tree: $(echo "$out" | sort -u | tr '\n' ' ')"
    grep -q "calling init: $L/libc.so.6" "$f" || { cat "$f"; red "$1: no sign of the tree's libc in LD_DEBUG"; }
}

# 4. vainfo without a node: the plain answer, exit 1.
ns vainfo-drm sh "$STAGE/run" vainfo --display drm
clean vainfo-drm
[ "$rc" = 1 ] || { cat "$RUN_DIR/vainfo-drm.txt"; red "vainfo --display drm: exit $rc (wanted 1)"; }
grep -q "failed to initialize display 'drm'" "$RUN_DIR/vainfo-drm.txt" || { cat "$RUN_DIR/vainfo-drm.txt"; red "vainfo --display drm did not say 'failed to initialize display'"; }
echo "ok   run vainfo --display drm without a node: \"$(grep -o "error: failed to initialize display 'drm'" "$RUN_DIR/vainfo-drm.txt" | head -n 1)\", exit 1"
ns vainfo-null sh "$STAGE/run" vainfo --display drm --device /dev/null
clean vainfo-null
[ "$rc" = 1 ] || { cat "$RUN_DIR/vainfo-null.txt"; red "vainfo --device /dev/null: exit $rc (wanted 1)"; }
grep -q "Failed to a DRM display for the given device" "$RUN_DIR/vainfo-null.txt" || { cat "$RUN_DIR/vainfo-null.txt"; red "vainfo --device /dev/null: no plain answer"; }
echo "ok   run vainfo --device /dev/null (no DRM node): libva-utils' own message, exit 1"

# 5. run --check names the VA driver; the existing lines stay (ul.pkg.mesa reads them).
ns check sh "$STAGE/run" --check
[ "$rc" = 0 ] || { cat "$RUN_DIR/check.txt"; red "run --check: exit $rc"; }
for want in "mesa: libraries: " "mesa: driver radeon_icd.json: libvulkan_radeon.so" "mesa: driver radeonsi_drv_video.so: ${gal##*/}"; do
    grep -q "^$want" "$RUN_DIR/check.txt" || { cat "$RUN_DIR/check.txt"; red "run --check has no line '$want'"; }
done
echo "ok   run --check: $(grep '^mesa: driver radeonsi_drv_video.so' "$RUN_DIR/check.txt")"
ns badname env LIBVA_DRIVER_NAME=../x sh "$STAGE/run" --check
[ "$rc" = 0 ] && grep -q "^mesa: driver radeonsi_drv_video.so: ${gal##*/}" "$RUN_DIR/badname.txt" \
  || { cat "$RUN_DIR/badname.txt"; red "run takes a caller's LIBVA_DRIVER_NAME=../x (exit $rc)"; }
echo "ok   run takes no caller's LIBVA_DRIVER_NAME: radeonsi, the tree's one VA driver"

# 6. The driver through run's search path, called without a device. The probe is the host's gcc output; the tree's
# glibc is the same pacman glibc.
gcc -O2 -Wall -Wextra -Werror -o "$RUN_DIR/vaprobe" host/tests/gm/vaprobe.c
ns probe sh "$STAGE/run" "$RUN_DIR/vaprobe"
clean probe
[ "$rc" = 0 ] || { cat "$RUN_DIR/probe.txt"; red "vaprobe: exit $rc"; }
grep -q "^driver: $L/dri/radeonsi_drv_video.so$" "$RUN_DIR/probe.txt" || { cat "$RUN_DIR/probe.txt"; red "vaprobe did not find the tree's radeonsi_drv_video.so"; }
grep -q "calling init: $gal" "$RUN_DIR/probe.txt" || { cat "$RUN_DIR/probe.txt"; red "vaprobe: the tree's libgallium was not started"; }
echo "ok   vaprobe: $(grep -e '^init:' -e '^no fd:' -e '^/dev/null:' "$RUN_DIR/probe.txt" | tr '\n' ' ')"
echo "PASS gm/80-vainfo-stage"
