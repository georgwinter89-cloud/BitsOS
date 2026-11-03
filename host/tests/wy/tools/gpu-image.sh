#!/bin/bash
# WY-14: the QEMU scenarios once with the gpu build (WY_GPU=1; nothing happens without it).
#   gpu-image.sh config        prints the CONFIG.TXT addition (";bitscomp=gpu") for `xtask image --config-extra`
#   gpu-image.sh image NAME    puts the gpu build at \BITSOS2\bin\bitscomp-gpu and the Mesa tree it runs with at
#                              \BITSOS2\mesa\ onto xtask's image NAME (its ESP tree, then the FAT image again with
#                              room for them, as host/tests/gm/10-mesa.sh does)
# The scenarios still start /bin/bitscomp (the rescue build); CONFIG.TXT bitscomp=gpu makes it hand over to the gpu
# build (`flavour`). Since INT-W24 the stick's manifest carries the gpu build and the stage pixman/libxkbcommon.
# WY_GPU=1: the gpu build and a tree it loads from (the stage plus the host's copies of what it lacks).
# WY-15: WY_GPU=gles: the same image with CONFIG.TXT bitscomp=gles (the gpu build draws with GLES on llvmpipe).
# WY-3: WY_XWAYLAND=1 also puts the Xwayland tree (user/ports/xwayland/scripts/build-v2.sh, with its test clients)
# at \BITSOS2\xwayland\.
# Fallback cases (the rescue build must run on): WY_GPU=bare: a stage without libpixman-1 (the stage before INT-W24
# lacked it; since the stage carries it, it is removed here), so the gpu build cannot load; WY_GPU=notree: the gpu
# build without any Mesa tree (its loader is missing).
set -euo pipefail
case ${WY_GPU:-0} in
  1|gles|bare|notree) ;;
  0) exit 0 ;;
  *) echo "WY_GPU=1|gles|bare|notree, not ${WY_GPU}" >&2; exit 2 ;;
esac
case $1 in
  config) if [ "$WY_GPU" = gles ]; then echo ";bitscomp=gles"; else echo ";bitscomp=gpu"; fi; exit 0 ;;
  image) NAME=$2 ;;
  *) echo "gpu-image.sh config | image NAME" >&2; exit 2 ;;
esac
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT=$PWD
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
STAGE=${MESA_STAGE:-$ROOT/user/ports/mesa/out/mesa}
if [ ! -f "$STAGE/lib/ld-linux-x86-64.so.2" ]; then
    STAGE=$CARGO_TARGET_DIR/wy-mesa-out/mesa
    [ -f "$STAGE/lib/ld-linux-x86-64.so.2" ] || MESA_OUT=$CARGO_TARGET_DIR/wy-mesa-out bash user/ports/mesa/scripts/stage-v2.sh
fi
export MESA_STAGE=$STAGE
W=$CARGO_TARGET_DIR/wy-gpu
if [ "$WY_GPU" = bare ]; then
    WY_TREE_BARE=1 bash host/tests/wy/tools/gpu-tree.sh "$W/tree" "$W/bitscomp-gpu"
    rm -f "$W/tree/lib/libpixman-1.so.0"
else
    bash host/tests/wy/tools/gpu-tree.sh "$W/tree" "$W/bitscomp-gpu"
fi
ESP=$CARGO_TARGET_DIR/xtask/esp-$NAME
IMG=$CARGO_TARGET_DIR/xtask/$NAME.img
[ -d "$ESP/BITSOS2" ] || { echo "RED: no ESP tree $ESP (cargo xtask image --name $NAME first)"; exit 1; }
rm -rf "$ESP/BITSOS2/mesa"
[ "$WY_GPU" = notree ] || cp -r "$W/tree" "$ESP/BITSOS2/mesa"
# WY-3: WY_XWAYLAND=1 adds the Xwayland tree with its test clients at \BITSOS2\xwayland\ (user/ports/xwayland).
rm -rf "$ESP/BITSOS2/xwayland"
if [ "${WY_XWAYLAND:-0}" = 1 ]; then
    XW_TEST=1 bash user/ports/xwayland/scripts/build-v2.sh
    cp -r "$CARGO_TARGET_DIR/wy-xwayland/xwayland" "$ESP/BITSOS2/xwayland"
fi
mkdir -p "$ESP/BITSOS2/bin"
cp "$W/bitscomp-gpu" "$ESP/BITSOS2/bin/bitscomp-gpu"
bytes=$(du -sb "$ESP" | cut -f1)
kib=$(( (bytes + bytes / 4 + (64 << 20)) / 1048576 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "$ESP/EFI" "$ESP/BITSOS2" ::/
want=gpu
[ "$WY_GPU" = gles ] && want=gles
grep -q "^bitscomp=$want" "$ESP/BITSOS2/CONFIG.TXT" || { echo "RED: CONFIG.TXT of $NAME has no bitscomp=$want"; exit 1; }
[ "${WY_XWAYLAND:-0}" = 1 ] && echo "gpu-image: $NAME carries \BITSOS2\xwayland\ (Xwayland and the X11 test clients)"
echo "gpu-image ($WY_GPU): $NAME carries \\BITSOS2\\bin\\bitscomp-gpu$([ "$WY_GPU" = notree ] && echo ' and no Mesa tree' || echo ' and \BITSOS2\mesa\') ($((bytes >> 20)) MB ESP), CONFIG.TXT bitscomp=$want"
