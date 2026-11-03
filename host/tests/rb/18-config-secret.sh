#!/bin/bash
# RB-18: a boot with a token and a made-up passphrase in CONFIG.TXT: kernel.config.secret reads the BootInfo CONFIG
# record from RAM and finds no value in it, both values blanked, and both reached the kernel first (config::init
# before the blanking). The plain gate's boot has no secret in CONFIG, so only this run proves that part.
# rb/20-reload.sh does the same after each Reload (slow, no plain gate).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/rb-18-config-secret}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LOG="$RUN_DIR/serial.log"
BOOT_LINE="BdsDxe: loading"
TOKEN="rb18-fake-token-not-real"
PSK="rb18-fake-psk-not-real"

cargo xtask image --name rb-config-secret --ktest --config-extra "ktest=kernel.config.secret;token=$TOKEN;wlan_psk=$PSK;rb18_expect_blanked=2"
for attempt in 1 2; do
    if cargo xtask qemu --name rb-config-secret --timeout 120 --serial-log "$LOG" --net virtio; then
        break
    fi
    grep -aq "$BOOT_LINE" "$LOG" && { echo "RED: the ktest failed"; grep -a "KTEST" "$LOG"; exit 1; }
    echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
done
grep -a "KTEST" "$LOG" || true
grep -aq "KTEST ok kernel.config.secret" "$LOG" || { echo "RED: kernel.config.secret did not pass"; exit 1; }
grep -aq "KTEST done passed=1 failed=0 skipped=0" "$LOG" || { echo "RED: no clean KTEST done line"; exit 1; }
if grep -aqF -e "$TOKEN" -e "$PSK" "$LOG"; then
    echo "RED: a secret value is in the serial log"
    exit 1
fi
echo "ok   the record holds no secret, both arrived, neither is in the log (RB-18)"
