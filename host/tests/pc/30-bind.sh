#!/bin/bash
# PC-4: the driver registry on q35 with bridges: one driver per function in the defined order (exact ID
# before class), probe/remove/re-probe with the record freed, a rescan through four bridge levels that keeps
# every node and sizes nothing, the /sys/bus/pci view with nested bridge directories, R8a on the raw write
# paths and the untouched functions' command registers unchanged after boot. QEMU-only tests must pass here.
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
NAME="$BITSOS_LANE-bind"
LOG="$RUN/serial.log"
cargo xtask image --name "$NAME" --ktest --config-extra "ktest=pc.bind,pc.rescan,pc.sysfs,pc.optin,pc.storage,pc.scan.tree,contract.pci;loglevel=info"
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
grep -a "KTEST" "$LOG" || true
check 'KTEST done passed=[0-9]* failed=0' "no ktest failed"
for t in pc.bind.order pc.bind.remove_reprobe pc.bind.busy pc.bind.gone_during_probe pc.bind.one_per_function pc.rescan.keeps_bound pc.sysfs.view \
         pc.optin.command_unchanged pc.storage.raw_write_refused contract.pci_registry_optin; do
    check "KTEST ok $t" "$t"
done
check 'I pci bound driver=virtio-net function=00:[0-9a-f.:]* matched=id' "virtio-net bound by ID"
check 'I pci bound driver=ktest-narrow .* matched=class failed_before=1' "failed probe hands over to the next driver"
check 'I pci removed function=00:1f.0 driver=ktest-enable gone=false' "remove frees the binding"
check 'I pci rescan added=0 gone=0 held=0 kept=[0-9]* sized=0 bound=0' "rescan through the bridges changes nothing"
check 'tree.line text=".*driver virtio-net' "hw_info tree names the driver"
check 'W pci write.refused function=00:1f.2 offset=0x0 reason=storage' "raw write to AHCI refused (R8a)"
if grep -aq 'E pci driver.duplicate\|W pci bind.none\|W pci function.gone\|W pci crs.timeout\|W pci rescan.uncertain' "$LOG"; then
    echo "FAIL unexpected registry problems:"; grep -a 'driver.duplicate\|bind.none\|function.gone\|crs.timeout\|rescan.uncertain' "$LOG"; fail=1
fi
exit $fail
