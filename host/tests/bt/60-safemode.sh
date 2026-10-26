#!/bin/bash
# slow
# BT-5 Gaming-PC safe mode. OVMF with an Intel CPU model and a hybrid-like topology (20 CPUs like the 265K's
# 8 P + 12 E cores; QEMU cannot model core types). The Gaming-PC's DMI names are injected with -smbios: then the
# profile is chosen by DMI (under a hypervisor DMI is the only way; every other QEMU test runs on the Gaming-PC's
# own Intel CPU and must stay normal). Safe mode writes nothing to the stick (whole image compared file by file),
# starts the newest confirmed generation instead of spending a new one's try, withholds STICK.ID so the kernel
# mounts the stick read-only, keeps the firmware's picture, and its CONFIG lines win over the stick's own.
# CONFIG profile=normal turns it off; profile=gaming-pc turns it on anywhere.
. "$(dirname "$0")/lib.bash"

INTEL=(-cpu SierraForest -smp 20,sockets=1,dies=1,modules=5,cores=4,threads=1)
GPC_DMI=(-smbios "type=1,manufacturer=ASRock,product=Z890 Taichi OCF" -smbios "type=2,manufacturer=ASRock,product=Z890 Taichi OCF")

# 1. The Gaming-PC by DMI: generation 1 confirmed, generation 2 new.
make_image bt-safe "ktest=fs.fat.tree"
mark bt-safe 1 OK
copy_gen bt-safe 2
mark bt-safe 2 NEW
snapshot bt-safe before >"$RUN_DIR/before.sum"
boot bt-safe s-dmi 90 --net none --smp 20 -- "${INTEL[@]}" "${GPC_DMI[@]}"
want_re s-dmi "cpu: GenuineIntel family 0x6 model 0x[0-9a-f]+ hybrid=no hypervisor=yes" "the CPU line names the Intel model"
want_re s-dmi "dmi: SMBIOS [0-9]+\.[0-9]+ system \"ASRock\" \"Z890 Taichi OCF\", board \"ASRock\" \"Z890 Taichi OCF\"" "bitsboot read the DMI names"
want s-dmi "bitsboot profile.chosen name=gaming-pc reason=dmi: DMI names the Gaming-PC (ASRock Z890 Taichi OCF)" "DMI chose safe mode"
want_gen s-dmi 1 FALLBACK_AFTER_TRY 2 SAFE_MODE "the confirmed generation starts; the new one keeps its try"
want s-dmi "bitsboot stick.id_withheld" "STICK.ID is withheld from the kernel"
want_re s-dmi "bitsboot screen.kept wanted=firmware mode=[0-9]+ \(1280x800\)" "the firmware's picture stays"
want_re s-dmi "kernel config values=\".*net_drivers=off.*nvme_write=off.*panic=halt.*profile=gaming-pc profile_reason=dmi screen=firmware stick_write=off.*usb_msd_write=off wlan=off" "the kernel got the safe-mode lines"
want_re s-dmi "block configured .*stick_id=false stick_write=false usb_msd_write=false nvme_write=false" "the block layer has every write switch off and no stick identity"
want_re s-dmi "fat stick.decided .*rw=false.*bitsboot handed over no" "the kernel mounted the stick read-only"
want_re s-dmi "display device name=gopfb width=1280 height=800" "the kernel draws on the firmware's picture"
want_re s-dmi "kexec boot.confirm_not_needed reason=\"generation 1 started as FALLBACK_AFTER_TRY\"" "the kernel waits for no MCP confirmation"
want_re s-dmi "net stack.start cards=0" "no network card is driven"
want_kernel_ok s-dmi
refuse s-dmi "marker.written" "bitsboot wrote no marker"
want_files bt-safe 2 "INITRD KERNEL NEW" "generation 2 is still NEW (no TRY)"
snapshot bt-safe after >"$RUN_DIR/after.sum"
if diff "$RUN_DIR/before.sum" "$RUN_DIR/after.sum" >"$RUN_DIR/sum.diff"; then
    echo "  ok: the stick is byte for byte the same after the safe-mode boot"
else
    echo "  RED: the safe-mode boot changed the stick:"; cat "$RUN_DIR/sum.diff"; FAILED=1
fi

# 2. The same machine with profile=normal: the normal boot (generation 2 gets its try, the stick is writable).
make_image bt-safe-off "ktest=fs.fat.tree;profile=normal"
mark bt-safe-off 1 OK
copy_gen bt-safe-off 2
mark bt-safe-off 2 NEW
boot bt-safe-off s-off 90 --net none --smp 20 -- "${INTEL[@]}" "${GPC_DMI[@]}"
want s-off "bitsboot profile.chosen name=normal reason=config: CONFIG profile=normal" "CONFIG profile=normal wins over DMI"
want_gen s-off 2 NEWEST_TRY 0 - "the new generation gets its one try as always"
want_re s-off "fat stick.decided .*rw=true" "the stick is writable as always"
want_kernel_ok s-off
refuse s-off "stick.id_withheld" "STICK.ID is handed over"
refuse s-off "profile_reason=" "the CONFIG record is the file itself"

# 3. Intel under QEMU without the DMI names: normal (what every other QEMU test relies on).
make_image bt-safe-qemu "ktest=fs.fat.tree"
boot bt-safe-qemu s-qemu 90 --net none --smp 20 -- "${INTEL[@]}"
want_re s-qemu "bitsboot profile.chosen name=normal reason=hypervisor: GenuineIntel under a hypervisor" "an Intel CPU under QEMU stays normal"
want_re s-qemu "fat stick.decided .*rw=true" "the stick is writable"
want_kernel_ok s-qemu

# 4. profile=gaming-pc on any machine; the stick's own panic= wins over the default, screen= does not.
make_image bt-safe-cfg "ktest=fs.fat.tree;profile=gaming-pc;panic=reboot;screen=1024x768;sys_keep=3"
boot bt-safe-cfg s-cfg 90 --net none
want s-cfg "bitsboot profile.chosen name=gaming-pc reason=config: CONFIG profile=gaming-pc" "CONFIG profile=gaming-pc chose safe mode"
want_re s-cfg "generation.chosen number=1 reason=[A-Z_]+ skipped=0 flags=[A-Z_,]*SAFE_MODE" "the record says safe mode"
want_re s-cfg "kernel config values=\".*panic=reboot.*profile_reason=config screen=firmware.*sys_keep=3" "the stick's panic= wins, screen= does not"
want_re s-cfg "bitsboot screen.kept wanted=firmware" "no mode switch to 1024x768"
want_re s-cfg "fat stick.decided .*rw=false" "the stick is read-only"
want_re s-cfg "kexec boot.confirm_not_needed" "no boot confirmation in safe mode"
want_kernel_ok s-cfg

# 5. A hostile SMBIOS table: a type 1 structure whose header claims 127 bytes but holds 27 (then its strings).
#    The parser reads garbage names or none; the boot goes on normally (under a hypervisor without a match).
printf '\001\177\000\001\001\002\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000\006\000\000ASRock\000Z890 Taichi OCF\000\000' >"$RUN_DIR/long-type1.bin"
make_image bt-safe-bad "ktest=fs.fat.tree"
boot bt-safe-bad s-bad 90 --net none -- "${INTEL[@]}" -smbios "file=$RUN_DIR/long-type1.bin"
want_re s-bad "(dmi: SMBIOS|bitsboot dmi\.)" "bitsboot read or refused the table and said so"
want_re s-bad "bitsboot profile.chosen name=normal reason=hypervisor" "no false Gaming-PC match from a broken structure"
want_kernel_ok s-bad
finish
