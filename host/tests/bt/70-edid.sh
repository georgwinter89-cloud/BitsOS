#!/bin/bash
# slow
# BT-edid (SN-5 CR3): bitsboot reads the monitor's EDID from the GOP handle (EFI_EDID_ACTIVE_PROTOCOL, else
# EFI_EDID_DISCOVERED_PROTOCOL) and hands it on as BootInfo record EDID (tag 21). QEMU's OVMF installs neither
# protocol (QemuVideoDxe, also with VGA edid=on), so the found paths run through the hypervisor-only test hooks
# bitsboot.test=edid_active / edid_fallback (a generic 2560x1440 test monitor, bitsboot_pure::edid::fixture).
# The kernel proves arrival: gopfb takes the refresh from the EDID; the kernel glue (main.rs after pci::bind_drivers) has hda name the sink.
. "$(dirname "$0")/lib.bash"

# 1. No EDID from the firmware (std VGA with its EDID on): no record, the kernel keeps its estimate.
make_image bt-edid-none
boot bt-edid-none e-none 60 --net none -- -vga none -device VGA,xres=2560,yres=1440,vgamem_mb=64,edid=on
want e-none "bitsboot edid.none active=absent discovered=absent" "no EDID protocol: logged as none"
want_re e-none "MODE_LIST\([0-9]+ B\) RSDP" "no EDID record"
refuse e-none "source=edid" "nothing in the kernel claims an EDID"
want_kernel_ok e-none

# 2. Active answers: its bytes go on whole, the kernel's refresh comes from the EDID's detailed timing.
make_image bt-edid-act "bitsboot.test=edid_active;loglevel=info"
boot bt-edid-act e-active 60 --net none --res 2560x1440
want e-active "bitsboot edid.found source=active bytes=256 blocks=2 manufacturer=GEN product=0x2560 checksum=ok" "Active EDID found"
want_re e-active "MODE_LIST\([0-9]+ B\) EDID\(256 B\) RSDP" "the EDID record (256 B) follows the mode list"
want e-active "gopfb vblank.refresh refresh_mhz=59951 period_ns=16680414 source=edid clock_hz=241500000 htotal=2720 vtotal=1481" \
    "the kernel's refresh comes from the handed-on EDID"
want e-active 'dpaudio.sink source=edid manufacturer=0xae1c product=0x2560 name="BitsOS test" sads=1' "hda's DP audio sink comes from the EDID"
want_kernel_ok e-active

# 3. Active is broken (bad header): Discovered is taken, with a warning naming why Active was passed over.
make_image bt-edid-fb "bitsboot.test=edid_fallback"
boot bt-edid-fb e-fallback 60 --net none --res 2560x1440
want e-fallback "warning: bitsboot edid.found source=discovered bytes=256 blocks=2 manufacturer=GEN product=0x2560 checksum=ok active=bad_header" \
    "a broken Active EDID falls back to Discovered"
want_re e-fallback "MODE_LIST\([0-9]+ B\) EDID\(256 B\) RSDP" "the EDID record is written"
want e-fallback "source=edid clock_hz=241500000" "the kernel uses the Discovered bytes"
want_kernel_ok e-fallback

finish
