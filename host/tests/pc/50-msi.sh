#!/bin/bash
# PC-3: MSI and MSI-X on q35 with four CPUs and devices no BitsOS driver binds:
#  - QEMU's edu (MSI, one message, no mask bits; an MMIO write raises it): delivery, IRQ -> thread wake,
#    software mask, the storm guard switching MSI off and on again,
#  - an e1000e behind a root port (MSI-X with 5 entries in BAR3, and MSI): three entries spread over the CPUs,
#    each delivered on its own vector, masked -> pending bit -> delivered on unmask, affinity change, MSI-only
#    mode, a fake storm of fixed IPIs (entry masked in the hard-IRQ path), disable_device releasing the vectors,
#    restore_config after a lost MSI-X state,
#  - pci-testdev (no MSI at all): the polled fallback,
#  - virtio-rng-pci (virtio MSI-X; the gate's virtio-net belongs to NW's driver): table programming, masks,
#    affinity.
# The QEMU-only tests must pass here, not skip. The MSI trace part of V1's msi-test.mjs (MSI on, INTx off,
# deliveries) is checked from the log lines.
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
NAME="$BITSOS_LANE-msi"
LOG="$RUN/serial.log"
cargo xtask image --name "$NAME" --ktest --config-extra "ktest=pc.msi,contract.irq_vector;loglevel=info"
cargo xtask qemu --name "$NAME" --timeout 120 --smp 4 --net virtio --serial-log "$LOG" -- \
    -device pcie-root-port,id=rp1,chassis=1,slot=1 \
    -device e1000e,bus=rp1 \
    -device edu \
    -device pci-testdev \
    -device virtio-rng-pci || true
touch "$LOG"
fail=0
check() { if grep -aq -- "$1" "$LOG"; then echo "ok   $2"; else echo "FAIL $2 (no '$1')"; fail=1; fi; }
grep -a "KTEST" "$LOG" || true
check 'KTEST done passed=[0-9]* failed=0' "no pc.msi ktest failed"
for t in compose refused edu edu_storm msix e1000e_msi fake_storm disable_device restore polled virtio; do
    check "KTEST ok pc.msi.$t " "pc.msi.$t"
done
check 'KTEST ok contract.irq_vector' "vector contract test"
check 'pci msi.alloc .*driver=ktest-edu mode=msi vectors=1 .*intx=off' "edu: MSI on, INTx off"
check 'pci msi.alloc .*driver=ktest-msix mode=msix vectors=3 ' "e1000e: three MSI-X vectors"
check 'W pci msi.storm .*driver=ktest-storm .*masked=msi_off' "storm guard: MSI off for edu"
check 'W pci msi.storm .*driver=ktest-fake-storm .*masked=entry_masked' "storm guard: MSI-X entry masked"
check 'pci msi.storm_end ' "storm hold ends"
check 'pci msi.affinity ' "affinity change logged"
check 'W pci msi.restored ' "MSI-X state written back"
check 'pci msi.free ' "teardown logged"
if grep -aq 'E pci msi\.' "$LOG"; then echo "FAIL error events:"; grep -a 'E pci msi\.' "$LOG"; fail=1; fi
exit $fail
