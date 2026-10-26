#!/bin/bash
# FS-10: checks host/tests/fs/memfd-linux.txt against Linux itself. Builds host/tests/fs/progs/fs-memfd.c for this
# machine, runs the modes basic, wl-fork and wl-socket on CachyOS (as a normal user) and compares the ends and the
# output with the file. On V2 ktest fs.memfd.programs runs `basic` and fs.memfd.wl_shm runs the wl-client and
# wl-compositor halves (the kernel hands the client's memfd to the compositor, as SCM_RIGHTS would), each half
# compared with the wl-fork block; `cargo xtask abi-diff abi-memfd` compares `basic` (user/abi-tests/c/memfd.c).
set -eu
cd "$BITSOS_ROOT"
RUN="${BITSOS_RUN_DIR:-$(mktemp -d)}"
mkdir -p "$RUN"
if [ "$(findmnt -n -o FSTYPE --target /dev/shm)" != tmpfs ]; then
    echo "fs-memfd: /dev/shm is not a tmpfs here"; exit 1
fi
cc -O1 -o "$RUN/fs-memfd" host/tests/fs/progs/fs-memfd.c
out="$RUN/memfd-host.txt"
grep '^#' host/tests/fs/memfd-linux.txt > "$out"
for mode in basic wl-fork wl-socket; do
    echo "=== $mode" >> "$out"
    set +e
    "$RUN/fs-memfd" "$mode" > "$RUN/stdout" 2>/dev/null
    rc=$?
    set -e
    if [ "$rc" -gt 128 ]; then echo "end: signal $((rc - 128))" >> "$out"; else echo "end: exit $rc" >> "$out"; fi
    cat "$RUN/stdout" >> "$out"
done
if diff -u host/tests/fs/memfd-linux.txt "$out"; then
    echo "fs-memfd: 3 modes the same as host/tests/fs/memfd-linux.txt"
else
    echo "fs-memfd: Linux answers differently from host/tests/fs/memfd-linux.txt"; exit 1
fi
