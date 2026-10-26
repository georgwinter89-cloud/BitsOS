#!/bin/bash
# slow
# SC-3: the scheduler on many CPUs in QEMU (functional only; timing is judged on the Mini-PC).
#  1. -smp 16 (KVM, else TCG): all 15 APs join, every sc./sched. ktest passes, sched.scale64 included.
#  2. an SMT/cluster topology (-smp 16 as 2 sockets x 2 dies x 2 cores x 2 threads with KVM): placement, steal,
#     push and the class tests on SMT siblings and several cache clusters.
#  3. TCG -smp 4 (no KVM timing help): the wake-up, balance, reaper and class tests.
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
fail=0
check() { if grep -aEq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3 (no '$2' in $(basename "$1"))"; fail=1; fi; }
count() { grep -acE -- "$2" "$1" || true; }

run() { # tag, ktest filter, qemu args...
    local tag="$1" filter="$2"
    local name="$BITSOS_LANE-$tag"
    shift 2
    cargo xtask image --name "$name" --ktest --config-extra "ktest=$filter" > "$RUN/$tag-image.txt" 2>&1
    cargo xtask qemu --name "$name" --timeout 600 --boot-retries 4 --serial-log "$RUN/$tag.log" -- "$@" > "$RUN/$tag-qemu.txt" 2>&1 || true
    touch "$RUN/$tag.log"
}

# 1. 16 CPUs.
run smp16 "sc.,sched." -smp 16
L="$RUN/smp16.log"
# 15 APs print their join event at the same moment; serial lines may interleave, so count from the ktest's event.
check "$L" 'sched smp.handover aps=15 cpus=16' "the 15 APs were handed to the scheduler"
check "$L" 'sched ktest.cpus online=16 ' "all 16 CPUs schedule"

check "$L" 'KTEST ok sched.scale64' "64 busy threads on 16 CPUs: every wake arrived, every thread advanced"
check "$L" 'sched ktest.scale64 cpus=16 ' "scale64 ran with 16 CPUs"
check "$L" 'KTEST ok sched.cpus.joined' "a pinned thread ran on every CPU"
check "$L" 'KTEST ok sched.wake.irq' "wake-ups from an interrupt handler"
check "$L" 'KTEST ok sched.balance.spread' "threads started on one CPU spread over all"
check "$L" 'KTEST done passed=[0-9]+ failed=0 skipped=0' "every sc./sched. ktest passed"
if grep -aq "E ksync lock.violation\|reap.stalled\|cpu.join_failed" "$L"; then echo "FAIL lock violation, reaper stall or failed join"; fail=1; fi

# 2. SMT siblings and several cache clusters.
if [ -e /dev/kvm ]; then
    run topo "sched." -smp 16,sockets=2,dies=2,cores=2,threads=2
    check "$RUN/topo.log" 'smp shape cpus=16 packages=2 dies=4 cores=8 clusters=4 nodes=1 smt=2 ' "2 sockets x 2 dies x 2 cores x 2 threads"
    check "$RUN/topo.log" 'sched ktest.cpus online=16 ' "SMT siblings joined"
    check "$RUN/topo.log" 'KTEST done passed=[0-9]+ failed=0 skipped=0' "sched. ktests on the SMT/cluster topology"
fi

# 3. TCG, 4 CPUs.
run tcg4 "sched.cpus,sched.wake,sched.balance,sched.reap,sched.class,sched.preempt,sched.exclusive,sc.fpu,sc.threads" -machine q35,accel=tcg -cpu max -smp 4
check "$RUN/tcg4.log" 'KTEST done passed=[0-9]+ failed=0 ' "TCG: the scheduler ktests passed on 4 CPUs"
exit $fail
