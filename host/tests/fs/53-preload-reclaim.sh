#!/bin/bash
# FS-13b: the start-up preload with a small RAM. fs.preload.reclaim reads a file larger than free memory with
# the service's floor (a quarter of RAM): the pass stops at the floor (memory); then program memory beyond what
# is free: no allocation fails, reclaim takes the preloaded pages nobody used first (the read file and the used
# preloaded page stay), and a pass budget made before the spike stops with "reclaim". The other fs.preload.*
# ktests run in the same boot. 512M as in mm/60-reclaim.sh; when OVMF stalls there, once more with 1G (said on
# the last line and in $RUN_DIR/mem); BITSOS_RECLAIM_STRICT=1 forbids the fallback.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/fs-53-preload-reclaim}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
NAME=fs-preload-reclaim
TESTS="fs.preload."
fail() { echo "RED: $*"; exit 1; }

cargo xtask image --name "$NAME" --ktest --config-extra "ktest=$TESTS;loglevel=info"

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
echo "fs preload reclaim: ran with -m $MEM"
grep -a "KTEST\|ktest.preload_reclaim\|kswapd\.\|reclaim.direct_failed" "$LOG" || true
grep -aq "KTEST done passed=[0-9]* failed=0" "$LOG" || fail "fs.preload ktests not green ($MEM)"
if grep -aq "KTEST FAIL\|KTEST PANIC" "$LOG"; then fail "fs.preload ($MEM)"; fi
grep -aq "KTEST ok fs.preload.reclaim" "$LOG" || fail "the preload reclaim test did not run with $MEM"
grep -aq "reclaim.direct_failed" "$LOG" && fail "an allocation failed after reclaim ($MEM)"
if [ "$MEM" = 512M ]; then echo "fs preload reclaim (512M): green"; else echo "fs preload reclaim: green with the 1G fallback (512M did not boot)"; fi
