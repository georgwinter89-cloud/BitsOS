#!/bin/bash
# slow
# MM-10: mm.tlb.give_up alone, ROUNDS times (default 2, MM_GIVE_UP_ROUNDS=30 for the stability proof) with
# 16 CPUs while LOAD busy loops (default: one per host CPU, MM_GIVE_UP_LOAD) compete with QEMU's vCPUs. The test
# holds its AP until the unmap gave up, so host load must not make it fail. Functional checks only.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mm-41-tlb-give-up-loop}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
ROUNDS="${MM_GIVE_UP_ROUNDS:-2}"
LOAD="${MM_GIVE_UP_LOAD:-$(nproc)}"
NAME="${BITSOS_LANE:-mm}-give-up-loop"
fail=0
pids=()
stop_load() { for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done; pids=(); }
trap stop_load EXIT

cargo xtask image --name "$NAME" --ktest --config-extra "ktest=mm.tlb.give_up,-mm.tlb.give_up_device;loglevel=info"
for _ in $(seq 1 "$LOAD"); do
    (while :; do :; done) &
    pids+=("$!")
done
for r in $(seq 1 "$ROUNDS"); do
    log="$RUN_DIR/$NAME-$r.log"
    cargo xtask qemu --name "$NAME" --smp 16 --mem 4G --timeout 300 --serial-log "$log" || true
    touch "$log"
    if grep -aq "KTEST ok mm.tlb.give_up" "$log" && grep -aq "KTEST done passed=1 failed=0 skipped=0" "$log"; then
        echo "round $r: ok $(grep -ao "ktest.tlb_give_up .*" "$log" | head -1)"
    else
        echo "RED: round $r: mm.tlb.give_up did not pass"
        grep -a "KTEST FAIL\|KTEST PANIC\|KTEST skip\|KTEST done" "$log" || true
        fail=1
    fi
    [ "$(grep -ac "as.shootdown_unconfirmed" "$log")" -le 1 ] || { echo "RED: round $r: more than one unconfirmed shootdown"; fail=1; }
done
stop_load
[ $fail = 0 ] && echo "mm give_up loop: green ($ROUNDS rounds, $LOAD load loops)"
exit $fail
