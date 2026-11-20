#!/bin/bash
# slow
# time-limit: 600
# MEMCRASH row 2: allocations too small to fail. A program holds nearly all of 1 GiB, then a kernel thread takes
# 96 MiB of 28 KiB heap blocks: none may fail, the OOM killer must end the program (crates/proc/src/oom_ktest.rs).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mm-83-oom}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
NAME="${BITSOS_LANE:-mm}-oom"
LOG="$RUN_DIR/serial.log"
fail() { echo "RED: $*"; exit 1; }

cargo xtask image --name "$NAME" --ktest --config-extra "ktest=mm.oom.pressure;ktest_run=mm/83-oom;loglevel=info"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --smp 4 --mem 1G --timeout 300 --serial-log "$LOG" || true
touch "$LOG"
grep -a "KTEST\|ktest.oom_pressure\|oom\.\|PANIC\|panic" "$LOG" || true
grep -aq "KTEST done passed=[0-9]* failed=0" "$LOG" || fail "mm.oom.pressure not green"
if grep -aq "KTEST FAIL\|KTEST PANIC" "$LOG"; then fail "mm.oom.pressure"; fi
grep -aq "KTEST ok mm.oom.pressure" "$LOG" || fail "mm.oom.pressure did not run"
echo "mm oom pressure: green"
