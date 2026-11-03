#!/bin/bash
# WY-14: builds bitscomp's gpu build and the Mesa tree it runs with, for a test.
#   gpu-tree.sh TREE BIN [GPU_ROOT]
# TREE: a copy of the staged Mesa tree (user/ports/mesa/scripts/stage-v2.sh; MESA_STAGE overrides where it is)
# with the libraries the gpu build links that the stage lacks today (libpixman-1; the host's files, the same
# packages as the stage's: GM change request WY-14/2 adds them to the stage). BIN: the gpu build
# (`--no-default-features --features gpu`, own target directory), linked for GPU_ROOT (default the stick's
# /boot/BITSOS2/mesa). Then every library of BIN must resolve inside TREE/lib with the tree's own loader (no
# system library next to the tree's glibc), else RED. WY_STRICT_TREE=1: RED when the stage itself lacks one;
# WY_TREE_BARE=1: the stage as it is, nothing added and nothing checked (the fallback tests).
set -euo pipefail
TREE=$1 BIN=$2 GPU_ROOT=${3:-/boot/BITSOS2/mesa}
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT=$PWD
STAGE=${MESA_STAGE:-$ROOT/user/ports/mesa/out/mesa}
[ -f "$STAGE/lib/ld-linux-x86-64.so.2" ] || { echo "RED: no staged Mesa in $STAGE (bash user/ports/mesa/scripts/stage-v2.sh)"; exit 1; }
# WY_GPU_NESTED=1: the gpu build with the `nested` feature too (the host scenarios' test paths, e.g. `ctl phys`).
FEAT=gpu TD=${CARGO_TARGET_DIR:-$ROOT/target}/wy-gpu
[ "${WY_GPU_NESTED:-0}" = 1 ] && FEAT=gpu,nested TD=$TD-nested
(cd user/services/bitscomp && BITSCOMP_GPU_ROOT=$GPU_ROOT CARGO_TARGET_DIR=$TD \
    cargo build --release -q --offline --locked --no-default-features --features "$FEAT")
rm -rf "$TREE"
mkdir -p "$(dirname "$TREE")"
cp -r "$STAGE" "$TREE"
added=()
for so in $(readelf -d "$TD/release/bitscomp" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
    [ -f "$TREE/lib/$so" ] && continue
    src=/usr/lib/$so
    [ -f "$src" ] || { echo "RED: $so: neither in the stage nor on the host"; exit 1; }
    added+=("$so")
    [ "${WY_STRICT_TREE:-0}" = 1 ] || [ "${WY_TREE_BARE:-0}" = 1 ] && continue
    cp -L "$src" "$TREE/lib/$so"
    echo "gpu-tree: $so added from the host (not in the stage)"
done
# The stage by itself must carry what the gpu build links (GM change request WY-14/2); until then this is a known
# gap: loud here, RED under WY_STRICT_TREE=1. Without the libraries the gpu build's check fails and the rescue
# build runs on (flavour::check_gpu), so a stick with the bare stage keeps working.
cp "$TD/release/bitscomp" "$BIN"
if [ "${WY_TREE_BARE:-0}" = 1 ]; then
    # The stage as it is (a fallback test: the gpu build may not load from it).
    echo "gpu-tree: $BIN with the bare stage in $TREE (the stage lacks: ${added[*]:-nothing})"
    exit 0
fi
if [ ${#added[@]} -gt 0 ]; then
    if [ "${WY_STRICT_TREE:-0}" = 1 ]; then
        echo "RED (WY_STRICT_TREE=1): the Mesa stage lacks ${added[*]} (GM change request WY-14/2)"
        exit 1
    fi
    echo "gpu-tree: WARNING KNOWN-GAP: the Mesa stage lacks ${added[*]}; this test tree has them from the host" \
        "(GM change request WY-14/2; WY_STRICT_TREE=1 makes this RED)"
fi
out=$("$TREE/lib/ld-linux-x86-64.so.2" --inhibit-cache --library-path "$TREE/lib" --list "$BIN" 2>&1) \
    || { echo "RED: the tree's loader cannot load $BIN: $out"; exit 1; }
miss=$(echo "$out" | grep -v -e "=> $TREE/lib/" -e "linux-vdso" -e "^[[:space:]]*/" -e "^[[:space:]]*$TREE/lib/" || true)
[ -z "$miss" ] || { echo "RED: libraries of $BIN outside the tree:"; echo "$miss"; exit 1; }
interp=$(readelf -p .interp "$BIN" | sed -n 's/.*\] *//p')
[ "$interp" = "$GPU_ROOT/lib/ld-linux-x86-64.so.2" ] || { echo "RED: $BIN's interpreter is $interp"; exit 1; }
readelf -d "$BIN" | grep -q "(RPATH).*\[$GPU_ROOT/lib\]" || { echo "RED: $BIN has no DT_RPATH $GPU_ROOT/lib"; exit 1; }
echo "gpu-tree: $BIN (interpreter $interp) and its libraries in $TREE/lib"
