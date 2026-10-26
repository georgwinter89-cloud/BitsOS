#!/bin/bash
# WL: the rtw89 driver thread's stack in the dev kernel (opt-level 1, what the stick runs). On the Mini-PC with
# wlan=on the thread ran into its guard page in Device::bring_up (2026-10-22: card::run, Runner::bring_up,
# Device::bring_up and Device::up held a Device by value, 18-26 KiB per frame, on a 64 KiB stack). QEMU has no
# RTL8852BE and the fake chip is a node process, so no ktest enters these frames: stack_check.py measures them in
# the built kernel's machine code instead (prologues with their probe pages, the direct calls from card::run through
# rtw89, rtw89_core and wifi, the deepest driver hook on top) and fails above 24 KiB for the path or 8 KiB for one
# frame. Host only: no QEMU, no ports, no hardware.
set -euo pipefail
ROOT="${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
cd "$ROOT"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wl-20-stack}"
mkdir -p "$RUN_DIR" target/xtask

echo "== stack_check.py --selftest"
python3 host/tests/wl/stack_check.py --selftest

# xtask's kernel lock: parallel chains build `--features ktest` into the same file (image.rs build_kernel)
echo "== the dev kernel, plain"
flock target/xtask/kernel-build.lock bash -c '
    CARGO_TARGET_DIR="$1/target" cargo build --offline -q -p kernel --target x86_64-unknown-none &&
    cp "$1/target/x86_64-unknown-none/debug/kernel" "$2/kernel-dev"' _ "$ROOT" "$RUN_DIR" 2>&1 | tee "$RUN_DIR/build.log"
test -s "$RUN_DIR/kernel-dev" || { echo "RED: no kernel built (see $RUN_DIR/build.log)"; exit 1; }

echo "== the rtw89 thread's frames"
if python3 host/tests/wl/stack_check.py "$RUN_DIR/kernel-dev" | tee "$RUN_DIR/stack.txt"; then
    rm -f "$RUN_DIR/kernel-dev"
    echo "GREEN"
else
    echo "RED: the rtw89 thread's stack is over budget (see $RUN_DIR/stack.txt)"
    exit 1
fi
