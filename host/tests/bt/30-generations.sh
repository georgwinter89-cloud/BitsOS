#!/bin/bash
# slow
# BT-2 OVMF scenarios of the generation marker rule (V2-PLAN §7), each with a second boot of the same image:
# a new generation that never gets confirmed (the kernel ends through isa-debug-exit with -no-reboot: the
# model of a hang + watchdog reset) -> the older OK generation from the next boot on; a confirmed generation
# stays and nothing is written; a broken ELF in a NEW generation -> the OK one at once, NEW untouched; no
# older OK generation; a pin; an unmarked newest generation above a confirmed one; hostile PIN files and odd
# entries below SYS; and bitsboot's only write is a new empty TRY below \BITSOS2\SYS\ (whole image compared
# file by file before and after; NEW stays, nothing is renamed or deleted).
. "$(dirname "$0")/lib.bash"

# 1. Generation 1 confirmed, generation 2 freshly installed (NEW).
make_image bt-gen
mark bt-gen 1 OK
copy_gen bt-gen 2
mark bt-gen 2 NEW
snapshot bt-gen before >"$RUN_DIR/before.sum"
boot bt-gen g-new 60 --net none
want_gen g-new 2 NEWEST_TRY 0 - "boot 1: the new generation 2 gets its one try"
want g-new "bitsboot marker.written: \\BITSOS2\\SYS\\2\\TRY" "TRY was written before the start"
want_kernel_ok g-new
want_files bt-gen 2 "INITRD KERNEL NEW TRY" "generation 2 now carries TRY next to NEW"
want_files bt-gen 1 "INITRD KERNEL OK" "generation 1 is unchanged"
snapshot bt-gen after >"$RUN_DIR/after.sum"
{ cat "$RUN_DIR/before.sum"; echo "$(sha256sum </dev/null | cut -d' ' -f1)  ./BITSOS2/SYS/2/TRY"; } | sort -k2 >"$RUN_DIR/before-plus-try.sum"
if sort -k2 "$RUN_DIR/after.sum" | diff "$RUN_DIR/before-plus-try.sum" - >"$RUN_DIR/sum.diff"; then
    echo "  ok: every other file of the stick is byte for byte the same (only an empty SYS\\2\\TRY was added)"
else
    echo "  RED: the boot changed more than the marker:"; cat "$RUN_DIR/sum.diff"; FAILED=1
fi

# 2. The kernel of boot 1 never confirmed (it left through isa-debug-exit, QEMU did not reboot): the next
#    boot of the same image is the watchdog reset's boot -> generation 1, and it stays so.
boot bt-gen g-after-try 60 --net none
want_gen g-after-try 1 FALLBACK_AFTER_TRY 2 - "boot 2: generation 2 was tried and never confirmed -> newest OK"
want_kernel_ok g-after-try
want_files bt-gen 2 "INITRD KERNEL NEW TRY" "TRY stays (no BAD marker, plan §7)"
boot bt-gen g-after-try2 60 --net none
want_gen g-after-try2 1 FALLBACK_AFTER_TRY 2 - "boot 3: still generation 1 until generation 2 is confirmed or replaced"

# 3. Confirmation (what the stick tool / RB-6 do: OK written, TRY deleted): generation 2 is started and
#    bitsboot writes nothing at all (the image file stays byte for byte the same).
mdel -i "$(img bt-gen)" ::/BITSOS2/SYS/2/TRY
mark bt-gen 2 OK
sha256sum <"$(img bt-gen)" >"$RUN_DIR/img-before.sha"
boot bt-gen g-confirmed 60 --net none
want_gen g-confirmed 2 NEWEST_OK 0 - "a confirmed newest generation is started"
want_kernel_ok g-confirmed
boot bt-gen g-confirmed2 60 --net none
want_gen g-confirmed2 2 NEWEST_OK 0 - "and stays on the next boot"
if sha256sum <"$(img bt-gen)" | cmp -s - "$RUN_DIR/img-before.sha"; then
    echo "  ok: two boots of a confirmed generation left the stick image unchanged"
else
    echo "  RED: the stick image changed although no marker had to be written"; FAILED=1
fi

# 4. Broken ELF in a NEW generation 3: generation 2 (newest OK) at once, generation 3 keeps NEW (no try
#    needed, rule 5).
#    No copy of the real INITRD (~26 MB since A2): a third full generation no longer fits the test stick, and
#    the broken kernel fails before the INITRD matters; a small file with the cpio magic stands in for it.
head -c 100000 /dev/zero | tr '\0' 'Z' >"$RUN_DIR/junk-kernel"
{ printf '070701'; head -c 4090 /dev/zero; } >"$RUN_DIR/small-initrd"
mmd -i "$(img bt-gen)" ::/BITSOS2/SYS/3 2>/dev/null || true
mcopy -o -i "$(img bt-gen)" "$RUN_DIR/small-initrd" ::/BITSOS2/SYS/3/INITRD
mcopy -o -i "$(img bt-gen)" "$RUN_DIR/junk-kernel" ::/BITSOS2/SYS/3/KERNEL
mark bt-gen 3 NEW
boot bt-gen g-badelf 60 --net none
want_gen g-badelf 2 FALLBACK_BAD_FILES 3 - "a broken ELF in the newest generation -> newest OK at once"
want g-badelf "bitsboot generation.bad_files number=3: \\BITSOS2\\SYS\\3\\KERNEL: not an ELF file (bad magic)" "the bad file is named"
want_kernel_ok g-badelf
want_files bt-gen 3 "INITRD KERNEL NEW" "the broken generation keeps NEW (never tried)"

# 5. A broken INITRD (not cpio) in generation 3 with a good kernel: the same fallback.
mcopy -o -i "$(img bt-gen)" "$RUN_DIR/gen-copy/KERNEL" ::/BITSOS2/SYS/3/KERNEL
mcopy -o -i "$(img bt-gen)" "$RUN_DIR/junk-kernel" ::/BITSOS2/SYS/3/INITRD
boot bt-gen g-badinitrd 60 --net none
want_gen g-badinitrd 2 FALLBACK_BAD_FILES 3 - "a broken INITRD -> newest OK at once"
want g-badinitrd "\\BITSOS2\\SYS\\3\\INITRD (100000 bytes): not a cpio newc archive (bad magic)" "the bad INITRD is named"
want_files bt-gen 3 "INITRD KERNEL NEW" "generation 3 keeps NEW"

# 6. A pin: PIN=1 starts generation 1 although 2 is the newest OK and 3 is NEW; nothing is marked.
printf '1\r\n' >"$RUN_DIR/PIN"
mcopy -o -i "$(img bt-gen)" "$RUN_DIR/PIN" ::/BITSOS2/SYS/PIN
boot bt-gen g-pin 60 --net none
want_gen g-pin 1 PINNED 3 - "PIN 1 wins over the newest generations"
want g-pin "\\BITSOS2\\SYS\\PIN: generation 1" "the pin is logged"
want_kernel_ok g-pin
want_files bt-gen 3 "INITRD KERNEL NEW" "a pinned boot does not touch the NEW generation"

# 7. A pin to an unconfirmed generation is ignored (flag PIN_IGNORED): PIN=3 (NEW, bad INITRD) -> rule 5.
printf '3' >"$RUN_DIR/PIN"
mcopy -o -i "$(img bt-gen)" "$RUN_DIR/PIN" ::/BITSOS2/SYS/PIN
boot bt-gen g-badpin 60 --net none
want_gen g-badpin 2 FALLBACK_BAD_FILES 3 PIN_IGNORED "PIN 3 (not confirmed) is ignored and said so"
want g-badpin "PIN 3 ignored: not confirmed (no OK)" "the reason the pin was ignored"
want_kernel_ok g-badpin

# 8. No older OK generation: a fresh stick whose only generation was tried once (TRY) is started again
#    (else it could never start twice before RB-6 confirms it) and says so.
make_image bt-onlytry
mark bt-onlytry 1 TRY
boot bt-onlytry g-onlytry 60 --net none
want_gen g-onlytry 1 NEWEST_TRY 0 NO_CONFIRMED_FALLBACK "the only (tried) generation is started again, flagged"
want_kernel_ok g-onlytry
want_files bt-onlytry 1 "INITRD KERNEL TRY" "nothing written"

# 9. No older OK generation and the newest one is broken: the readable error screen, back to the firmware;
#    the NEW marker stays (rule 5: no try needed).
make_image bt-nook
copy_gen bt-nook 2
mcopy -o -i "$(img bt-nook)" "$RUN_DIR/junk-kernel" ::/BITSOS2/SYS/2/KERNEL
mark bt-nook 2 NEW
mark bt-nook 1 TRY
boot bt-nook g-nook 45 --net none
want_refusal g-nook
want g-nook "generation 2 failed the checks and no older confirmed (OK) generation can start" "the reason says there is no confirmed generation"
want g-nook "\\BITSOS2\\SYS\\2\\KERNEL: not an ELF file (bad magic)" "and names the bad file"
want_files bt-nook 2 "INITRD KERNEL NEW" "generation 2 keeps NEW"
# 10. An unmarked newest generation above a confirmed one (a hand copy, an install whose marker never got
#     written): one try like NEW, then the confirmed one; an unmarked generation alone is started as it is
#     (10-boot-matrix, the xtask test images).
make_image bt-unmarked
mark bt-unmarked 1 OK
copy_gen bt-unmarked 2
boot bt-unmarked g-unmarked 60 --net none
want_gen g-unmarked 2 NEWEST_TRY 0 UNMARKED "boot 1: the unmarked generation 2 gets one try"
want g-unmarked "bitsboot marker.written: \\BITSOS2\\SYS\\2\\TRY" "its TRY was written"
want_kernel_ok g-unmarked
want_files bt-unmarked 2 "INITRD KERNEL TRY" "generation 2 now carries TRY"
boot bt-unmarked g-unmarked2 60 --net none
want_gen g-unmarked2 1 FALLBACK_AFTER_TRY 2 - "boot 2: generation 2 was never confirmed -> generation 1"
want_kernel_ok g-unmarked2

# 11. Hostile input below SYS: PIN with junk, an empty PIN, PIN as a directory; directories that are no
#     generation (junk, NEW, TRY, a number beyond u32), a file named like a generation, and an empty
#     generation 4294967295 (the highest number there is, no KERNEL): no crash, the newest OK one starts.
make_image bt-odd
mark bt-odd 1 OK
for d in junk NEW TRY 99999999999 4294967295; do mmd -i "$(img bt-odd)" "::/BITSOS2/SYS/$d"; done
mcopy -o -i "$(img bt-odd)" "$RUN_DIR/empty-marker" ::/BITSOS2/SYS/5
printf 'abc\n' >"$RUN_DIR/PIN"
mcopy -o -i "$(img bt-odd)" "$RUN_DIR/PIN" ::/BITSOS2/SYS/PIN
boot bt-odd g-odd 60 --net none
want_gen g-odd 1 FALLBACK_BAD_FILES 4294967295 PIN_IGNORED "odd entries and a junk PIN: generation 1, the pin ignored"
want g-odd "bitsboot pin.invalid: \\BITSOS2\\SYS\\PIN holds no generation number (4 bytes); ignored" "the junk PIN is logged"
want g-odd "generation 4294967295 has no KERNEL" "the empty generation is named"
want_kernel_ok g-odd
want_files bt-odd 4294967295 "" "nothing was written into the empty generation"
: >"$RUN_DIR/PIN"
mcopy -o -i "$(img bt-odd)" "$RUN_DIR/PIN" ::/BITSOS2/SYS/PIN
boot bt-odd g-emptypin 60 --net none
want_gen g-emptypin 1 FALLBACK_BAD_FILES 4294967295 PIN_IGNORED "an empty PIN is ignored"
want g-emptypin "holds no generation number (0 bytes); ignored" "and logged"
mdel -i "$(img bt-odd)" ::/BITSOS2/SYS/PIN
mmd -i "$(img bt-odd)" ::/BITSOS2/SYS/PIN
boot bt-odd g-dirpin 60 --net none
want_gen g-dirpin 1 FALLBACK_BAD_FILES 4294967295 PIN_IGNORED "a PIN directory is ignored"
want g-dirpin "bitsboot pin.invalid: \\BITSOS2\\SYS\\PIN is a directory, not a file; ignored" "and logged"
want_kernel_ok g-dirpin
finish
