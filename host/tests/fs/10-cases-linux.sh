#!/bin/bash
# FS-1: checks host/tests/fs/cases.txt against Linux itself. Builds the case runner
# host/tests/fs/progs/fs-cases.c for this machine and runs it on a tmpfs (/tmp on CachyOS), as a normal user.
# The same list runs on BitsOS V2 (ktest fs.calls.linux_cases) and against the VFS on the host (cargo test -p vfs
# sys_cases), so a green run here means all three agree with Linux.
set -eu
cd "$BITSOS_ROOT"
RUN="${BITSOS_RUN_DIR:-$(mktemp -d)}"
mkdir -p "$RUN"
if [ "$(id -u)" = 0 ]; then
    echo "fs-cases: run as a normal user (the permission cases assume no CAP_DAC_OVERRIDE)"; exit 1
fi
if [ "$(findmnt -n -o FSTYPE --target /tmp)" != tmpfs ]; then
    echo "fs-cases: /tmp is not a tmpfs here (directory sizes in the list are tmpfs's)"; exit 1
fi
cc -O1 -o "$RUN/fs-cases" host/tests/fs/progs/fs-cases.c
base="$(mktemp -u /tmp/fs-cases-linux.XXXXXX)"
set +e
"$RUN/fs-cases" host/tests/fs/cases.txt "$base" | tee "$RUN/fs-cases.out"
rc=${PIPESTATUS[0]}
set -e
rm -rf "$base"
exit "$rc"
