#!/bin/bash
# PC-2: enable_device / disable_device on q35 with devices behind bridges that no driver uses:
#  - a PCIe root port with an e1000e card (PM, MSI, MSI-X, BARs; no BitsOS driver binds it, while igc
#    binds the igb since NW-2),
#  - root port -> pcie-pci-bridge -> pci-bridge -> pci-bridge -> pci-testdev (four bridges deep).
# OVMF leaves bus master on and QEMU does not model it, so the ktests clear it on every bridge first and
# check that enable_device sets it on the whole path and logs one bridge.bus_master line per bridge
# (spec/usb.md §8). The QEMU-only tests must pass here, not skip.
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
NAME="$BITSOS_LANE-enable"
LOG="$RUN/serial.log"
cargo xtask image --name "$NAME" --ktest --config-extra "ktest=pc.enable,contract.pci;loglevel=info"
cargo xtask qemu --name "$NAME" --timeout 120 --net virtio --serial-log "$LOG" -- \
    -device pcie-root-port,id=rp1,chassis=1,slot=1 \
    -device e1000e,bus=rp1 \
    -device pcie-root-port,id=rp2,chassis=2,slot=2 \
    -device pcie-pci-bridge,id=pb1,bus=rp2 \
    -device pci-bridge,id=b1,bus=pb1,chassis_nr=5,addr=1 \
    -device pci-bridge,id=b2,bus=b1,chassis_nr=6,addr=1 \
    -device pci-testdev,bus=b2,addr=2,membar=1G || true
touch "$LOG"
fail=0
check() { if grep -aq -- "$1" "$LOG"; then echo "ok   $2"; else echo "FAIL $2 (no '$1')"; fail=1; fi; }
count() { grep -ac -- "$1" "$LOG" || true; }
grep -a "KTEST" "$LOG" || true
check 'KTEST done passed=[0-9]* failed=0' "no ktest failed"
check 'KTEST ok pc.enable.bridges' "bus master set on every bridge up to the root port, logged, disable leaves it off"
check 'KTEST ok pc.enable.msi_stale' "stale MSI/MSI-X enables cleared"
check 'KTEST ok pc.enable.restore' "header written back after a simulated retrain"
check 'KTEST ok pc.enable.refused' "untouched and mass storage refused"
check 'KTEST ok pc.enable.dma_path' "functions enabled for DMA have bus master on their path"
check 'KTEST ok contract.pci_registry_optin' "PCI contract test"
if grep -aq 'KTEST ok pc.enable.wake' "$LOG"; then
    echo "ok   D3hot -> D0 through PMCSR (pm.wake)"
else
    echo "FAIL pc.enable.wake did not pass:"; grep -a 'pc.enable.wake\|ktest.wake_skipped' "$LOG" || true; fail=1
fi
# The e1000e's root port and the four bridges above the pci-testdev: one line each, naming bridge and device.
n=$(count 'I pci bridge.bus_master bridge=')
if [ "$n" -ge 5 ]; then echo "ok   $n bridge.bus_master lines"; else echo "FAIL only $n bridge.bus_master lines (want >= 5)"; fail=1; fi
check 'pci bridge.bus_master bridge=[0-9a-f:.]* device=[0-9a-f:.]* command_old=' "bridge line names bridge and device"
check 'pci ktest.bridges function=.* bridges=4' "pci-testdev four bridges deep"
check 'pci ktest.bridges function=.* bridges=1' "e1000e below one root port"
check 'pci pm.wake function=.* from=D3' "pm.wake line"
check 'pci msi.stale_cleared function=.* kind=msix' "msi.stale_cleared line"
check 'pci config.restored function=.* offset=0x10' "config.restored line for BAR0"
check 'W pci enable.refused function=00:1f.2' "AHCI refused (R8a)"
if grep -aq 'E pci enable.failed\|W pci bar.outside_window\|W pci pm.retry' "$LOG"; then
    echo "FAIL unexpected enable problems:"; grep -a 'pci enable.failed\|bar.outside_window\|pm.retry' "$LOG"; fail=1
fi
exit $fail
