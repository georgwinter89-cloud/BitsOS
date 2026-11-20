#!/bin/bash
# slow
# SELFOPT-1: the sampling profiler end to end on 4 CPUs. One boot runs `so.perf.bitsprof`: profbusy (a program with
# one hot function) runs once before and once after `bitsprof run` started; `bitsprof report` must name
# profbusy_hot_loop in /bin/profbusy as profbusy's hottest function, meta.json must say BitsOS with kernel names.
# The report's first lines are in the log (`ktest so.perf.report line=...`).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/so-10-profile}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
NAME="${BITSOS_LANE:-so}-profile"
LOG="$RUN_DIR/serial.log"
fail() { echo "RED: $*"; exit 1; }

cargo xtask image --name "$NAME" --ktest --config-extra "ktest=so.perf.bitsprof;ktest_run=so/10-profile;loglevel=info"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --smp 4 --mem 2G --timeout 300 --serial-log "$LOG" || true
touch "$LOG"
grep -a "KTEST\|so.perf.report" "$LOG" || true
if grep -aq "KTEST FAIL\|KTEST PANIC" "$LOG"; then fail "so.perf.bitsprof"; fi
grep -aq "KTEST ok so.perf.bitsprof" "$LOG" || fail "so.perf.bitsprof did not run"
grep -aq "KTEST done passed=1 failed=0" "$LOG" || fail "the boot did not end green"
echo "so profile: green"
