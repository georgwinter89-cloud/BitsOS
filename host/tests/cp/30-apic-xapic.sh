#!/bin/bash
# CP-3: the xAPIC fallback. QEMU's CPU without x2APIC (-cpu host,-x2apic); every cp.irq ktest must pass on the
# MMIO path (self-IPIs through ICR high/low with the delivery-status poll, EOI through the page).
# Without the kernel hooks (desk change requests CP-2/1 and CP-3/1) the ktests set the LAPIC up themselves and
# the I/O APIC ones skip; with them nothing may skip.
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
NAME="$BITSOS_LANE-xapic"
if [ -e /dev/kvm ]; then CPU="host,-x2apic"; else CPU="max,-x2apic"; fi
hooks=no
if grep -q 'acpi::init(&bi)' kernel/src/main.rs && grep -q 'acpi::irq::init(' kernel/src/main.rs; then hooks=yes; fi
cargo xtask image --name "$NAME" --ktest --config-extra "ktest=cp.irq,contract.irq"
cargo xtask qemu --name "$NAME" --timeout 120 --serial-log "$RUN/serial.log" -- -cpu "$CPU" || true
log="$RUN/serial.log"
touch "$log"
fail=0
check() { if grep -aEq -- "$1" "$log"; then echo "ok   $2"; else echo "FAIL $2 (no '$1')"; fail=1; fi; }
check 'apic setup mode=xapic firmware_mode=xapic cpuid_x2apic=false' "LAPIC in xAPIC mode, CPUID without x2APIC"
check 'apic fallback.xapic reason=' "the fallback is an event"
check 'apic setup .* svr=0x1ff ' "spurious vector 0xFF, enabled"
check 'KTEST ok cp.irq.self_ipi' "self-IPI through the xAPIC ICR"
check 'KTEST ok cp.irq.ipi_physical' "physical-destination IPI (ICR high = id << 24)"
check 'KTEST ok cp.irq.spurious_vector' "spurious path without EOI"
if [ "$hooks" = yes ]; then
    check 'KTEST done passed=[0-9]+ failed=0 skipped=0' "all cp.irq ktests pass, none skipped (hooks in)"
else
    echo "note: kernel hooks missing, I/O APIC ktests skip"
    check 'KTEST done passed=[0-9]+ failed=0 ' "cp.irq ktests: no failure"
fi
exit $fail
