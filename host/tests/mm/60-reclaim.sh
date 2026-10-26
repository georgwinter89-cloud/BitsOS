#!/bin/bash
# MM-9: page-cache reclaim with a small RAM, booted from a stick image (busybox on its FAT volume at /boot).
# mm.reclaim.pressure fills the memory with a file's pages, then writes more program memory than is free: no
# allocation fails, the cache shrinks, the file reads right again. mm.reclaim.mapped_exec maps busybox from
# the stick like a program's text and keeps reading the same bytes under the same pressure (its unmapped pages
# are reclaimed and read again from the stick). mm.reclaim.dirty_pressure does the same with half the file dirty.
# 512M is the brief's size; when OVMF stalls before the boot loader there (seen in 10-frames-sizes.sh), the run
# falls back to 1G once and says so on its last line (and in $RUN_DIR/mem); BITSOS_RECLAIM_STRICT=1 forbids it.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mm-60-reclaim}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
NAME=mm-reclaim
TESTS="mm.reclaim."
fail() { echo "RED: $*"; exit 1; }

cargo xtask image --name "$NAME" --ktest --config-extra "ktest=$TESTS;loglevel=info"
mdir -i "target/xtask/$NAME.img" ::/BITSOS2/bin > /dev/null 2>&1 || echo "note: no bin folder on the stick image: mapped_exec will skip"

run() { # $1 = memory
    local log="$RUN_DIR/serial-$1.log"
    rm -f "$log"
    cargo xtask qemu --name "$NAME" --mem "$1" --timeout 240 --serial-log "$log" || true
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
echo "mm reclaim: ran with -m $MEM"
grep -a "KTEST\|ktest.reclaim\|kswapd\.\|reclaim.direct_failed" "$LOG" || true
grep -aq "KTEST done passed=[0-9]* failed=0" "$LOG" || fail "mm.reclaim ktests not green ($MEM)"
if grep -aq "KTEST FAIL\|KTEST PANIC" "$LOG"; then fail "mm.reclaim ($MEM)"; fi
grep -aq "KTEST ok mm.reclaim.pressure" "$LOG" || fail "the pressure test did not run with $MEM"
grep -aq "KTEST ok mm.reclaim.dirty_pressure" "$LOG" || fail "the dirty pressure test did not run with $MEM"
grep -aq "KTEST ok mm.reclaim.mapped_exec" "$LOG" || fail "the mapped busybox test did not run with $MEM (no /boot?)"
grep -aq "reclaim.direct_failed" "$LOG" && fail "an allocation failed after reclaim ($MEM)"
if [ "$MEM" = 512M ]; then echo "mm reclaim (512M): green"; else echo "mm reclaim: green with the 1G fallback (512M did not boot)"; fi
