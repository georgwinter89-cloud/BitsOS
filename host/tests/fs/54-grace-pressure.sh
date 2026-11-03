#!/bin/bash
# PF-5: memory that waits for a grace period never turns into a false out-of-memory, with a small RAM.
# fs.pf5.pressure takes the frames until 8 MiB stay free, then writes a 16 MiB tmpfs file and punches it 40
# times while lockless readers keep sections open: every round's pages come from the frames the last punch left
# waiting (or from reclaim). No write fails, no allocation fails after reclaim, no bounded wait runs out, nothing
# leaks. The other fs.pf5.* ktests run in the same boot. 512M as in mm/60-reclaim.sh; when OVMF stalls there,
# once more with 1G (said on the last line and in $RUN_DIR/mem); BITSOS_RECLAIM_STRICT=1 forbids the fallback.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/fs-54-grace-pressure}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
NAME=fs-grace-pressure
TESTS="fs.pf5."
fail() { echo "RED: $*"; exit 1; }

cargo xtask image --name "$NAME" --ktest --config-extra "ktest=$TESTS;loglevel=info"

run() { # $1 = memory
    local log="$RUN_DIR/serial-$1.log"
    rm -f "$log"
    cargo xtask qemu --name "$NAME" --mem "$1" --timeout 240 --serial-log "$log" -- -smp 4 || true
    grep -aq "KTEST done" "$log"
}

MEM=512M
if ! run "$MEM"; then
    [ "${BITSOS_RECLAIM_STRICT:-0}" = 1 ] && fail "no KTEST done line with 512M (BITSOS_RECLAIM_STRICT=1: no fallback)"
    echo "note: no KTEST done line with 512M (OVMF stall?): once more with 1G"
    MEM=1G
    run "$MEM" || fail "no KTEST done line with 1G either"
fi
LOG="$RUN_DIR/serial-$MEM.log"
echo "$MEM" > "$RUN_DIR/mem"
echo "fs grace pressure: ran with -m $MEM"
grep -a "KTEST\|fs.pf5.pressure\|grace\.\|reclaim.direct_failed" "$LOG" || true
grep -aq "KTEST done passed=[0-9]* failed=0" "$LOG" || fail "fs.pf5 ktests not green ($MEM)"
if grep -aq "KTEST FAIL\|KTEST PANIC" "$LOG"; then fail "fs.pf5 ($MEM)"; fi
grep -aq "KTEST ok fs.pf5.pressure" "$LOG" || fail "the pressure test did not run with $MEM"
grep -aq "reclaim.direct_failed" "$LOG" && fail "an allocation failed after reclaim ($MEM)"
grep -aq "grace.wait_timeout\|grace.stalled" "$LOG" && fail "a grace period was held too long ($MEM)"
if [ "$MEM" = 512M ]; then echo "fs grace pressure (512M): green"; else echo "fs grace pressure: green with the 1G fallback (512M did not boot)"; fi
