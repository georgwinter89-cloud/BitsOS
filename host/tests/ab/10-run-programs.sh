#!/bin/bash
# slow
# AB-2: programs started with the `run` MCP tool from the host (QEMU hostfwd in AB's port range), compared with
# the same binaries run on this CachyOS host: a static-PIE C program that prints its argv/envp/auxv in a
# machine-independent form, a C++ program on UL-1's libc++, stdin/stderr through run, a faulting program
# (SIGSEGV + the V2 message), Rust std hello (static-PIE) and xtask's musl C hello (static ET_EXEC).
# The C/C++ programs are built with UL-1's wrappers (user/sysroot/bin/v2-cc, v2-c++); the sysroot comes from
# $V2_SYSROOT, else user/sysroot/out/sysroot (built with user/sysroot/build.sh when its sources exist).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
export BITSOS_ROOT="$(pwd)"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/ab-10-run-programs}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/ab"
LO="${BITSOS_PORT_LO:-23500}"
FWD=$((LO + 100))
TOKEN="ab2-scenario-token-5c1e"
NAME="ab-run"
LOG="$RUN_DIR/serial.log"
PROGS="$RUN_DIR/progs"
mkdir -p "$PROGS"

if ! grep -q "net::init" kernel/src/main.rs; then
    echo "SKIP: kernel/src/main.rs lacks net::init; MCP is not reachable from the host"
    exit 0
fi

# 1. Test programs (host builds; the same files go into the initramfs).
SYSROOT="${V2_SYSROOT:-$(pwd)/user/sysroot/out/sysroot}"
if [ ! -f "$SYSROOT/usr/lib/libc.a" ] && [ -x user/sysroot/build.sh ]; then
    echo "building UL-1's sysroot (user/sysroot/build.sh)"
    user/sysroot/build.sh > "$RUN_DIR/sysroot-build.log" 2>&1 || { echo "sysroot build failed, see $RUN_DIR/sysroot-build.log"; tail -5 "$RUN_DIR/sysroot-build.log"; }
fi
ADD=()
if [ -f "$SYSROOT/usr/lib/libc.a" ]; then
    export V2_SYSROOT="$SYSROOT"
    user/sysroot/bin/v2-cc -O2 -Wall -Werror -o "$PROGS/ab-auxv" "$HERE/progs/auxv.c"
    user/sysroot/bin/v2-cc -O2 -Wall -Werror -o "$PROGS/ab-stdin" "$HERE/progs/stdin.c"
    user/sysroot/bin/v2-cc -O2 -o "$PROGS/ab-fault" "$HERE/progs/fault.c"
    user/sysroot/bin/v2-c++ -O2 -std=c++20 -Wall -Werror -o "$PROGS/ab-cxx" "$HERE/progs/cxx.cpp"
    for p in ab-auxv ab-stdin ab-fault ab-cxx; do
        file "$PROGS/$p" | grep -q "static-pie linked" || { echo "RED: $p is not static-pie"; file "$PROGS/$p"; exit 1; }
        ADD+=("bin/$p=$PROGS/$p")
    done
else
    echo "RED: no musl sysroot (set V2_SYSROOT or build user/sysroot/out with user/sysroot/build.sh)"
    exit 1
fi

# 2. Image with MCP on, the test programs added to its initramfs.
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
IMG="target/xtask/$NAME.img"
INITRD=$(mdir -b -i "$IMG" ::/BITSOS2/SYS/ | head -1)
mcopy -o -i "$IMG" "${INITRD}/INITRD" "$RUN_DIR/initrd.orig"
python3 -I "$HERE/cpio_add.py" "$RUN_DIR/initrd.orig" "$RUN_DIR/initrd.new" "${ADD[@]}"
mcopy -o -i "$IMG" "$RUN_DIR/initrd.new" "${INITRD}/INITRD"

# 3. Boot and compare. A listener already on the port would answer for this boot (a VM left from an earlier
# run): refuse instead of testing someone else's kernel.
if ss -ltn 2>/dev/null | grep -q "127.0.0.1:$FWD "; then
    echo "RED: port $FWD is already in use (a VM left from an earlier run?); stop it by its pid first"
    ss -ltnp 2>/dev/null | grep "127.0.0.1:$FWD " || true
    exit 1
fi
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 300 --serial-log "$LOG" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
# Ending xtask alone can leave its QEMU child running; stop the QEMU pids xtask printed too (only ours).
cleanup() {
    for q in $(sed -n 's/^xtask: qemu pid \([0-9]*\) .*/\1/p' "$RUN_DIR/qemu.out" 2>/dev/null); do
        if tr '\0' ' ' < "/proc/$q/cmdline" 2>/dev/null | grep -q -- "-name v2-ab-$NAME "; then kill "$q" 2>/dev/null || true; fi
    done
    if kill -0 "$QPID" 2>/dev/null; then kill "$QPID" 2>/dev/null || true; wait "$QPID" 2>/dev/null || true; fi
}
trap cleanup EXIT

python3 -I "$HERE/run_client.py" "$FWD" "$TOKEN" "$PROGS"
if [ ! -s "$LOG" ] || grep -q "Could not set up host forwarding" "$RUN_DIR/qemu.out"; then
    echo "RED: this run's VM did not boot (no serial log or no port forward), so the answers came from elsewhere"
    cat "$RUN_DIR/qemu.out"
    exit 1
fi
grep -a "proc fault" "$LOG" | tail -3 || true
if grep -aq "$TOKEN" "$LOG"; then echo "RED: the token appears in the log (R8i)"; exit 1; fi
if grep -aq "KTEST PANIC\|panicked at" "$LOG"; then echo "RED: kernel panic"; grep -a "PANIC\|panicked" "$LOG" | head; exit 1; fi
echo "ab run programs: green"
