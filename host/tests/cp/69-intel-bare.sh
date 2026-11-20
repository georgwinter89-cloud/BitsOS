#!/bin/bash
# slow
# GP-CPU: the vendor audit's proof, on this very CPU (the Gaming-PC's Core Ultra 7 265K) under KVM.
#  1. bare: `-cpu host,-hypervisor,+invtsc -overcommit cpu-pm=on -device intel-iommu`: the kernel sees a bare Intel
#     machine (the loader chooses profile gaming-pc by CPUID), MWAIT with PC10 sub-states and a DMAR table, while KVM
#     raises #GP for every MSR it does not emulate. So every read or write the audit left unguarded would end this boot.
#     ktests of the CPU, clock, tables, interrupts, idle and the measurement files; the bare view's facts (HPET left
#     out, DMAR read before the drivers, a faulted MSR read survived and named, every CPU up) are read from the log.
#     Four CPUs only: with cpu-pm=on (it is what shows MWAIT) an idle vCPU never leaves guest mode, so each one keeps
#     a host thread busy for the whole boot, and 20 of them would take every thread of this host from the scenario
#     chains the gate runs beside this one. 20 CPUs on this CPU are shown by cp/68 and by boot 3 here.
#     The ktest filter of this boot IS the Gaming-PC's remote ktest list (plan/spec/gaming-pc.md section 8): the
#     kernel sees no hypervisor bit, so the classes let through exactly the tests they will let through there.
#  2. bare-boot: the same machine, a whole boot (the Gaming-PC profile: the text console): init, the console shell, a minute of the metric
#     sampler's rounds.
#  3. host: the suite's usual view (`-cpu host`, hypervisor bit shown): the measurement and table tests on the Intel CPU
#     and this run's own cp.host.intel_view (a `Run` test needs the hypervisor bit, so it lives in this boot).
#     Its own cp.dmar.enable_refused: with a blocking IOMMU verdict no function is given bus master (B7).
# Any line that names a panic, a #GP or a #UD in any of the three logs is red.
# What this cannot show: HWP (KVM shows no HWP bit), P and E cores, the real registers' values, CPUID 0x15 (KVM hides it).
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
fail=0
check() { if grep -aEq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3 (no '$2' in $(basename "$1"))"; fail=1; fi; }
refuse() { if grep -aEq -- "$2" "$1"; then echo "FAIL $3 ('$2' in $(basename "$1"): $(grep -aE -- "$2" "$1" | head -1 | cut -c1-200))"; fail=1; else echo "ok   $3"; fi; }

model=$(awk -F': ' '/^model\t/ {print $2; exit}' /proc/cpuinfo)
family=$(awk -F': ' '/^cpu family/ {print $2; exit}' /proc/cpuinfo)
vendor=$(awk -F': ' '/^vendor_id/ {print $2; exit}' /proc/cpuinfo)
if [ "$vendor/$family/$model" != "GenuineIntel/6/198" ]; then
    echo "FAIL this run needs the Gaming-PC's CPU, the host has $vendor family $family model $model"
    exit 1
fi

image_and_boot() { # tag, timeout, config, qemu args...
    local tag="$1" timeout="$2" config="$3"
    local name="$BITSOS_LANE-ib-$tag"
    shift 3
    cargo xtask image --name "$name" $KTEST --config-extra "$config" > "$RUN/$tag-image.txt" 2>&1
    cargo xtask qemu --name "$name" --timeout "$timeout" --boot-retries 4 --serial-log "$RUN/$tag.log" -- "$@" > "$RUN/$tag-qemu.txt" 2>&1 || true
    touch "$RUN/$tag.log"
}

no_fail() { # log, what
    if grep -aqE 'KTEST (FAIL|PANIC|note)' "$1"; then echo "FAIL $2: $(grep -aE 'KTEST (FAIL|PANIC|note)' "$1" | head -3 | tr '\n' ' ' | cut -c1-600)"; fail=1; else echo "ok   $2: no ktest failed"; fi
    check "$1" 'KTEST done passed=[0-9]+ failed=0' "$2: the ktest run finished"
}

# A fault the kernel did not expect, in any wording the trap and panic paths use.
FAULT='PANIC|panic\.|Kernel panic|kernel general protection|\(#GP\)|\(#UD\)|invalid opcode|kernel double fault|cpu ap\.failed'
no_fault() { refuse "$1" "$FAULT" "$2: no panic, no #GP, no #UD, no failed AP"; }

# B7's order: once the log says DMA is blocked, no function may be enabled with bus master after it.
no_dma_after_block() { # log, what
    local blocked late
    blocked=$(grep -anE 'acpi dma\.blocked' "$1" | head -1 | cut -d: -f1)
    if [ -z "$blocked" ]; then echo "ok   $2: DMA was never blocked"; return; fi
    late=$(tail -n "+$blocked" "$1" | grep -aE 'pci enable function=.* dma=true|pci bridge\.bus_master' | head -1 | cut -c1-200)
    if [ -z "$late" ]; then echo "ok   $2: nothing got bus master after 'acpi dma.blocked'"; else echo "FAIL $2: bus master after 'acpi dma.blocked': $late"; fail=1; fi
}

# The hypervisor bit off, the invariant-TSC bit on (KVM hides it by default), MWAIT passed through, an Intel IOMMU.
BARE=(-smp 4 -cpu host,-hypervisor,+invtsc -overcommit cpu-pm=on -device intel-iommu,intremap=off)
# Without the hypervisor bit the kernel leaves the serial port alone (as on a real machine): the test key turns it on.
BARE_CONFIG="loglevel=info;ktest_serial=on"

# 1. The ktests that test what the kernel did, not how fast (timing is never judged under a hypervisor, and this view
#    hides the bit that says so). Among them, tests of the Mini-PC's own parts (class Hardware: its AMD sensors and
#    energy counter, its GPU and NVMe) would run here because the kernel believes it is on a real machine, and this
#    one has none of those parts: they are taken out by name, and run where they belong.
FILTER="cp.cpu,cp.msr,cp.trap,cp.acpi,cp.irq,cp.time.calibration,cp.smp.all_up,cp.smp.hwp,cp.smp.spec_ctrl,cp.smp.topology,cp.smp.mtrr,cp.smp.apic_map,cp.smp.ap_registers,sc.idle,so.sysk,rb.metrics"
MINIPC_ONLY="-rb.metrics.rapl,-rb.metrics.temps,-so.sysk.abi_strict,-so.sysk.cpufreq_real,-so.sysk.drm_real,-so.sysk.k10temp_real,-so.sysk.nvme_hwmon_real,-so.sysk.package_power_real"
KTEST=--ktest
image_and_boot bare 600 "$BARE_CONFIG;ktest=$FILTER,$MINIPC_ONLY" "${BARE[@]}"
L="$RUN/bare.log"
check "$L" 'kernel config values=".*profile=gaming-pc profile_reason=cpuid' "the loader took this for a bare Intel machine (safe profile)"
check "$L" 'cpu setup vendor=GenuineIntel brand="Intel\(R\) Core\(TM\) Ultra 7 265K" family=0x6 model=0xc6' "the real CPU's name reaches the kernel"
check "$L" 'KTEST ok cp.msr.try_read' "a register nobody has: read and write answer, no fault reaches the kernel"
check "$L" 'KTEST ok cp.cpu.vendor_paths' "Intel paths only"
check "$L" 'KTEST ok cp.time.calibration' "the clock without the HPET"
check "$L" 'acpi dmar width=[0-9]+ flags=0x[0-9a-f]+ opt_in=(true|false) units=1' "the DMAR table is read"
check "$L" 'acpi dmar.unit base=0xfed90000 .*translation=false .*protected_memory=false' "the IOMMU's state is read"
check "$L" 'acpi dma.state verdict=free blocked=false' "DMA is free"
check "$L" 'time hpet.untrusted reason=pc10_limit_unreadable' "the HPET is left out (the package limit MSR faulted and that was survived)"
refuse "$L" 'time hpet.started' "the HPET was not started"
check "$L" 'time clock.calibrated tsc_hz=[0-9]+ source=pm_timer .*tried="pm_timer=[0-9]+"' "the TSC was measured against the PM timer first"
check "$L" 'smp done cpus=4 planned=3 up=3 failed=0' "every CPU of the bare view up"
check "$L" 'cpu hwp.refused cpu=0 reason=no_hwp' "no HWP bit, no HWP register touched"
# The one register KVM refuses here is named, once, before any ktest ran (they fault on purpose later).
check "$L" 'cpu msr\.faults stage=smp count=1 new=0xe2 all="count=1 registers=0xe2"' "the swallowed fault is named: the package limit register, nothing else"
if [ "$(grep -acE 'cpu msr\.faults' "$L")" = 1 ]; then echo "ok   no other register faulted during the boot"; else echo "FAIL more than one 'cpu msr.faults' line: $(grep -aE 'cpu msr\.faults' "$L" | cut -c1-300 | tr '\n' ' ')"; fail=1; fi
check "$L" 'cpu spec_ctrl value=0x401 ' "IA32_SPEC_CTRL as Linux 7.3.1 sets it on this CPU (IBRS and BHI_DIS_S), on every CPU (cp.smp.spec_ctrl below)"
# The IOMMU line comes before the first function is enabled (bus master set).
dma_line=$(grep -anE 'acpi dma\.state' "$L" | head -1 | cut -d: -f1)
first_enable=$(grep -anE 'pci (enable|disable) function=|pci bind\.' "$L" | head -1 | cut -d: -f1)
if [ -n "$dma_line" ] && { [ -z "$first_enable" ] || [ "$dma_line" -lt "$first_enable" ]; }; then echo "ok   the DMA verdict is logged before the first PCI function is bound (line $dma_line, first bind ${first_enable:-none})"; else echo "FAIL the DMA verdict (line ${dma_line:-none}) does not come before the first PCI bind (line ${first_enable:-none})"; fail=1; fi
no_dma_after_block "$L" "bare ktests"
no_fail "$L" "bare ktests"
no_fault "$L" "bare ktests"
# The set the Gaming-PC's remote ktest round will run (same filter, same classes): kept beside the logs.
grep -aoE 'KTEST (ok|skip) [a-z0-9_.]+' "$L" | sort > "$RUN/gaming-pc-ktests.txt"
echo "ok   bare ktests: $(grep -c '^KTEST ok' "$RUN/gaming-pc-ktests.txt") ran, $(grep -c '^KTEST skip' "$RUN/gaming-pc-ktests.txt") belong to another run (list: gaming-pc-ktests.txt)"

# 2. A whole boot of the same machine; xtask stops QEMU at the timeout.
KTEST=
image_and_boot bare-boot 75 "$BARE_CONFIG" "${BARE[@]}"
L="$RUN/bare-boot.log"
check "$L" 'smp done cpus=4 planned=3 up=3 failed=0' "bare boot: every CPU up"
check "$L" 'ul init.start pid=1' "bare boot: init started"
check "$L" 'init: started the console shell /bin/sh' "bare boot: the console shell came up"
check "$L" 'klog metrics.start period_s=2 .*temps="not an AMD CPU" rapl="no AMD RAPL' "bare boot: the sampler runs and leaves AMD's sensors alone"
check "$L" 'acpi dma.state verdict=free blocked=false' "bare boot: DMA is free"
# The sampler's rounds went by (every 2 s): the log reaches well past the boot.
last=$(grep -aoE '^\[ *[0-9]+\.[0-9]+\]' "$L" | tail -1 | tr -d '[] ' | cut -d. -f1)
if [ "${last:-0}" -ge 30 ]; then echo "ok   bare boot: still running after ${last} s"; else echo "FAIL bare boot: the log ends at ${last:-0} s"; fail=1; fi
no_dma_after_block "$L" "bare boot"
no_fault "$L" "bare boot"

# 3. The usual view of this CPU.
KTEST=--ktest
image_and_boot host 600 "loglevel=info;ktest=cp.msr,cp.acpi,cp.host.intel_view,cp.dmar,so.sysk,rb.metrics" -smp 20 -cpu host
L="$RUN/host.log"
check "$L" 'KTEST ok cp.host.intel_view' "host view: this run's own test (HPET trusted and started, no DMAR, no TCO, energy registers shut)"
check "$L" 'KTEST ok cp.msr.try_read' "host view: the fault-safe read"
check "$L" 'KTEST ok cp.dmar.enable_refused' "host view: with an IOMMU verdict that blocks, no function is given bus master"
check "$L" 'pci enable\.refused function=[0-9a-f:.]+ driver=- dma=true reason="[^"]*translation_on' "host view: the refusal names the function and the verdict"
check "$L" 'smp done cpus=20 planned=19 up=19 failed=0' "host view: 20 CPUs up"
refuse "$L" 'cpu msr\.faults' "host view: no register faulted during the boot"
check "$L" 'KTEST ok so.sysk.k10temp' "host view: no AMD temperature files on Intel"
check "$L" 'KTEST ok so.sysk.package_power' "host view: no AMD energy files on Intel"
refuse "$L" 'acpi dmar' "host view: no DMAR table, no line about it"
no_fail "$L" "host ktests"
no_fault "$L" "host ktests"
exit $fail
