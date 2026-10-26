#!/bin/bash
# PC-5: the raw MCP tools and the L1.2 helper on q35 with:
#  - a pci-testdev on bus 0 (configuration writes at widths 1 and 4, its MMIO test protocol through
#    mmio_write/mmio_read: one write at the offset its header names counts one hit; its BAR register and bus
#    master refused, every BAR access refused while its memory decoding is off),
#  - a PCIe root port with a slot and an e1000e behind it (Slot Control lost and restored, the saved copy
#    names Slot/Root Control and the "2" registers),
# plus the plain machine's AHCI (class 01: every raw write and mapping refused) and the q35 MCH/SMBus with
# test keys (untouched: header reads only, every write refused). The QEMU-only tests must pass here.
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
NAME="$BITSOS_LANE-raw"
LOG="$RUN/serial.log"
cargo xtask image --name "$NAME" --ktest --config-extra "ktest=pc.tool,pc.pcie,pc.aspm,pc.raw,pc.optin,kreg.mcp;loglevel=info"
cargo xtask qemu --name "$NAME" --timeout 120 --net virtio --serial-log "$LOG" -- \
    -device pcie-root-port,id=rp1,chassis=1,slot=1 \
    -device e1000e,bus=rp1 \
    -device pci-testdev || true
touch "$LOG"
fail=0
check() { if grep -aq -- "$1" "$LOG"; then echo "ok   $2"; else echo "FAIL $2 (no '$1')"; fail=1; fi; }
grep -a "KTEST" "$LOG" || true
check 'KTEST done passed=[0-9]* failed=0' "no ktest failed"
for t in pc.tool.registry pc.tool.read_host_bridge pc.tool.storage_refused pc.tool.untouched_refused pc.tool.hostile_args \
         pc.tool.rescan pc.tool.testdev pc.tool.core_regs_refused pc.aspm.fake pc.aspm.kernel_path pc.pcie.saved_regs pc.pcie.restore_ctl2 pc.raw.guards kreg.mcp_tools_unique; do
    check "KTEST ok $t" "$t"
done
check 'W pci raw.refused tool=pci_write function=00:1f.2 reason=storage' "AHCI config write refused (R8a)"
check 'W pci raw.refused tool=mmio_read function=00:1f.2 reason=storage' "AHCI BAR read refused (R8a)"
check 'W pci raw.refused tool=pci_write function=00:00.0 reason=untouched' "untouched MCH write refused (R8d)"
check 'I pci raw.cfg_write function=00:0[0-9a-f].0 driver=- offset=0x3c width=1' "pci-testdev interrupt line written"
check 'I pci raw.mmio_write function=00:0[0-9a-f].0 bar=0' "pci-testdev MMIO written"
check 'I pci ktest.restore_ctl function=00:0[0-9a-f].0 register=dev_ctl2' "a Device Control 2 restored"
check 'I pci ktest.restore_ctl function=00:0[0-9a-f].0 register=slot_ctl' "root port Slot Control restored"
check 'W pci raw.refused tool=pci_write function=00:0[0-9a-f].0 reason=addresses' "BAR write refused"
check 'W pci raw.refused tool=pci_write function=00:0[0-9a-f].0 reason=interrupts' "MSI/MSI-X control write refused"
check 'W pci raw.refused tool=pci_write function=00:0[0-9a-f].0 reason=bus_master' "bus master on by a raw write refused"
check 'W pci raw.refused tool=mmio_read function=00:0[0-9a-f].0 reason=decode_off' "BAR read with memory decoding off refused"
check 'W pci raw.refused tool=mmio_write function=00:0[0-9a-f].0 reason=decode_off' "BAR write with memory decoding off refused"
check 'I pci aspm.l1_2_unsupported function=0[0-9a-f]:00.0' "e1000e's link: no L1SS on QEMU's root port"
check 'W pci aspm.refused function=00:1f.2 reason=' "ASPM helper refuses the AHCI"
if grep -aq 'E pci aspm\.\|E pci raw\.' "$LOG"; then
    echo "FAIL unexpected errors:"; grep -a 'E pci aspm\.\|E pci raw\.' "$LOG"; fail=1
fi
exit $fail
