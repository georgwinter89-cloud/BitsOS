#!/bin/bash
# slow
# W35 MM-2: the load that hung the Mini-PC on 12.11.2026 (generation 137), small: mm.migrate.load on 16 CPUs with
# 6G. A program with gigabytes of anonymous memory whose threads unmap, map and fault, CPUs that keep its address
# space loaded, a page cache that fills memory, and kernel threads taking and dropping buffers of 68, 12 and 28 KiB.
# Judged are counts over the run (crates/mm/src/load_ktest.rs: interrupts for freed kernel ranges, drains of the
# per-CPU frame lists, frames read per direct compaction run, a round of the program every second); times are
# written out only. The `mm ktest.load_sec` lines hold each second's counters.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mm-80-load}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
NAME="${BITSOS_LANE:-mm}-load"
LOG="$RUN_DIR/serial.log"
fail() { echo "RED: $*"; exit 1; }

cargo xtask image --name "$NAME" --ktest --config-extra "ktest=mm.migrate.load;ktest_run=mm/80-load;loglevel=info"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --smp 16 --mem 6G --timeout 600 --serial-log "$LOG" || true
touch "$LOG"
grep -a "KTEST\|ktest.load " "$LOG" || true
grep -aq "KTEST done passed=[0-9]* failed=0" "$LOG" || fail "mm.migrate.load not green"
if grep -aq "KTEST FAIL\|KTEST PANIC" "$LOG"; then fail "mm.migrate.load"; fi
grep -aq "KTEST ok mm.migrate.load" "$LOG" || fail "mm.migrate.load did not run"
echo "mm load: green"
