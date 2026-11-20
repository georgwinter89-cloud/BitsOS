#!/bin/bash
# slow
# CP-265K: BitsOS on this very CPU (the Gaming-PC's Core Ultra 7 265K, family 6 model 0xC6) under KVM, -cpu host -smp 20.
# What the guest sees of the CPU is real, what it does not is not: KVM hides the hybrid bit, leaf 6 and the HWP MSRs, so
# the P/E paths are proven by host tests on the real CPUID dumps and on the one day Georg boots his stick here. No clock
# or timing measured here says anything about the bare-metal path. The boots:
#  1. ktests with this run's own cp.host.kvm_265k: 20 CPUs, the Intel paths, HWP refused (hypervisor), one feature set.
#  2. a whole boot, no ktest: init and the console shell come up on all 20 CPUs (no render node: the text console).
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
fail=0
check() { if grep -aEq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3 (no '$2' in $(basename "$1"))"; fail=1; fi; }
refuse() { if grep -aEq -- "$2" "$1"; then echo "FAIL $3 ('$2' in $(basename "$1"): $(grep -aE -- "$2" "$1" | head -1 | cut -c1-200))"; fail=1; else echo "ok   $3"; fi; }

# This scenario has no other machine: on another CPU it cannot do its work, and that is red.
model=$(awk -F': ' '/^model\t/ {print $2; exit}' /proc/cpuinfo)
family=$(awk -F': ' '/^cpu family/ {print $2; exit}' /proc/cpuinfo)
vendor=$(awk -F': ' '/^vendor_id/ {print $2; exit}' /proc/cpuinfo)
if [ "$vendor/$family/$model" != "GenuineIntel/6/198" ]; then
    echo "FAIL this run needs the Gaming-PC's CPU, the host has $vendor family $family model $model"
    exit 1
fi

image_and_boot() { # tag, timeout, config, qemu args...
    local tag="$1" timeout="$2" config="$3"
    local name="$BITSOS_LANE-$tag"
    shift 3
    cargo xtask image --name "$name" $KTEST --config-extra "$config" > "$RUN/$tag-image.txt" 2>&1
    cargo xtask qemu --name "$name" --timeout "$timeout" --boot-retries 4 --serial-log "$RUN/$tag.log" -- "$@" > "$RUN/$tag-qemu.txt" 2>&1 || true
    touch "$RUN/$tag.log"
}

no_fail() { # log, what
    if grep -aqE 'KTEST (FAIL|PANIC|note)' "$1"; then echo "FAIL $2: $(grep -aE 'KTEST (FAIL|PANIC|note)' "$1" | head -3 | tr '\n' ' ')"; fail=1; else echo "ok   $2: no ktest failed"; fi
    check "$1" 'KTEST done passed=[0-9]+ failed=0' "$2: the ktest run finished"
}

MACHINE=(-smp 20 -cpu host)
KTEST=--ktest

# 1. The ktests of the CPU paths on the host's CPU.
FILTER="cp.smp,cp.host,cp.cpu,cp.msr,cp.entry,cp.time,sc.idle"
image_and_boot ktests 600 "loglevel=info;ktest=$FILTER" "${MACHINE[@]}"
L="$RUN/ktests.log"
check "$L" 'KTEST ok cp.host.kvm_265k' "this run's own test: the 265K under KVM, 20 CPUs, HWP refused"
check "$L" 'KTEST ok cp.smp.hwp' "HWP on every CPU as CPUID says"
check "$L" 'KTEST ok cp.smp.spec_ctrl' "enhanced IBRS on every CPU"
check "$L" 'cpu setup vendor=GenuineIntel brand="Intel\(R\) Core\(TM\) Ultra 7 265K" family=0x6 model=0xc6 stepping=2' "the real CPU's name reaches the kernel"
check "$L" 'cpu hwp.refused cpu=0 reason=hypervisor' "HWP refused under a hypervisor, before any MSR"
refuse "$L" 'cpu hwp.(on|failed)' "no CPU ran the HWP sequence"
check "$L" 'smp done cpus=20 planned=19 up=19 failed=0' "20 CPUs up"
check "$L" 'smp shape cpus=20 .*min_capacity=1024 max_perf=0 hwp=none' "uniform in the guest: full capacity, no HWP number"
check "$L" 'time timers.ready cpus=20 live=20 mode=tsc_deadline interrupt_driven=true' "TSC-deadline on all 20 CPUs"
check "$L" 'sched idle.c1_only reason=hypervisor' "idle: C1 by hlt"
refuse "$L" 'cpu ap.differs' "no feature difference between the vCPUs"
check "$L" 'cpu flags count=[0-9]+ on=".*hwp_epp|cpu flags count=[0-9]+ on=".*spec_ctrl' "the new flags are in the table"
no_fail "$L" "ktests"

# 2. A whole boot, no ktest: xtask stops QEMU at the timeout, the log says how far it came.
KTEST=
image_and_boot boot 45 "loglevel=info" "${MACHINE[@]}"
L="$RUN/boot.log"
check "$L" 'console desktop.no_gpu reason=gpu' "no GPU in QEMU: the text console stays"
check "$L" 'smp done cpus=20 planned=19 up=19 failed=0' "20 CPUs up"
check "$L" 'ul init.start pid=1' "init started"
check "$L" 'init: started the console shell /bin/sh' "the console shell came up"
refuse "$L" 'PANIC|panic\.|Kernel panic|cpu ap.failed' "no panic, no failed AP"
exit $fail
