#!/bin/bash
# FS-5: checks host/tests/fs/mmap-linux.txt against Linux itself. Builds host/tests/fs/progs/fs-mmap.c for this
# machine, runs every mode on CachyOS (tmpfs /tmp and /dev/shm, as a normal user) and compares the ends and the
# output with the file. The same program runs on V2 in ktest fs.map.programs (compared with the same file) and
# through `cargo xtask abi-diff abi-mmap` (once user/abi-tests carries it, FS-5 change request 5).
set -eu
cd "$BITSOS_ROOT"
RUN="${BITSOS_RUN_DIR:-$(mktemp -d)}"
mkdir -p "$RUN"
for d in /tmp /dev/shm; do
    if [ "$(findmnt -n -o FSTYPE --target "$d")" != tmpfs ]; then
        echo "fs-mmap: $d is not a tmpfs here"; exit 1
    fi
done
cc -O1 -o "$RUN/fs-mmap" host/tests/fs/progs/fs-mmap.c
out="$RUN/mmap-host.txt"
grep '^#' host/tests/fs/mmap-linux.txt > "$out"
for mode in shm order2 private errors maps bus grow; do
    echo "=== $mode" >> "$out"
    set +e
    "$RUN/fs-mmap" "$mode" > "$RUN/stdout" 2>/dev/null
    rc=$?
    set -e
    if [ "$rc" -gt 128 ]; then echo "end: signal $((rc - 128))" >> "$out"; else echo "end: exit $rc" >> "$out"; fi
    cat "$RUN/stdout" >> "$out"
done
if diff -u host/tests/fs/mmap-linux.txt "$out"; then
    echo "fs-mmap: 7 modes the same as host/tests/fs/mmap-linux.txt"
else
    echo "fs-mmap: Linux answers differently from host/tests/fs/mmap-linux.txt"; exit 1
fi
