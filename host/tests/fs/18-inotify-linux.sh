#!/bin/bash
# FS-14: checks host/tests/fs/inotify-linux.txt against Linux itself. Builds host/tests/fs/progs/fs-inotify.c for
# this machine, runs the modes checks, dir, self and wait on CachyOS (as a normal user) and compares the ends and
# the output with the file. On V2 ktest fs.inotify.programs runs every mode and compares it with its block;
# `cargo xtask abi-diff abi-inotify` compares them too (user/abi-tests/c/inotify.c is the same file).
set -eu
cd "$BITSOS_ROOT"
RUN="${BITSOS_RUN_DIR:-$(mktemp -d)}"
mkdir -p "$RUN"
cc -O1 -o "$RUN/fs-inotify" host/tests/fs/progs/fs-inotify.c
out="$RUN/inotify-host.txt"
grep '^#' host/tests/fs/inotify-linux.txt > "$out"
for mode in checks dir self wait; do
    echo "=== $mode" >> "$out"
    set +e
    (cd / && "$RUN/fs-inotify" "$mode") > "$RUN/stdout" 2>/dev/null
    rc=$?
    set -e
    if [ "$rc" -gt 128 ]; then echo "end: signal $((rc - 128))" >> "$out"; else echo "end: exit $rc" >> "$out"; fi
    cat "$RUN/stdout" >> "$out"
done
if diff -u host/tests/fs/inotify-linux.txt "$out"; then
    echo "fs-inotify: 4 modes the same as host/tests/fs/inotify-linux.txt"
else
    echo "fs-inotify: Linux answers differently from host/tests/fs/inotify-linux.txt"; exit 1
fi
