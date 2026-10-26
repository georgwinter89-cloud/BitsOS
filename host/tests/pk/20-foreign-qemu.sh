#!/bin/bash
# slow
# PK-2 + PK-4 on V2 (QEMU): a normal boot with MCP (/bin/bitspkg of this tree is in the image); over MCP as
# uid 1000: `bitspkg selftest` (PK-2's removal without leftovers with a private settingsd and a program that
# tries to write elsewhere), then CachyOS's dynamic jq imported on the host (`bitspkg import`, layouts libpath
# and view) installs, runs from its package and removes without leftovers (pk_qemu_check.py). Ports: lane range + 20.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT="$(pwd)"
for t in python3 ldd; do command -v $t >/dev/null || { echo "SKIP: $t missing"; exit 0; }; done
[ -x /usr/bin/jq ] || { echo "SKIP: no /usr/bin/jq on this host"; exit 0; }
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/pk-20-foreign-qemu}"
rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-18500}"
FWD=$((LO + 20))
TOKEN="pk-scenario-token-2c4a"
LOG="$RUN_DIR/serial.log"
NAME=pk-foreign

(cd user && env -u CARGO_TARGET_DIR -u RUSTUP_TOOLCHAIN cargo build -q --offline --release -p bitspkg --target-dir "$ROOT/target/user-host") \
    > "$RUN_DIR/build.log" 2>&1 || { tail -20 "$RUN_DIR/build.log"; echo "RED: host build"; exit 1; }
B="$ROOT/target/user-host/release/bitspkg"
mkdir -p "$RUN_DIR/src/bin" "$RUN_DIR/out"
cp /usr/bin/jq "$RUN_DIR/src/bin/jq"
for n in libpath view; do
    "$B" import "$RUN_DIR/src" "$RUN_DIR/tree-$n" --system user/packaging/libset.txt --layout $n --launcher bin/jq > "$RUN_DIR/import-$n.txt"
    printf 'format = 1\nname = "jq-%s"\nversion = "1"\nprogram = "bitsos/run"\n' $n > "$RUN_DIR/tree-$n/package.toml"
    "$B" build "$RUN_DIR/tree-$n" --out "$RUN_DIR/out/pkg-$n" > /dev/null
done
tar -cf "$RUN_DIR/pkgs.tar" -C "$RUN_DIR/out" pkg-libpath pkg-view

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;wlan=off;nvme_write=off"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 500 --serial-log "$LOG" \
    > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
stop() {
    kill "$QPID" 2>/dev/null || true
    pf="target/xtask/qemu-$NAME.pid"
    if [ -f "$pf" ]; then
        pid="$(head -1 "$pf")"
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then kill "$pid" 2>/dev/null || true; fi
    fi
    wait "$QPID" 2>/dev/null || true
}
trap stop EXIT
python3 -I host/tests/pk/pk_qemu_check.py "$FWD" "$TOKEN" "$RUN_DIR/pkgs.tar"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "PK-2/PK-4 QEMU scenario: green"
