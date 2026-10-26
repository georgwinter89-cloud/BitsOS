#!/bin/bash
# PC-1: PCI enumeration on q35 with extra topology the plain gate does not have:
#  - a PCIe root port with an igb card (BARs, MSI-X, AER in the extended space),
#  - a multi-function pair of root ports at 00:06.0/.1,
#  - a chain root port -> pcie-pci-bridge -> pci-bridge -> pci-bridge -> pci-testdev whose BAR2 is a 1 GiB
#    64-bit prefetchable BAR (OVMF puts it above 4 GiB).
# Runs every pc.* ktest plus the PCI contract test on that machine; the topology tests must pass, not skip.
# With the kernel hook `acpi::init(&bi)` (desk change request CP-2/1) the walk uses ECAM and the
# ECAM-only checks run too; without it PCI uses 0xCF8 and the script says so.
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
NAME="$BITSOS_LANE-topology"
LOG="$RUN/serial.log"
cargo xtask image --name "$NAME" --ktest --config-extra "ktest=pc,contract.pci;loglevel=info"
cargo xtask qemu --name "$NAME" --timeout 120 --net virtio --serial-log "$LOG" -- \
    -device pcie-root-port,id=rp1,chassis=1,slot=1 \
    -device igb,bus=rp1 \
    -device pcie-root-port,id=rp3,chassis=3,slot=3,bus=pcie.0,addr=0x6.0,multifunction=on \
    -device pcie-root-port,id=rp4,chassis=4,slot=4,bus=pcie.0,addr=0x6.1 \
    -device pcie-root-port,id=rp2,chassis=2,slot=2 \
    -device pcie-pci-bridge,id=pb1,bus=rp2 \
    -device pci-bridge,id=b1,bus=pb1,chassis_nr=5,addr=1 \
    -device pci-bridge,id=b2,bus=b1,chassis_nr=6,addr=1 \
    -device pci-testdev,bus=b2,addr=2,membar=1G || true
touch "$LOG"
fail=0
check() { if grep -aq -- "$1" "$LOG"; then echo "ok   $2"; else echo "FAIL $2 (no '$1')"; fail=1; fi; }
grep -a "KTEST" "$LOG" || true
check 'KTEST done passed=[0-9]* failed=0' "no pc.* ktest failed"
check 'KTEST ok pc.topo.root_port' "igb below a root port: tree, BARs, MSI-X"
check 'KTEST ok pc.topo.bridge_chain' "bridge chain, 1 GiB 64-bit BAR above 4 GiB"
check 'KTEST ok pc.topo.multifunction' "multi-function device"
check 'KTEST ok pc.bars.restored' "BARs restored after sizing"
check 'KTEST ok contract.pci_registry_optin' "PCI contract test"
check 'pci function function=00:06.1 vendor=0x1b36 device=0xc' "second function of the root port pair"
check 'pci function function=.* vendor=0x8086 device=0x10c9 class=0x20000' "igb listed"
check 'pci function function=.* vendor=0x1b36 device=0x5 ' "pci-testdev listed"
if grep -aq 'pci scan .*method=ecam' "$LOG"; then
    echo "ok   ECAM in use"
    check 'KTEST ok pc.scan.ecam' "every PCIe function reaches 4 KiB, conventional ones 256 bytes"
else
    echo "note: PCI ran on 0xCF8 (kernel/src/main.rs without acpi::init, desk change request CP-2/1)"
fi
if grep -aq 'W pci \(cap\|bridge\|bar\|bus\|msix\|ari\|crs\|root\)\.' "$LOG"; then echo "FAIL problems on QEMU's tree:"; grep -a 'W pci ' "$LOG"; fail=1; fi
exit $fail
