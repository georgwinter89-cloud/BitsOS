#!/bin/bash
# slow
# CP-4: SMP bring-up in QEMU (functional only; timing is judged on the Mini-PC).
#  1. -smp 16 under KVM (TCG when /dev/kvm is missing): every AP up, each with its times, topology and 9 TSC
#     rounds; all cp.smp/cp.ist/cp.cpu/contract.percpu ktests pass with 16 CPUs.
#  2. -smp 16 under TCG (-machine q35,accel=tcg -cpu max): QEMU's TCG clears an AP's MTRRs on INIT, so the
#     MTRR-copy path must run at bring-up (`mtrr=copied`), and the APs still pass every check.
#  3. a multi-level topology via -smp sockets/dies/cores/threads: `smp shape` must match the command line
#     (QEMU has no hybrid core types; the 0x1A decoding is host-tested).
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
    # More boot retries than the default: OVMF's usb-storage stall hits this machine under load (CONTRACTS §1).
    cargo xtask qemu --name "$name" --timeout 600 --boot-retries 4 --serial-log "$RUN/$tag.log" -- "$@" > "$RUN/$tag-qemu.txt" 2>&1 || true
    touch "$RUN/$tag.log"
}

# 1. 16 CPUs, the default accelerator.
run smp16 "cp.smp,cp.ist,cp.cpu,cp.percpu,contract.percpu,cp.irq" -smp 16
L="$RUN/smp16.log"
check "$L" 'smp done cpus=16 planned=15 up=15 failed=0' "16 CPUs up (15 APs, none failed)"
n=$(count "$L" 'cpu ap.up cpu=[0-9]+ ')
[ "$n" = 15 ] && echo "ok   15 'cpu ap.up' events" || { echo "FAIL $n 'cpu ap.up' events, wanted 15"; fail=1; }
n=$(count "$L" 'cpu ap.tsc cpu=[0-9]+ offset_ticks=-?[0-9]+ .*rounds=9')
[ "$n" = 15 ] && echo "ok   15 TSC offsets from 9 rounds" || { echo "FAIL $n TSC offsets with 9 rounds, wanted 15"; fail=1; }
n=$(count "$L" 'cpu topology cpu=[0-9]+ ')
[ "$n" = 16 ] && echo "ok   16 topology events" || { echo "FAIL $n topology events, wanted 16"; fail=1; }
check "$L" 'KTEST ok cp.smp.ipi_every_cpu' "a fixed IPI reached every CPU"
check "$L" 'KTEST ok cp.smp.call_every_cpu' "a cross-CPU call ran on every CPU"
check "$L" 'KTEST ok cp.ist.double_fault' "the #DF self-test runs with 16 CPUs up (private IDT)"
check "$L" 'KTEST ok cp.ist.nmi' "the soft-NMI IST test with NMI IPIs paused"
check "$L" 'KTEST ok cp.smp.call_owners' "two callers on one target: own results, Late frees the mailbox"
check "$L" 'KTEST ok cp.smp.start_guards' "start_ap refuses running CPUs' APIC IDs"
check "$L" 'KTEST skip cp.smp.tsc_offsets_hardware' "the 38-tick check skips under a hypervisor"
check "$L" 'KTEST done passed=[0-9]+ failed=0 skipped=3' "every ktest passed; skipped only the hardware TSC check and the two CONFIG-limit tests (55-smp-limits.sh)"

# 2. TCG: the MTRR-copy path at bring-up.
run tcg16 "cp.smp" -machine q35,accel=tcg -cpu max -smp 16
L="$RUN/tcg16.log"
check "$L" 'smp done cpus=16 planned=15 up=15 failed=0' "TCG: 16 CPUs up"
n=$(count "$L" 'cpu ap.up .* mtrr=copied ')
[ "$n" = 15 ] && echo "ok   TCG: all 15 APs took the BSP's MTRRs (QEMU clears them on INIT)" || { echo "FAIL TCG: $n APs with mtrr=copied, wanted 15"; fail=1; }
check "$L" 'KTEST ok cp.smp.call_owners' "TCG: two callers on one target"
check "$L" 'KTEST done passed=[0-9]+ failed=0 skipped=3' "TCG: every cp.smp ktest passed (the hardware TSC check and the CONFIG-limit tests skip)"

# 3. Topology from the command line.
if [ -e /dev/kvm ]; then
    run topo "cp.smp.topology,cp.smp.all_up,cp.smp.apic_map" -smp 16,sockets=2,dies=2,cores=2,threads=2
    check "$RUN/topo.log" 'smp shape cpus=16 packages=2 dies=4 cores=8 clusters=4 nodes=1 smt=2 ' "2 sockets x 2 dies x 2 cores x 2 threads (leaf 0x1F)"
    check "$RUN/topo.log" 'cpu topology cpu=15 apic_id=15 package=1 die=3 core=3 thread=1 threads_per_core=2 sibling_of=14 ' "the last CPU decoded"
else
    run topo "cp.smp.topology,cp.smp.all_up,cp.smp.apic_map" -smp 16,sockets=2,cores=4,threads=2
    check "$RUN/topo.log" 'smp shape cpus=16 packages=2 .*cores=8 .*smt=2 ' "2 sockets x 4 cores x 2 threads"
fi
check "$RUN/topo.log" 'KTEST done passed=[0-9]+ failed=0 skipped=0' "topology ktests passed"
exit $fail
