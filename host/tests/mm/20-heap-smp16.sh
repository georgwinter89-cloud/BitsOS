#!/bin/bash
# MM-3: every ktest on a 16-CPU machine (-smp 16, 4G), functional only. Until CP-4 starts the APs only the
# BSP runs kernel code, so this proves the slab heap and its per-CPU magazines boot and pass with the MADT of
# 16 CPUs; once CP-4 is in, the same run has the APs online (the `mm ktest.heap` line then shows cpus=16, and
# `magazine_cpus` shows how many CPUs got magazines: change request MM-1 #3 for PerCpu growth).
# MM-5a adds the address-space ktests and the no-leak check below; MM-6 the shootdown checks (40-tlb-smp.sh
# repeats the TLB tests with and without PCID).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mm-20-heap-smp16}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
NAME="${BITSOS_LANE:-mm}-smp16"
log="$RUN_DIR/serial.log"

cargo xtask image --name "$NAME" --ktest --config-extra "loglevel=info"
cargo xtask qemu --name "$NAME" --smp 16 --mem 4G --timeout 300 --serial-log "$log" || true
touch "$log"
grep -a "KTEST done\|KTEST FAIL\|KTEST PANIC\|heap.ready\|ktest.heap\|heap.bad_free\|heap.corrupt" "$log" || true
fail=0
grep -aq "KTEST done passed=[0-9]* failed=0" "$log" || { echo "RED: ktests not green with -smp 16"; fail=1; }
if grep -aq "KTEST FAIL\|KTEST PANIC" "$log"; then echo "RED: a ktest failed or the kernel panicked"; fail=1; fi
for t in classes stress_sizes vec_growth arc_churn bad_frees_refused slabs_return named_cache stats_consistent speed; do
    grep -aq "KTEST ok mm.heap.$t" "$log" || { echo "RED: mm.heap.$t did not pass"; fail=1; }
done
grep -aq "KTEST ok contract.global_alloc" "$log" || { echo "RED: contract.global_alloc"; fail=1; }
# MM-5a: address spaces with 16 CPUs online; a space only the BSP loaded is freed, not leaked.
for t in lazy_fill guard split brk user_touch abi_diff eager frees; do
    grep -aq "KTEST ok mm.as.$t" "$log" || { echo "RED: mm.as.$t did not pass"; fail=1; }
done
grep -aq "KTEST ok mm.vmap.reserve" "$log" || { echo "RED: mm.vmap.reserve"; fail=1; }
grep -aq "as.drop_leaked" "$log" && { echo "RED: an address space leaked on drop"; fail=1; }
# MM-6: with 16 CPUs the vmap ranges go back (shootdown) instead of into the quarantine, DMA UC/WC works.
for t in vmap.guard_fault vmap.unmap_frees_tables dma.uc_wc_pat tlb.storm tlb.stale_user; do
    grep -aq "KTEST ok mm.$t" "$log" || { echo "RED: mm.$t did not pass"; fail=1; }
done
grep -aq "ktest.kva .*released=true cpus=16" "$log" || { echo "RED: kernel VA not released with 16 CPUs"; fail=1; }
grep -aq "vmap.quarantined\|vmap.kva_quarantined" "$log" && { echo "RED: a vmap range was quarantined"; fail=1; }
grep -aq "heap.corrupt" "$log" && { echo "RED: heap.corrupt event"; fail=1; }
[ $fail = 0 ] && echo "mm heap smp16: green"
exit $fail
