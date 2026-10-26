#!/bin/bash
# slow
# BT-1 OVMF matrix, the paths that must stop readably (error screen, also on serial; back to the firmware)
# or fall back: no kernel, a broken ELF in every generation, a broken newest generation with a good older
# confirmed one, an ExitBootServices that never succeeds (test hook, honoured only under a hypervisor), a
# zero-byte KERNEL next to a well-formed ELF whose entry point lies outside its code, and no INITRD.
. "$(dirname "$0")/lib.bash"

# 1. No KERNEL at all.
make_image bt-nokernel
mdel -i "$(img bt-nokernel)" ::/BITSOS2/SYS/1/KERNEL
boot bt-nokernel r-nokernel 45 --net none
want_refusal r-nokernel
want r-nokernel "generation 1 has no KERNEL and no older confirmed (OK) generation can start" "the reason names the missing kernel"

# 2. Bad ELF: generation 1 (confirmed) cut short, generation 2 not an ELF at all.
make_image bt-badelf
mark bt-badelf 1 OK
KERNEL_FILE="$ROOT/target/x86_64-unknown-none/debug/kernel"
head -c 3000 "$KERNEL_FILE" >"$RUN_DIR/cut-kernel"
head -c 100000 /dev/zero | tr '\0' 'Z' >"$RUN_DIR/junk-kernel"
mcopy -o -i "$(img bt-badelf)" "$RUN_DIR/cut-kernel" ::/BITSOS2/SYS/1/KERNEL
copy_gen bt-badelf 2   # with an INITRD, so the KERNEL check decides
mcopy -o -i "$(img bt-badelf)" "$RUN_DIR/junk-kernel" ::/BITSOS2/SYS/2/KERNEL
boot bt-badelf r-badelf 45 --net none
want_refusal r-badelf
want r-badelf "\\BITSOS2\\SYS\\2\\KERNEL: not an ELF file (bad magic)" "the junk file is named"
want r-badelf "\\BITSOS2\\SYS\\1\\KERNEL: a segment lies outside the file" "the cut-short kernel is named"

# 3. Broken newest generation, good older confirmed one: start the older one and say so.
make_image bt-fallback
mark bt-fallback 1 OK
copy_gen bt-fallback 2   # with an INITRD, so the KERNEL check decides
mcopy -o -i "$(img bt-fallback)" "$RUN_DIR/junk-kernel" ::/BITSOS2/SYS/2/KERNEL
boot bt-fallback r-fallback 60 --net none
want r-fallback "generation.chosen number=1 reason=FALLBACK_BAD_FILES skipped=2 flags=-:" "the fallback is logged"
want r-fallback "kernel start version=0.1.0 generation=1" "the kernel got generation 1 in its GENERATION record"
want_kernel_ok r-fallback

# 4. ExitBootServices never succeeds: readable error instead of a reset or a black screen.
make_image bt-ebsfail "bitsboot.test=ebs_fail"
boot bt-ebsfail r-ebsfail 45 --net none
want_refusal r-ebsfail
want r-ebsfail "the firmware did not let go of the machine: ExitBootServices failed 8 times" "the reason names ExitBootServices"

# 5. Generation 2: a zero-byte KERNEL; generation 1: the real kernel with its entry point moved into the
#    read-only data segment (valid header, every segment fine, entry not executable).
make_image bt-hostile
mark bt-hostile 1 OK
: >"$RUN_DIR/empty-kernel"
python3 -I -c '
import struct, sys
b = bytearray(open(sys.argv[1], "rb").read())
phoff, = struct.unpack_from("<Q", b, 32)
phnum, = struct.unpack_from("<H", b, 56)
for i in range(phnum):
    ty, flags = struct.unpack_from("<II", b, phoff + 56 * i)
    vaddr, = struct.unpack_from("<Q", b, phoff + 56 * i + 16)
    if ty == 1 and not flags & 1:
        struct.pack_into("<Q", b, 24, vaddr + 16)
        break
else:
    sys.exit("no non-executable PT_LOAD segment")
open(sys.argv[2], "wb").write(b)
' "$KERNEL_FILE" "$RUN_DIR/badentry-kernel"
mcopy -o -i "$(img bt-hostile)" "$RUN_DIR/badentry-kernel" ::/BITSOS2/SYS/1/KERNEL
copy_gen bt-hostile 2   # with an INITRD, so the KERNEL check decides
mcopy -o -i "$(img bt-hostile)" "$RUN_DIR/empty-kernel" ::/BITSOS2/SYS/2/KERNEL
boot bt-hostile r-hostile 45 --net none
want_refusal r-hostile
want r-hostile "\\BITSOS2\\SYS\\2\\KERNEL: too short for an ELF header (under 64 bytes)" "the zero-byte kernel is named"
want r-hostile "\\BITSOS2\\SYS\\1\\KERNEL: the entry point lies outside every executable segment" "the bad entry point is named"

# 6. No INITRD in the only generation: "bad files" (rule 5), no confirmed generation -> error screen.
make_image bt-noinitrd
mdel -i "$(img bt-noinitrd)" ::/BITSOS2/SYS/1/INITRD
boot bt-noinitrd r-noinitrd 45 --net none
want_refusal r-noinitrd
want r-noinitrd "generation 1 has no INITRD and no older confirmed (OK) generation can start" "the reason names the missing INITRD"
finish
