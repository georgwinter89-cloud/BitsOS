#!/bin/bash
# RB-2 helper (not a scenario: no number prefix): puts the symbol table of the kernel an image boots into the
# image as \BITSOS2\fw\KSYMS, which bitsboot loads as a FIRMWARE module and kexec::crash reads at boot.
# Until the desk's xtask writes boot/kernel.syms into every initramfs (change request in plan/notes/rb-RB-2.md),
# the RB scenarios add it this way. Usage: ksyms.sh <image name> [ktest|plain] (debug profile).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
NAME="$1"
KIND="${2:-ktest}"
X=target/xtask
IMG="$X/$NAME.img"
KERNEL="$X/kernel-debug-$KIND"
[ -f "$IMG" ] && [ -f "$KERNEL" ] || { echo "ksyms.sh: $IMG or $KERNEL missing"; exit 1; }
SYMS="$X/$NAME.ksyms"
nm -n -C --defined-only "$KERNEL" | grep -E '^[0-9a-f]{16} [tTwW] ' > "$SYMS"
mmd -i "$IMG" ::/BITSOS2/fw 2> /dev/null || true
mcopy -o -i "$IMG" "$SYMS" ::/BITSOS2/fw/KSYMS
echo "ksyms: $(wc -l < "$SYMS") code symbols ($(stat -c %s "$SYMS") bytes) from $KERNEL into $IMG as \\BITSOS2\\fw\\KSYMS"
