#!/bin/bash
# VS-1: the bitsvol ktests under a real boot (QEMU, 4 CPUs): model, crash images, stacked device, out of space,
# rollback, damaged superblock. All build their own RAM device; nothing touches a disk.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/vs-10-bitsvol}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LOG="$RUN_DIR/serial.log"
BOOT_LINE="BdsDxe: loading"

cargo xtask image --name vs-bitsvol --ktest --config-extra "ktest=bitsvol."
for attempt in 1 2; do
    if cargo xtask qemu --name vs-bitsvol --timeout 300 --serial-log "$LOG" -- -smp 4; then
        break
    fi
    grep -aq "$BOOT_LINE" "$LOG" && { echo "RED: ktests failed"; grep -a "KTEST" "$LOG"; exit 1; }
    echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
done
grep -a "KTEST" "$LOG" || true
grep -aq "KTEST done passed=.* failed=0" "$LOG" || { echo "RED: no clean KTEST done line"; exit 1; }
ok=$(grep -ac "KTEST ok bitsvol\." "$LOG" || true)
[ "$ok" -ge 6 ] || { echo "RED: only $ok bitsvol ktests passed (want 6)"; exit 1; }
echo "GREEN: $ok bitsvol ktests"
