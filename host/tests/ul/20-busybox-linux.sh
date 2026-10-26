#!/bin/bash
# UL-3a: BusyBox on CachyOS, the oracle for the ul.bb.* ktests. Builds user/ports/busybox (offline: the one
# tarball must be in the download cache, see user/ports/README.md) and runs every case in
# user/ports/busybox/tests/ against its <case>.out transcript (tests/run-host.sh). No tarball or no sysroot: SKIP.
set -eu
BITSOS_ROOT="${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
P="$BITSOS_ROOT/user/ports/busybox"
if [ ! -f "${V2_SYSROOT:-$BITSOS_ROOT/user/sysroot/out/sysroot}/usr/lib/libc.a" ]; then
    echo "SKIP: no musl sysroot (user/sysroot/build.sh)"
    exit 0
fi
if ! out=$(V2_OFFLINE=1 sh "$P/build.sh" 2>&1); then
    echo "$out" | tail -5
    if echo "$out" | grep -q "V2_OFFLINE=1"; then
        echo "SKIP: busybox tarball not in the download cache (run user/ports/busybox/build.sh once)"
        exit 0
    fi
    exit 1
fi
echo "$out" | tail -1
sh "$P/tests/run-host.sh"
echo "busybox on CachyOS: green"
