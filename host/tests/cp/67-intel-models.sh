#!/bin/bash
# slow
# CP-6: the Intel paths under a named Intel CPU model (the default -cpu host covers the Gaming-PC's own 265K).
#  1. -cpu Skylake-Client-v4 -smp 4: GenuineIntel without the hybrid bit: TSC-deadline on every CPU, no AMD MSR
#     path, no Intel RAPL in a guest (cp.cpu.vendor_paths), uniform cores at full capacity.
#  2. -cpu Skylake-Client-v4,-tsc-deadline -smp 4: the same CPU without TSC-deadline takes the one-shot timer.
# Hybrid P/E cores cannot be shown here (QEMU hides CPUID 7 EDX[15]); host tests and the Gaming-PC check them.
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
fail=0
check() { if grep -aEq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3 (no '$2' in $(basename "$1"))"; fail=1; fi; }

run() { # tag, ktest filter, qemu args...
    local tag="$1" filter="$2"
    local name="$BITSOS_LANE-$tag"
    shift 2
    cargo xtask image --name "$name" --ktest --config-extra "loglevel=info;ktest=$filter" > "$RUN/$tag-image.txt" 2>&1
    cargo xtask qemu --name "$name" --timeout 600 --boot-retries 4 --serial-log "$RUN/$tag.log" -- "$@" > "$RUN/$tag-qemu.txt" 2>&1 || true
    touch "$RUN/$tag.log"
}

no_fail() { # log, what
    if grep -aqE 'KTEST (FAIL|PANIC)' "$1"; then echo "FAIL $2: $(grep -aE 'KTEST (FAIL|PANIC)' "$1" | head -3 | tr '\n' ' ')"; fail=1; else echo "ok   $2: no ktest failed"; fi
    check "$1" 'KTEST done passed=[0-9]+ failed=0' "$2: the ktest run finished"
}

FILTER="cp.cpu,cp.msr,cp.smp.topology,cp.time.lapic_every_cpu"

# 1. A named Intel model with TSC-deadline.
run skylake "$FILTER" -smp 4 -cpu Skylake-Client-v4
L="$RUN/skylake.log"
check "$L" 'time timers.ready cpus=4 live=4 mode=tsc_deadline interrupt_driven=true' "Skylake: TSC-deadline on all 4 CPUs"
check "$L" 'KTEST ok cp.cpu.vendor_paths' "Skylake: Intel paths only (no SYSCFG, no RAPL in a guest)"
check "$L" 'smp shape cpus=4 .*p_cores=0 e_cores=0 min_capacity=1024 max_perf=0' "Skylake: uniform cores, full capacity"
no_fail "$L" "Skylake"

# 2. The same model without TSC-deadline.
run skylake-oneshot "$FILTER" -smp 4 -cpu Skylake-Client-v4,-tsc-deadline
L="$RUN/skylake-oneshot.log"
check "$L" 'time timers.ready cpus=4 live=4 mode=oneshot interrupt_driven=true' "Skylake -tsc-deadline: one-shot on all 4 CPUs"
check "$L" 'KTEST ok cp.cpu.vendor_paths' "Skylake -tsc-deadline: timer kind matches CPUID"
no_fail "$L" "Skylake -tsc-deadline"
exit $fail
