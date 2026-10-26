#!/bin/bash
# slow
# BT-3: `xtask stick` on real FAT32 file systems. Every stick here is an image file made in this run (mkfs.fat on
# a file below the worktree), attached as a loop device and mounted by udisks without any password prompt
# (--no-user-interaction); before every call the mount's source is checked to be that loop device, and the loop
# device's backing file to be that image. No other device is ever named. Covered: the refusal matrix (V1
# stick-test.mjs make-stick cases: system paths, NTFS -> ext4 here, FAT16, wrong/missing label even with --force,
# wrong/missing ID, a Windows folder, another system's loader, cancel), dry runs that change nothing,
# --retire-v1 (backup equals the stick, only V1's paths gone, STICK.ID moved byte for byte), and the generation
# cycle on the same image with boots through bitsboot in OVMF: NEW -> TRY -> confirm -> next bundle -> fallback
# after an unconfirmed try -> a Reload-proven bundle takes its place as OK -> prune.
. "$(dirname "$0")/lib.bash"

W="$RUN_DIR/stick"
rm -rf "$W"
mkdir -p "$W"
# A test identity only; the real stick's ID is never used here.
TEST_ID="0123456789abcdef0123456789abcdef"
printf '%s\r\n' "$TEST_ID" >"$W/v1-stick.id"
printf '%s' "$TEST_ID" >"$W/stick.id"
LOOPS=""
DEV=""
MP=""

cleanup() {
    for d in $LOOPS; do
        udisksctl unmount --no-user-interaction -b "$d" >/dev/null 2>&1 || true
        udisksctl loop-delete --no-user-interaction -b "$d" >/dev/null 2>&1 || true
    done
}
trap cleanup EXIT

# $1 = image file (below the worktree). Sets DEV and MP.
attach() {
    local f out
    f="$(realpath "$1")"
    case "$f" in "$ROOT"/*) ;; *) echo "RED: $f is not below the worktree"; exit 1 ;; esac
    out="$(udisksctl loop-setup --no-user-interaction -f "$f" 2>&1)" || { echo "$out"; return 1; }
    DEV="$(printf '%s\n' "$out" | sed -n 's|^Mapped file .* as \(/dev/loop[0-9]*\)\.$|\1|p')"
    [ -n "$DEV" ] || { echo "RED: no loop device in: $out"; exit 1; }
    LOOPS="$LOOPS $DEV"
    if [ "$(cat "/sys/block/${DEV#/dev/}/loop/backing_file")" != "$f" ]; then
        echo "RED: $DEV is not backed by $f"
        exit 1
    fi
    udevadm settle --timeout=10 2>/dev/null || sleep 1
    MP="$(findmnt -rn -S "$DEV" -o TARGET | head -1)"
    if [ -z "$MP" ]; then
        udisksctl mount --no-user-interaction -b "$DEV" >/dev/null
        MP="$(findmnt -rn -S "$DEV" -o TARGET | head -1)"
    fi
    MP="$(printf '%b' "$MP")"  # findmnt -r writes a space as \x20
    [ -n "$MP" ] || { echo "RED: $DEV did not mount"; exit 1; }
}

detach() {
    udisksctl unmount --no-user-interaction -b "$DEV" >/dev/null
    udisksctl loop-delete --no-user-interaction -b "$DEV" >/dev/null
    LOOPS="${LOOPS/ $DEV/}"
    udevadm settle --timeout=10 2>/dev/null || sleep 1
    DEV=""
    MP=""
}

# $1 = tag, $2 = answer to the question, rest = xtask stick arguments. Sets RC; output in stick-<tag>.txt.
stick() {
    local tag="$1" ans="$2"
    shift 2
    if [ -n "$MP" ] && [ "$(findmnt -rn -o SOURCE -M "$MP")" != "$DEV" ]; then
        echo "RED: $MP is no longer our loop device $DEV; stopping"
        exit 1
    fi
    set +e
    printf '%s\n' "$ans" | cargo xtask stick --id-file "$W/stick.id" "$@" >"$RUN_DIR/stick-$tag.txt" 2>&1
    RC=$?
    set -e
}

ok_run() { # $1 = tag, $2 = what
    if [ "$RC" = 0 ]; then echo "  ok: $2"; else echo "  RED: $2 (exit $RC):"; tail -5 "$RUN_DIR/stick-$1.txt"; FAILED=1; fi
}

refused_run() { # $1 = tag, $2 = text the refusal names, $3 = what
    if [ "$RC" != 0 ] && grep -qF -- "$2" "$RUN_DIR/stick-$1.txt"; then
        echo "  ok: $3: $(grep -m1 -E 'refused|cancelled|xtask:' "$RUN_DIR/stick-$1.txt" | sed 's/^xtask: //' | cut -c1-110)"
    else
        echo "  RED: $3 (exit $RC, wanted '$2'):"; tail -5 "$RUN_DIR/stick-$1.txt"; FAILED=1
    fi
}

# Every folder and file below $1 with checksums; names lower case (FAT ignores case, and mtools and the vfat
# mount may spell a short name differently).
dir_sum() {
    (cd "$1" && { find . -type d; find . -type f -print0 | xargs -0r sha256sum; }) | tr 'A-Z' 'a-z' | sort
}

# The same for an image file (mtools, image not attached); the tree stays in $W/tree-<tag>.
tree_sum() {
    local d="$W/tree-$2"
    rm -rf "$d"
    mkdir -p "$d"
    mcopy -s -n -i "$1" "::/*" "$d/" 2>/dev/null || true
    dir_sum "$d"
}

same_tree() { # $1 = image, $2 = sum file from before, $3 = what
    if tree_sum "$1" after | diff "$2" - >"$W/diff.txt"; then echo "  ok: $3"; else echo "  RED: $3:"; head "$W/diff.txt"; FAILED=1; fi
}

fsck_ok() { # $1 = image, $2 = what
    if fsck.fat -n "$1" >"$W/fsck.txt" 2>&1; then echo "  ok: fsck.fat -n: $2"; else echo "  RED: fsck.fat -n: $2:"; tail -5 "$W/fsck.txt"; FAILED=1; fi
}

mkimg() { # $1 = file, $2 = label ("" = none), $3 = FAT bits, $4 = size in KiB
    rm -f "$1"
    if [ -n "$2" ]; then mkfs.fat -F "$3" -n "$2" -C "$1" "$4" >/dev/null; else mkfs.fat -F "$3" -C "$1" "$4" >/dev/null; fi
}

put() { # $1 = image, $2 = local file, $3 = path on the stick (A/B/C)
    local p="" c
    IFS=/ read -ra parts <<<"$(dirname "$3")"
    for c in "${parts[@]}"; do
        [ "$c" = . ] && continue
        p="$p/$c"
        mmd -i "$1" "::$p" 2>/dev/null || true
    done
    mcopy -o -i "$1" "$2" "::/$3"
}

# --- can this user attach a loop device without a prompt? (udisks/polkit of the desktop session)
mkimg "$W/probe.img" PROBE 32 65536
if ! attach "$W/probe.img" >"$W/probe.txt" 2>&1; then
    echo "SKIP: udisksctl cannot attach a loop device without a password here: $(head -1 "$W/probe.txt")"
    echo "bt 50-stick: skipped"
    exit 0
fi
detach

# --- the bundle (a ktest kernel that runs one test and leaves QEMU with PASS)
echo "--- building the bundle"
make_image bt-stickesp
ESP="$X/esp-bt-stickesp"
B1="$W/bundle1"
B2="$W/bundle2"
mkdir -p "$B1" "$B2"
objcopy --strip-debug "$ESP/BITSOS2/SYS/1/KERNEL" "$B1/KERNEL"
cp "$ESP/BITSOS2/SYS/1/KERNEL" "$B2/KERNEL"   # the unstripped one: other bytes, same kernel
for b in "$B1" "$B2"; do
    cp "$ESP/BITSOS2/SYS/1/INITRD" "$b/INITRD"
    cp "$ESP/EFI/BOOT/BOOTX64.EFI" "$b/BOOTX64.EFI"
done
cp "$ESP/BITSOS2/CONFIG.TXT" "$W/CONFIG.TXT"
cargo build -q -p xtask 2>/dev/null || true

# --- paths that are no stick at all (nothing is attached; the tool stops before it asks udev anything)
echo "--- refusals without a stick"
MP=""
stick root J --target / --bundle "$B1" --write
refused_run root "mount point of the running system" "/ is refused"
if findmnt -rn -M /boot >/dev/null; then
    stick boot J --target /boot --bundle "$B1" --write --force
    refused_run boot "mount point of the running system" "/boot (the system's EFI partition) is refused even with --force"
fi
stick plain J --target "$W" --bundle "$B1" --write
refused_run plain "not a mount point" "a plain folder is refused"
stick none J --target "$W/nope" --bundle "$B1" --write
refused_run none "refused" "a missing path is refused"
stick notarget J --bundle "$B1" --write
refused_run notarget "needs --target" "no target given"

# --- the refusal matrix on real file systems
echo "--- refusal matrix (loop-mounted images)"
printf 'not a loader' >"$W/foreign.efi"
[ -f /usr/lib/systemd/boot/efi/systemd-bootx64.efi ] && cp /usr/lib/systemd/boot/efi/systemd-bootx64.efi "$W/foreign.efi"
mkdir -p "$W/win"
echo x >"$W/win/notepad.exe"
matrix() { # $1 = name, $2 = label, $3 = FAT bits, $4 = id file or "", $5 = expected refusal text, $6 = what, [$7 = extra path]
    local im="$W/$1.img"
    mkimg "$im" "$2" "$3" 65536
    [ -n "$4" ] && put "$im" "$4" BITSOS2/STICK.ID
    case "${7:-}" in
        windows) put "$im" "$W/win/notepad.exe" Windows/System32/notepad.exe ;;
        foreign) put "$im" "$W/foreign.efi" EFI/BOOT/BOOTX64.EFI ;;
    esac
    tree_sum "$im" before >"$W/$1.before"
    attach "$im"
    stick "$1" J --target "$MP" --bundle "$B1" --write
    refused_run "$1" "$5" "$6"
    stick "$1-force" J --target "$MP" --bundle "$B1" --write --force
    refused_run "$1-force" "$5" "$6, also with --force"
    detach
    same_tree "$im" "$W/$1.before" "$1: the image is unchanged"
}
matrix label ZORINOS 32 "$W/stick.id" "must be labelled BITSOS" "another stick's label (ZORINOS)"
matrix nolabel "" 32 "$W/stick.id" "must be labelled BITSOS" "no label"
matrix fat16 BITSOS 16 "$W/stick.id" "not FAT32" "FAT16 with the right label and ID"
printf 'fedcba9876543210fedcba9876543210' >"$W/other.id"
matrix wrongid BITSOS 32 "$W/other.id" "is not the recorded ID" "the right label, another stick ID"
matrix noid BITSOS 32 "" "not Georg's BitsOS stick" "the right label, no STICK.ID"
matrix windows BITSOS 32 "$W/stick.id" "Windows folder" "a Windows folder" windows
matrix foreign BITSOS 32 "$W/stick.id" "belongs to another system" "another system's BOOTX64.EFI" foreign
# ext4 instead of FAT (V1: NTFS): mkfs.ext4 on a file, mounted the same way.
if command -v mkfs.ext4 >/dev/null; then
    rm -f "$W/ext4.img"
    truncate -s 64M "$W/ext4.img"
    mkfs.ext4 -q -L BITSOS "$W/ext4.img"
    attach "$W/ext4.img"
    stick ext4 J --target "$MP" --bundle "$B1" --write --force
    refused_run ext4 "not FAT32" "ext4 labelled BITSOS is refused even with --force"
    detach
fi

# --- the A0 path on one image: V1 stick -> retire -> bundles and boots
echo "--- a V1 stick (as Georg's, with test files) is retired"
IMG="$X/bt-stick.img"
# INT-W9: 1 GiB (sparse): a bundle with the stick programs and the cobalt tree (YT-2, 194 MB) needs about 300 MB.
mkimg "$IMG" BITSOS 32 1048576
head -c 1000000 /dev/urandom >"$W/doom.bxe"
echo "screen=native" >"$W/v1-config.txt"
printf 'V1 kernel' >"$W/kernel.bin"
echo "{guid}" >"$W/guid"
echo "Georg" >"$W/readme.txt"
V1_LOADER=/home/georg/BitsOS/BitsOS-Plattform/stick/EFI/BOOT/BOOTX64.EFI  # read-only reference, copied
if [ -f "$V1_LOADER" ]; then cp "$V1_LOADER" "$W/v1-loader.efi"; else printf 'MZ....BitsOS loader on ....' >"$W/v1-loader.efi"; fi
put "$IMG" "$W/v1-stick.id" BITSOS/STICK.ID
put "$IMG" "$W/kernel.bin" BITSOS/KERNEL.BIN
put "$IMG" "$W/v1-config.txt" BITSOS/CONFIG.TXT
put "$IMG" "$W/doom.bxe" BITSOS/GAMES/DOOM.BXE
put "$IMG" "$W/readme.txt" "BITSOS/Bild mit langem Namen.asm"
mmd -i "$IMG" ::/BITSOS/SAVES
put "$IMG" "$W/v1-loader.efi" EFI/BOOT/BOOTX64.EFI
put "$IMG" "$W/guid" "System Volume Information/IndexerVolumeGuid"
put "$IMG" "$W/readme.txt" README.TXT
tree_sum "$IMG" v1 >"$W/v1.sum"
cp -r "$W/tree-v1" "$W/orig"

attach "$IMG"
stick v1-bundle J --target "$MP" --bundle "$B1" --write
refused_run v1-bundle "retire V1 first" "a bundle onto a stick that still holds V1 is refused"
stick v1-dry J --target "$MP" --retire-v1 --backup "$W/backup"
ok_run v1-dry "retire-v1 dry run"
[ ! -e "$W/backup" ] && echo "  ok: the dry run made no backup" || { echo "  RED: the dry run made a backup"; FAILED=1; }
stick v1-cancel nein --target "$MP" --retire-v1 --backup "$W/backup" --write
refused_run v1-cancel "cancelled" "retire-v1 without J"
detach
same_tree "$IMG" "$W/v1.sum" "dry run and cancel changed nothing"

attach "$IMG"
stick retire J --target "$MP" --retire-v1 --backup "$W/backup" --write
ok_run retire "retire-v1 with J"
detach
if diff <(dir_sum "$W/orig") <(dir_sum "$W/backup/files") >"$W/diff.txt"; then echo "  ok: the backup equals the stick as it was"; else echo "  RED: backup differs:"; head "$W/diff.txt"; FAILED=1; fi
grep -q "compare green" "$W/backup/RESULT.txt" && echo "  ok: RESULT.txt says compare green" || { echo "  RED: no RESULT.txt"; FAILED=1; }
cmp -s "$W/backup/SHA256-stick.txt" "$W/backup/SHA256-copy.txt" && echo "  ok: SHA-256 lists equal" || { echo "  RED: SHA-256 lists differ"; FAILED=1; }
# vfat (the stick) and btrfs/ext4 (the worktree) both take O_DIRECT: the compare must not have read cached pages.
grep -q "both sides read with O_DIRECT" "$W/backup/RESULT.txt" && echo "  ok: the compare read both sides with O_DIRECT (past the page cache)" || { echo "  RED: the compare went through the page cache: $(cat "$W/backup/RESULT.txt")"; FAILED=1; }
rm -rf "$W/expected"
cp -r "$W/orig" "$W/expected"
find "$W/expected" -mindepth 1 -maxdepth 1 -iname bitsos -exec rm -rf {} +
find "$W/expected" -ipath "$W/expected/efi/boot/bootx64.efi" -delete
mkdir -p "$W/expected/BITSOS2"
find "$W/orig" -ipath "$W/orig/bitsos/stick.id" -exec cp {} "$W/expected/BITSOS2/STICK.ID" \;
cmp -s "$W/expected/BITSOS2/STICK.ID" "$W/v1-stick.id" || { echo "  RED: test set-up: no V1 STICK.ID in the extraction"; FAILED=1; }
if tree_sum "$IMG" retired | diff <(dir_sum "$W/expected") - >"$W/diff.txt"; then
    echo "  ok: only V1's paths are gone, STICK.ID moved byte for byte, everything else stayed"
else
    echo "  RED: the retired stick is not as expected:"; head "$W/diff.txt"; FAILED=1
fi
fsck_ok "$IMG" "after retire-v1"

echo "--- generation 1: first bundle (bitsboot, CONFIG.TXT), then its boot"
attach "$IMG"
stick retire-again J --target "$MP" --retire-v1 --backup "$W/backup2" --write
refused_run retire-again "V1 is not on this stick" "retire-v1 runs once"
stick g1-dry J --target "$MP" --bundle "$B1" --config "$W/CONFIG.TXT"
ok_run g1-dry "dry run of the first bundle"
stick g1 J --target "$MP" --bundle "$B1" --config "$W/CONFIG.TXT" --write
ok_run g1 "first bundle written"
detach
fsck_ok "$IMG" "after the first bundle"
want_files bt-stick 1 "INITRD KERNEL NEW" "generation 1 is NEW"
mcopy -n -i "$IMG" ::/EFI/BOOT/BOOTX64.EFI "$W/got.efi"
cmp -s "$W/got.efi" "$B1/BOOTX64.EFI" && echo "  ok: bitsboot is on the stick" || { echo "  RED: BOOTX64.EFI differs"; FAILED=1; }
boot bt-stick s-g1 120 --net none
want s-g1 "generation.chosen number=1 reason=NEWEST_TRY skipped=0 flags=-:" "bitsboot gives the new generation 1 its try"
want_kernel_ok s-g1
want_files bt-stick 1 "INITRD KERNEL NEW TRY" "bitsboot wrote TRY"

echo "--- green round: confirm 1 and write generation 2 in one go, then its try and the fallback"
attach "$IMG"
stick g2 J --target "$MP" --confirm 1 --bundle "$B2" --write
ok_run g2 "confirm 1 + bundle 2"
grep -qF "bitsboot is unchanged and stays" "$RUN_DIR/stick-g2.txt" && echo "  ok: unchanged bitsboot not rewritten" || { echo "  RED: bitsboot rewritten"; FAILED=1; }
detach
fsck_ok "$IMG" "after confirm + bundle"
want_files bt-stick 1 "INITRD KERNEL OK" "generation 1 confirmed (OK, TRY and NEW gone)"
want_files bt-stick 2 "INITRD KERNEL NEW" "generation 2 is NEW"
boot bt-stick s-g2 120 --net none
want s-g2 "generation.chosen number=2 reason=NEWEST_TRY skipped=0 flags=-:" "generation 2 gets its try"
want_kernel_ok s-g2
boot bt-stick s-g2-again 120 --net none
want s-g2-again "generation.chosen number=1 reason=FALLBACK_AFTER_TRY skipped=2" "never confirmed: the next start falls back to 1"
want_kernel_ok s-g2-again

echo "--- a Reload-proven bundle takes the unconfirmed generation 2's place as OK; prune"
attach "$IMG"
stick g2-same J --target "$MP" --bundle "$B2" --write
refused_run g2-same "same as generation 2" "the same bundle again needs --force"
stick g2p J --target "$MP" --bundle "$B1" --proven --write
ok_run g2p "proven bundle"
detach
want_files bt-stick 2 "INITRD KERNEL OK" "generation 2 replaced, confirmed"
boot bt-stick s-g2p 120 --net none
want s-g2p "generation.chosen number=2 reason=NEWEST_OK skipped=0 flags=-:" "the confirmed generation 2 starts"
want_kernel_ok s-g2p
attach "$IMG"
stick prune J --target "$MP" --prune --sys-keep 1 --write
ok_run prune "prune to one confirmed generation"
detach
fsck_ok "$IMG" "after prune"
if mdir -b -i "$IMG" ::/BITSOS2/SYS | sed 's|.*/||' | grep -qx 1; then echo "  RED: generation 1 still there"; FAILED=1; else echo "  ok: generation 1 pruned, 2 stays"; fi
want_files bt-stick 2 "INITRD KERNEL OK" "generation 2 untouched by the prune"

echo "--- --build: the tool builds kernel, initramfs and bitsboot itself (the plug-over path, debug here)"
attach "$IMG"
stick build J --target "$MP" --build --write
ok_run build "--build --write"
detach
fsck_ok "$IMG" "after --build"
want_files bt-stick 3 "INITRD KERNEL NEW" "the built bundle is generation 3, NEW"
for f in KERNEL INITRD; do
    mcopy -n -i "$IMG" "::/BITSOS2/SYS/3/$f" "$W/got-$f"
    cmp -s "$W/got-$f" "$X/stick-bundle/$f" && echo "  ok: generation 3's $f is the built one" || { echo "  RED: generation 3's $f differs from $X/stick-bundle/$f"; FAILED=1; }
done
mcopy -n -i "$IMG" ::/EFI/BOOT/BOOTX64.EFI "$W/got.efi"
cmp -s "$W/got.efi" "$X/stick-bundle/BOOTX64.EFI" && echo "  ok: the stick's bitsboot is the built one" || { echo "  RED: BOOTX64.EFI is not the built bitsboot"; FAILED=1; }
finish
