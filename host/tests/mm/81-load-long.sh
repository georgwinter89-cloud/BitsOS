#!/bin/bash
# slow
# time-limit: 1800
# MM2-NIGHT: the load of 80-load.sh for ten minutes with a second, larger program beside it (a sixth of memory, ten
# threads with regions up to 8 MiB, six more CPUs keeping it loaded): the Mini-PC's 30 minutes of Steam and a game
# beside a full cache (generation 137), as close as QEMU gets. Judged (crates/mm/src/load_ktest.rs): the bounds of
# 80-load.sh over the whole run, every minute against the desk's stop numbers (`mm ktest.load_min` lines), and the
# longest wait for and hold of mm.as (250 ms).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mm-81-load-long}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
NAME="${BITSOS_LANE:-mm}-loadl"
LOG="$RUN_DIR/serial.log"
fail() { echo "RED: $*"; exit 1; }

cargo xtask image --name "$NAME" --ktest --config-extra "ktest=mm.migrate.load_long;ktest_run=mm/81-load-long;loglevel=info"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --smp 16 --mem 6G --timeout 1500 --serial-log "$LOG" || true
touch "$LOG"
grep -a "KTEST\|ktest.load \|ktest.load_min" "$LOG" || true
grep -aq "KTEST done passed=[0-9]* failed=0" "$LOG" || fail "mm.migrate.load_long not green"
if grep -aq "KTEST FAIL\|KTEST PANIC" "$LOG"; then fail "mm.migrate.load_long"; fi
grep -aq "KTEST ok mm.migrate.load_long" "$LOG" || fail "mm.migrate.load_long did not run"
echo "mm load (long): green"
