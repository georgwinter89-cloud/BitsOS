#!/bin/bash
# FS-8c: checks host/tests/fs/data-linux.txt against Linux itself. Builds host/tests/fs/progs/fs-data.c for this
# machine and runs it in a fresh directory of the run directory's file system (CachyOS: btrfs; the probes are
# VFS-level: flock, fcntl locks, symlinks, rename over, fsync, fallocate, utimensat, statfs, the same on ext4).
# On V2 host/tests/fs/50-data.sh runs /bin/fs-data in /data (ext4 on the bitsvol volume) over MCP and compares it
# with the same file.
set -eu
cd "$BITSOS_ROOT"
RUN="${BITSOS_RUN_DIR:-$(mktemp -d)}"
mkdir -p "$RUN"
cc -O1 -Wall -Werror -o "$RUN/fs-data" host/tests/fs/progs/fs-data.c
out="$RUN/data-host.txt"
grep '^#' host/tests/fs/data-linux.txt > "$out"
rm -rf "$RUN/probe"
set +e
"$RUN/fs-data" "$RUN/probe" > "$RUN/stdout" 2>/dev/null
rc=$?
set -e
if [ "$rc" -gt 128 ]; then echo "end: signal $((rc - 128))" >> "$out"; else echo "end: exit $rc" >> "$out"; fi
cat "$RUN/stdout" >> "$out"
if diff -u host/tests/fs/data-linux.txt "$out"; then
    echo "fs-data: Linux answers as host/tests/fs/data-linux.txt"
else
    echo "fs-data: Linux answers differently from host/tests/fs/data-linux.txt"; exit 1
fi
