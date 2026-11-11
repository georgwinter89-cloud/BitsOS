#!/bin/bash
# GX-7n: the IH ring's vector path with a real MSI: QEMU's edu device (one message, no mask bits, an MMIO write raises it)
# wakes the real amdgpu-ih thread through the same line the chip uses; the ring is plain memory, filled by the test. Also
# a storm of raises past the PCI core's limit: the line turns to looks, nothing gets lost. Two rounds: the vector comes back.
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
mkdir -p "$RUN"
NAME="$BITSOS_LANE-ih"
LOG="$RUN/serial.log"
cargo xtask image --name "$NAME" --ktest --config-extra "ktest=gx.ih.;loglevel=info"
cargo xtask qemu --name "$NAME" --timeout 180 --smp 4 --net virtio --serial-log "$LOG" -- -device edu || true
touch "$LOG"
fail=0
check() { if grep -aq -- "$1" "$LOG"; then echo "ok   $2"; else echo "FAIL $2 (no '$1')"; fail=1; fi; }
grep -a "KTEST" "$LOG" | grep -av "KTEST ok" || true
check 'KTEST done passed=[0-9]* failed=0' "no gx.ih ktest failed"
for t in format watch reader overflow flood chip_side rearm_masked jobs lost fault hang cycles load edu_msi; do
    check "KTEST ok gx.ih.$t " "gx.ih.$t"
done
check 'pci msi.alloc .*driver=amdgpu mode=msi vectors=1 .*intx=off' "edu: MSI on, INTx off, under the driver's name"
check 'W pci msi.storm .*driver=amdgpu .*masked=msi_off' "the storm guard took the line"
check 'pci msi.free .*driver=amdgpu ' "the vector was given back"
if grep -aq 'E gx ih\.' "$LOG"; then echo "FAIL error events:"; grep -a 'E gx ih\.' "$LOG"; fail=1; fi
exit $fail
