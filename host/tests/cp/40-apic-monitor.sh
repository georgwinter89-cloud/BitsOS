#!/bin/bash
# CP-3: V1's apic-test.mjs monitor idea. A normal boot (no ktest) with a QMP socket; once the kernel says
# `irq enabled` and `kernel ready`, QEMU's HMP `info lapic` and `info pic` are compared with the MADT and the
# kernel's `irq route` events (apic_check.py): every routed pin (SCI, PS/2) with the MADT's trigger/polarity,
# its vector and the BSP as destination, masked until an owner installs a handler; every other pin masked,
# SVR 0x1FF, LINT0 masked, NMI on LINT1, LVT timer masked, both 8259s masked at 0x20/0x28. Needs the kernel hooks (desk change requests CP-2/1, CP-3/1).
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
NAME="$BITSOS_LANE-apicmon"
if ! grep -q 'acpi::init(&bi)' kernel/src/main.rs || ! grep -q 'acpi::irq::init(' kernel/src/main.rs; then
    echo "SKIP: kernel/src/main.rs does not call acpi::init(&bi) and acpi::irq::init yet (desk change requests CP-2/1, CP-3/1)"
    exit 0
fi
log="$RUN/serial.log"
sock="$RUN/qmp.sock"
cargo xtask image --name "$NAME"
cargo xtask qemu --name "$NAME" --timeout 120 --serial-log "$log" -- -qmp "unix:$sock,server,nowait" > "$RUN/qemu.out" 2>&1 &
qpid=$!
up=no
for _ in $(seq 1 100); do
    if grep -aq 'irq enabled' "$log" 2>/dev/null && grep -aq 'kernel ready' "$log" 2>/dev/null && [ -S "$sock" ]; then up=yes; break; fi
    sleep 1
done
if [ "$up" = yes ]; then
    python3 -I host/tests/cp/qmp_hmp.py "$sock" 'info lapic' 'info pic' --quit > "$RUN/monitor.txt" 2>&1 || true
fi
# xtask ends with QEMU (or at its own timeout); our own PID only.
for _ in $(seq 1 30); do kill -0 "$qpid" 2>/dev/null || break; sleep 1; done
if kill -0 "$qpid" 2>/dev/null; then kill "$qpid" 2>/dev/null || true; fi
wait "$qpid" 2>/dev/null || true
if [ "$up" != yes ]; then
    echo "FAIL the kernel did not reach 'irq enabled' + 'kernel ready' within 100 s"
    tail -20 "$log" 2>/dev/null || true
    exit 1
fi
cat "$RUN/monitor.txt"
python3 -I host/tests/cp/apic_check.py "$log" "$RUN/monitor.txt"
