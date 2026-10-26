#!/bin/bash
# slow
# MM-6 (+ MM-5b's fork): the TLB shootdown ktests (and every other MM test) with 16 CPUs, once with PCID and once with the CPU's
# PCID/INVPCID hidden (`-cpu host,-pcid,-invpcid`: every switch flushes, full kernel flushes toggle CR4.PGE),
# each ROUNDS times (default 2): the storm is a race test, one green run proves little. Functional checks only.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mm-40-tlb-smp}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
ROUNDS="${MM_TLB_ROUNDS:-2}"
TESTS="mm.,contract.address_space,contract.frame_alloc"
fail=0

run() { # $1 = name, $2.. = extra QEMU arguments
    local name="$1"
    shift
    local log="$RUN_DIR/$name.log"
    cargo xtask image --name "$name" --ktest --config-extra "ktest=$TESTS;loglevel=info"
    cargo xtask qemu --name "$name" --smp 16 --mem 4G --timeout 300 --serial-log "$log" -- "$@" || true
    touch "$log"
    grep -a "KTEST FAIL\|KTEST PANIC\|KTEST done\|ktest.tlb\|tlb.ready\|tlb.give_up\|as.drop_leaked\|vmap.quarantined" "$log" || true
    grep -aq "KTEST done passed=[0-9]* failed=0" "$log" || { echo "RED: $name not green"; fail=1; return; }
    if grep -aq "KTEST FAIL\|KTEST PANIC" "$log"; then echo "RED: $name: a ktest failed or the kernel panicked"; fail=1; fi
    for t in ready kernel_global stale_user stale_kernel stacks resend give_up storm; do
        grep -aq "KTEST ok mm.tlb.$t" "$log" || { echo "RED: $name: mm.tlb.$t did not pass"; fail=1; }
    done
    grep -aq "ktest.tlb_storm cpus=16 .* stale=0" "$log" || { echo "RED: $name: the storm did not run on 16 CPUs without stale accesses"; fail=1; }
    # MM-5b: fork write-protects the parent on every CPU (15 APs hold writable translations while the BSP forks).
    grep -aq "ktest.cow_smp aps=15" "$log" || { echo "RED: $name: mm.cow.smp_tlb did not run with 15 APs"; fail=1; }
    # Only the deliberate give-up test may leave an unconfirmed shootdown; nothing leaks or stays quarantined.
    [ "$(grep -ac "as.shootdown_unconfirmed" "$log")" -le 1 ] || { echo "RED: $name: unconfirmed shootdowns outside mm.tlb.give_up"; fail=1; }
    if grep -aq "as.drop_leaked\|vmap.quarantined\|vmap.kva_quarantined" "$log"; then echo "RED: $name: leak or quarantine"; fail=1; fi
}

for r in $(seq 1 "$ROUNDS"); do
    run "${BITSOS_LANE:-mm}-tlb16-pcid-$r"
    grep -aq "tlb.ready .*pcid=true" "$RUN_DIR/${BITSOS_LANE:-mm}-tlb16-pcid-$r.log" || echo "note: this host CPU offers no PCID (the PCID run equals the other one)"
    run "${BITSOS_LANE:-mm}-tlb16-nopcid-$r" -cpu host,-pcid,-invpcid
    grep -aq "tlb.ready .*pcid=false" "$RUN_DIR/${BITSOS_LANE:-mm}-tlb16-nopcid-$r.log" || { echo "RED: PCID still visible with -pcid"; fail=1; }
done
[ $fail = 0 ] && echo "mm tlb smp16: green"
exit $fail
