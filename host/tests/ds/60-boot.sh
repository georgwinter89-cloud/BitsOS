#!/usr/bin/env bash
# DS-5 on the host (headless back end, nothing on the screen): bitscomp's half of the boot screen from a fixture of
# /proc/bitsos/boot (BITSCOMP_BOOT_FILE). Its picture equals the kernel's (crates/console, drawn by console's own
# test) pixel for pixel outside the desktop row and the hint; the wait for a window ends at the cap or with the
# window; the fade takes at most 400 ms; afterwards the still desktop (one window) draws 0 frames; odd files, another screen state
# and an earlier start's step show no scene. QEMU (the kernel's real screen, bitscomp's first frame, a key): 61-boot-qemu.sh.
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
unset DISPLAY WAYLAND_DISPLAY

echo "== build bitscomp, wyclient"
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)
B=$RUN/boot
rm -rf "$B"
mkdir -m 700 -p "$B"
python3 -I "$ROOT/host/tests/ds/ds_boot.py" "$CARGO_TARGET_DIR/release/bitscomp" "$CARGO_TARGET_DIR/release/wyclient" "$B" "$ROOT"
