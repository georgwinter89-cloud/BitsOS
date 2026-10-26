#!/bin/bash
# FS-2: checks host/tests/fs/dev-cases.txt (/dev, /proc, /sys) against Linux itself, with the same runner as
# 10-cases-linux.sh. The list runs on BitsOS V2 too (ktest fs.calls.dev_cases) and, without the lines that need a
# process, against the VFS on the host (cargo test -p vfs dev_cases).
set -eu
cd "${BITSOS_ROOT:-$(dirname "$0")/../../..}"
RUN="${BITSOS_RUN_DIR:-$(mktemp -d)}"
mkdir -p "$RUN"
if [ "$(id -u)" = 0 ]; then
    echo "dev-cases: run as a normal user (the permission cases assume no CAP_DAC_OVERRIDE)"; exit 1
fi
# The runner's name matters: /proc/self/stat and status show it as "fs-cases".
cc -O1 -o "$RUN/fs-cases" host/tests/fs/progs/fs-cases.c
base="$(mktemp -u /tmp/dev-cases-linux.XXXXXX)"
set +e
"$RUN/fs-cases" host/tests/fs/dev-cases.txt "$base" | tee "$RUN/dev-cases.out"
rc=${PIPESTATUS[0]}
set -e
rm -rf "$base"
exit "$rc"
