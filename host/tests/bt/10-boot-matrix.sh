#!/bin/bash
# slow
# BT-1 OVMF matrix, the paths that must reach the kernel: 2560x1440 VGA (mode list), another display device
# (cirrus; OVMF offers only BGR modes on every QEMU display, RGB is host-tested in bitsboot_pure::pixel), a
# huge CONFIG.TXT, a second disk carrying \BITSOS2\KERNEL that must be ignored, the ExitBootServices
# retry on a stale map key (test hook, honoured only under a hypervisor), boot modules from \BITSOS2\fw\,
# an empty fw directory, and no CONFIG.TXT. (BT-2's generation and screen cases: 30-generations, 40-screen.)
. "$(dirname "$0")/lib.bash"

ALL_RECORDS="BootInfo records: HANDOVER GENERATION KERNEL_IMAGE MODULE CONFIG("
make_image bt-base

# 1. 2560x1440 (the Mini-PC's native size): OVMF already starts in it, so bitsboot keeps the mode.
boot bt-base m-1440 60 --net none --res 2560x1440
want_kernel_ok m-1440
want_re m-1440 "screen modes: .*[0-9]+:2560x1440 " "the GOP mode list holds 2560x1440"
want m-1440 "screen 2560x1440 stride 2560 BGR at 0x" "the current mode (OVMF starts in xres x yres) is recorded, BGR"
want_re m-1440 "bitsboot screen.kept wanted=2560x1440 mode=[0-9]+ \(2560x1440\): already in 2560x1440" "no switch when the firmware is already native"
want m-1440 "display device name=gopfb width=2560 height=1440" "the kernel draws at 2560x1440 from the FRAMEBUFFER record"
want m-1440 "$ALL_RECORDS" "the BootInfo record list is logged"
want_re m-1440 "STICK\([0-9]+ B\) FRAMEBUFFER MODE_LIST\([0-9]+ B\) (EDID\([0-9]+ B\) )?RSDP SMBIOS FIRMWARE BOOT_ID TSC RELOAD CRASH_PAGE TRAMPOLINE RELOAD_STUB PAGE_TABLES LOADER_LOG" "every BootInfo record is written"
want_re m-1440 "stick: controller PCI 0000:00:0[0-9a-f]\.0" "stick: the xHCI's PCI address"
want m-1440 "stick: USB 46f4:0001" "stick: USB VID:PID of QEMU's usb-storage"
want_re m-1440 'stick: FAT32 volume label "BITSOS" serial [0-9A-F]{4}-[0-9A-F]{4}' "stick: volume label and serial"
want m-1440 "stick: ID 00112233445566778899aabbccddeeff from \\BITSOS2\\STICK.ID" "stick: STICK.ID"
want m-1440 "ExitBootServices done (try 1)" "ExitBootServices on the first try"
want_re m-1440 "^BootInfo modules: INITRD [0-9]+ B[[:space:]]*$" "the INITRD is the only MODULE record"

# 2. Another display device (cirrus): a different GOP driver and mode list. The image also carries two boot
#    modules in \BITSOS2\fw\ (sizes that are no multiple of a page): MODULE records in name order.
make_image bt-fw
head -c 4097 /dev/urandom >"$RUN_DIR/rtw8852b_fw-2.bin"
printf 'ten bytes!' >"$RUN_DIR/amdgpu_dmcub.bin"
mmd -i "$(img bt-fw)" ::/BITSOS2/fw
mcopy -i "$(img bt-fw)" "$RUN_DIR/rtw8852b_fw-2.bin" "$RUN_DIR/amdgpu_dmcub.bin" ::/BITSOS2/fw/
boot bt-fw m-cirrus 60 --net none -- -vga cirrus
want_kernel_ok m-cirrus
want_re m-cirrus '^\\BITSOS2\\fw\\rtw8852b_fw-2\.bin: 4097 bytes at 0x' "the first module is loaded"
want_re m-cirrus '^\\BITSOS2\\fw\\amdgpu_dmcub\.bin: 10 bytes at 0x' "the second module is loaded"
want_re m-cirrus "^BootInfo modules: INITRD [0-9]+ B, amdgpu_dmcub.bin 10 B, rtw8852b_fw-2.bin 4097 B[[:space:]]*$" \
    "the MODULE records as the kernel's reader sees them (name, size, order)"
want m-cirrus "KERNEL_IMAGE MODULE*3 CONFIG(" "three MODULE records in the record list"
want_re m-cirrus "^screen [0-9]+x[0-9]+ stride [0-9]+ BGR at 0x" "cirrus mode recorded"
want_re m-cirrus "display device name=gopfb width=[0-9]+" "the kernel draws on the recorded framebuffer"

# 3. A huge CONFIG.TXT (V1 stopped at 2047 bytes): 6 MiB of comments, one 1 MiB line, the real keys last.
make_image bt-bigcfg
mtype -i "$(img bt-bigcfg)" ::/BITSOS2/CONFIG.TXT >"$RUN_DIR/base-config.txt"
python3 -I -c '
import sys
out = open(sys.argv[1], "wb")
line = b"# padding of the huge CONFIG.TXT test (BT-1), the kernel must still find its keys below\n"
out.write(line * (6 * 1024 * 1024 // len(line)))
out.write(b"#" + b"x" * (1024 * 1024) + b"\n")
out.write(open(sys.argv[2], "rb").read())
' "$RUN_DIR/big-config.txt" "$RUN_DIR/base-config.txt"
BIG=$(stat -c %s "$RUN_DIR/big-config.txt")
mcopy -o -i "$(img bt-bigcfg)" "$RUN_DIR/big-config.txt" ::/BITSOS2/CONFIG.TXT
boot bt-bigcfg m-bigcfg 90 --net none
want m-bigcfg "\\BITSOS2\\CONFIG.TXT: $BIG bytes" "bitsboot read all $BIG bytes"
want m-bigcfg "CONFIG($BIG B)" "the CONFIG record carries all of it"
want_kernel_ok m-bigcfg

# 4. A second disk with \BITSOS2\KERNEL, a newer generation 9, its own CONFIG.TXT and STICK.ID: ignored.
OTHER="$X/bt-otherdisk.img"
rm -f "$OTHER"
mkfs.fat -n OTHERDISK -C "$OTHER" 16384 >/dev/null
printf 'NOT A KERNEL: bitsboot must never read this disk\n' >"$RUN_DIR/fake-kernel"
printf 'ktest=bt.must.not.run\n' >"$RUN_DIR/other-config.txt"
printf 'ffeeddccbbaa99887766554433221100' >"$RUN_DIR/other-stick.id"
mmd -i "$OTHER" ::/BITSOS2 ::/BITSOS2/SYS ::/BITSOS2/SYS/9
mcopy -i "$OTHER" "$RUN_DIR/fake-kernel" ::/BITSOS2/KERNEL
mcopy -i "$OTHER" "$RUN_DIR/fake-kernel" ::/BITSOS2/SYS/9/KERNEL
mcopy -i "$OTHER" "$RUN_DIR/other-config.txt" ::/BITSOS2/CONFIG.TXT
mcopy -i "$OTHER" "$RUN_DIR/other-stick.id" ::/BITSOS2/STICK.ID
boot bt-base m-otherdisk 60 --net none -- -drive "if=none,id=other,format=raw,file=$OTHER" -device virtio-blk-pci,drive=other
want_kernel_ok m-otherdisk
want m-otherdisk "generation.chosen number=1 reason=NEWEST_NO_RULE skipped=0 flags=UNMARKED:" "generation 1 from the stick, not 9 from the other disk"
want m-otherdisk "stick: ID 00112233445566778899aabbccddeeff" "the stick's own STICK.ID"
refuse m-otherdisk "bt.must.not.run" "the other disk's CONFIG.TXT was not read"
refuse m-otherdisk "NOT A KERNEL" "the other disk's kernel was not read"

# 5. ExitBootServices retry: the hook makes the first map key stale.
make_image bt-ebsretry "bitsboot.test=ebs_retry"
boot bt-ebsretry m-ebsretry 60 --net none
want m-ebsretry "test hook bitsboot.test: EbsRetry" "the hook is on (QEMU)"
want m-ebsretry "ExitBootServices done (try 2)" "the stale key was retried with a fresh map"
want_kernel_ok m-ebsretry

# 6. An empty \BITSOS2\fw\: the INITRD is the only MODULE record. (A missing INITRD is "bad files" since
#    BT-2: 20-refusals case 6.)
make_image bt-emptyfw
mmd -i "$(img bt-emptyfw)" ::/BITSOS2/fw
boot bt-emptyfw m-emptyfw 60 --net none
want_re m-emptyfw "^BootInfo modules: INITRD [0-9]+ B[[:space:]]*$" "only the INITRD"
want_kernel_ok m-emptyfw

# 7. No CONFIG.TXT: a warning, an empty CONFIG record, the kernel starts with its defaults. Without the ktest
#    key the ktest kernel runs its whole suite, so only the start is checked and the timeout ends the run.
make_image bt-noconfig
mdel -i "$(img bt-noconfig)" ::/BITSOS2/CONFIG.TXT
boot bt-noconfig m-noconfig 40 --net none
want m-noconfig "\\BITSOS2\\CONFIG.TXT is missing: the kernel uses its defaults" "the missing CONFIG.TXT is a warning"
want m-noconfig "CONFIG(0 B)" "an empty CONFIG record"
want m-noconfig "bitsboot: ExitBootServices done" "bitsboot handed over"
want m-noconfig "BitsOS V2 kernel: entry" "the kernel started"
want m-noconfig 'kernel config values=""' "the kernel sees no config keys"
finish
