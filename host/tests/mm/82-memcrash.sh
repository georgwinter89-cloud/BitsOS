#!/bin/bash
# slow
# time-limit: 1200
# MEMCRASH: the Mini-PC's panic of 13.11.2026 (generation 150, a 60 GB download) in small: 2 GiB, 12 CPUs, three
# minutes of file writes and reads beside a program with a few hundred VMAs whose threads look up futex keys while
# others unmap and map, and kernel buffers of order 2 and 3 (crates/mm/src/memcrash_ktest.rs). Run it alone.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mm-82-memcrash}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
NAME="${BITSOS_LANE:-mm}-crash"
LOG="$RUN_DIR/serial.log"
fail() { echo "RED: $*"; exit 1; }

cargo xtask image --name "$NAME" --ktest --config-extra "ktest=mm.memcrash;ktest_run=mm/82-memcrash;loglevel=info"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --smp 12 --mem 2G --timeout 600 --serial-log "$LOG" || true
touch "$LOG"
grep -a "KTEST\|ktest.memcrash\|oom\.\|PANIC\|panic" "$LOG" || true
grep -aq "KTEST done passed=[0-9]* failed=0" "$LOG" || fail "mm.memcrash not green"
if grep -aq "KTEST FAIL\|KTEST PANIC" "$LOG"; then fail "mm.memcrash"; fi
grep -aq "KTEST ok mm.memcrash" "$LOG" || fail "mm.memcrash did not run"
echo "mm memcrash: green"
