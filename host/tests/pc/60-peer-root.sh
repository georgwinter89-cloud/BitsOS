#!/bin/bash
# GP-BUS (plan/spec/gaming-pc.md §5): a second PCI root bus that no bridge leads to, as on the Gaming-PC, where bus 80
# carries the keyboard's xHCI. q35 gets a `pxb-pcie` root at bus 0x80 with one root port at 80:14.0 (no device 0 on
# the bus, like the real one) and behind it an xHCI with a keyboard.
#  Boot 1, plain: the MCFG range is cut into 00-7f and 80-ff (as on every machine: PCI scans every host bridge); the
#          xHCI is found, its BAR sized at the firmware's address, the driver bound, the keyboard behind it enumerated
#          and bound to the HID driver.
#  Boot 2, `profile=gaming-pc`: the line bitsboot writes on the real machine. The loader's choice, the kernel's CONFIG
#          and the PCI core's profile rules (never-touch list, network rule) in one boot on a real PCI tree: the same
#          cut, the same xHCI and keyboard, none of QEMU's functions on the never-touch list, and buses 00-7f listed
#          the same as in boot 1.
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
NAME="$BITSOS_LANE-peer"
ON="$RUN/serial.log"
GP="$RUN/profile.log"
machine=(-device pxb-pcie,id=pxb80,bus_nr=0x80,bus=pcie.0
         -device pcie-root-port,id=rp80,bus=pxb80,addr=0x14,chassis=80,slot=80
         -device qemu-xhci,id=x80,bus=rp80
         -device usb-kbd,bus=x80.0,port=1)
cargo xtask image --name "$NAME" --ktest --config-extra "ktest=pc.peer,pc.scan,pc.bars,pc.guard;loglevel=info"
cargo xtask qemu --name "$NAME" --timeout 120 --serial-log "$ON" -- "${machine[@]}" > "$RUN/qemu.out" 2>&1 || true
cargo xtask image --name "$NAME-gp" --ktest --config-extra "ktest=pc.peer,pc.scan,pc.bars,pc.guard;profile=gaming-pc;loglevel=info"
cargo xtask qemu --name "$NAME-gp" --timeout 120 --serial-log "$GP" -- "${machine[@]}" > "$RUN/profile.out" 2>&1 || true
touch "$ON" "$GP"
fail=0
has() { if grep -aq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3 (no '$2' in $(basename "$1"))"; fail=1; fi; }
hasnot() { if grep -aq -- "$2" "$1"; then echo "FAIL $3:"; grep -a -- "$2" "$1" | head -5; fail=1; else echo "ok   $3"; fi; }
grep -a "KTEST\|pci peer\.\|pci scan " "$ON" "$GP" || true

has "$ON" 'KTEST done passed=[0-9]* failed=0' "boot 1: no ktest failed"
has "$ON" 'KTEST ok pc.peer.second_root' "boot 1: device behind the second root found, sized, bound"
has "$ON" 'I pci peer.root bus=0x80 first_device=0x14 vendor=0x1b36' "boot 1: bus 80 found at device 14"
has "$ON" 'I pci peer.plan segment=0 window=00-ff roots="\?00-7f 80-ff"\? peers=1' "boot 1: the range is cut into 00-7f and 80-ff"
has "$ON" 'I pci function function=80:14.0 vendor=0x1b36 device=0xc .*parent=root' "boot 1: the root port on the second root"
has "$ON" 'I pci function function=81:00.0 vendor=0x1b36 device=0xd class=0xc0330 .*parent=80:14.0' "boot 1: the xHCI behind it"
has "$ON" 'I pci bound driver=xhci function=81:00.0' "boot 1: xhci bound behind the second root"
has "$ON" 'I pci peer.checked roots=2' "boot 1: the ktest saw both roots"
has "$ON" 'KTEST ok pc.peer.keyboard' "boot 1: the keyboard behind the second root is enumerated and bound"
has "$ON" 'I usb peer.checked ctrl=xhci[0-9]* function=81:00.0 keyboard=xhci' "boot 1: it hangs on the controller at 81:00.0"
hasnot "$ON" 'W pci peer\.\|W pci root\.skipped\|W pci \(cap\|bridge\|bar\|bus\|msix\|ari\|crs\)\.' "boot 1: no PCI problem line"

has "$GP" 'bitsboot profile.chosen name=gaming-pc reason=config' "boot 2: the loader chose the Gaming-PC profile"
has "$GP" 'KTEST done passed=[0-9]* failed=0' "boot 2: no ktest failed"
has "$GP" 'KTEST ok pc.peer.second_root' "boot 2: device behind the second root found, sized, bound under the profile"
has "$GP" 'I pci peer.plan segment=0 window=00-ff roots="\?00-7f 80-ff"\? peers=1' "boot 2: the same cut under the profile"
has "$GP" 'I pci peer.checked roots=2' "boot 2: the ktest saw both roots"
has "$GP" 'I usb peer.checked ctrl=xhci[0-9]* function=81:00.0 keyboard=xhci' "boot 2: the keyboard behind the second root"
hasnot "$GP" 'key=gp_' "boot 2: no QEMU function is on the never-touch list or held by the network rule"
hasnot "$GP" 'W pci peer\.\|W pci root\.skipped\|W pci \(cap\|bridge\|bar\|bus\|msix\|ari\|crs\)\.' "boot 2: no PCI problem line"

# Below bus 80 both boots list the same functions, in the same order, with the same parents.
low() { grep -a 'I pci function function=[0-7][0-9a-f]:' "$1" | sed 's/.*I pci function //'; }
low "$ON" > "$RUN/low-plain.txt"
low "$GP" > "$RUN/low-profile.txt"
if [ -s "$RUN/low-plain.txt" ] && cmp -s "$RUN/low-plain.txt" "$RUN/low-profile.txt"; then
    echo "ok   buses 00-7f are listed the same with and without the profile ($(wc -l < "$RUN/low-plain.txt") functions)"
else
    echo "FAIL buses 00-7f differ under the profile:"; diff "$RUN/low-plain.txt" "$RUN/low-profile.txt" | head -20 || true; fail=1
fi
exit $fail
