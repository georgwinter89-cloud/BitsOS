#!/bin/bash
# AB-14: the vDSO image crates/proc builds, run on this host with a hand-filled vvar page
# (host/tests/ab/progs/vdso_host.c): exact answers on a made-up clock line, the syscall fallbacks, getcpu,
# a TSC behind the base, and a seqlock torture with 8 reader threads. No QEMU, a few seconds.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/ab-20-vdso-host}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
command -v gcc >/dev/null || { echo "SKIP: no gcc on this host"; exit 0; }
# The build script's output directory (cargo prints it for proc's build script).
OUT=$(cargo build -q -p proc --message-format=json 2>/dev/null | python3 -I -c '
import json, sys
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("reason") == "build-script-executed" and "/crates/proc#" in m.get("package_id", "") + "#" and m.get("out_dir"):
        print(m["out_dir"])
' | tail -n 1)
if [ -z "$OUT" ]; then
    OUT=$(cargo build -q -p proc --message-format=json 2>/dev/null | grep -o '"out_dir":"[^"]*proc-[^"]*"' | tail -n 1 | sed 's/"out_dir":"\(.*\)"/\1/')
fi
[ -f "$OUT/vdso.so" ] || { echo "FAIL: no vdso.so from crates/proc's build script (out_dir '$OUT')"; exit 1; }
# The page layout from the one definition both sides use.
sed -n 's/^pub const \([A-Z_]*\): [a-z0-9]* = \(.*\);$/#define \1 (\2)/p' crates/proc/src/vdso/layout.rs > "$RUN_DIR/layout.h"
gcc -O2 -Wall -Werror -std=gnu11 -pthread -I"$RUN_DIR" -Iuser/abi-tests/c -o "$RUN_DIR/vdso_host" host/tests/ab/progs/vdso_host.c
"$RUN_DIR/vdso_host" "$OUT/vdso.so" | tee "$RUN_DIR/vdso_host.txt"
if grep -q ': no$' "$RUN_DIR/vdso_host.txt"; then
    echo "FAIL: a check said no"
    exit 1
fi
echo "PASS: vdso host checks"
