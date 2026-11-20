#!/bin/bash
# slow
# PK-3 on V2 (QEMU): a normal boot with MCP (/bin/bitsrun, /bin/bitspkg and /bin/bwrap of this tree in the image);
# a test package built here (bitsrun-probe.c against the build host's glibc, dynamic, with its libc and ld.so as a
# per-program root tree root/usr + root/etc) installs as uid 1000 and starts through bitsrun (pk_bitsrun_check.py).
# The program and glibc are copied at test time into the run folder, never into git. Ports: lane range + 80.
set -eu -o pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT="$(pwd)"
for t in python3 cc ldd tar; do command -v $t >/dev/null || { echo "RED: $t missing on this host (this scenario needs it)"; exit 1; }; done
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/pk-80-bitsrun}"
rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-18500}"
FWD=$((LO + 80))
TOKEN="pk3-scenario-token-8d2f"
LOG="$RUN_DIR/serial.log"
NAME=pk-bitsrun

(cd user && env -u CARGO_TARGET_DIR -u RUSTUP_TOOLCHAIN cargo build -q --offline --release -p bitspkg --target-dir "$ROOT/target/user-host") \
    > "$RUN_DIR/build.log" 2>&1 || { tail -20 "$RUN_DIR/build.log"; echo "RED: host build"; exit 1; }
B="$ROOT/target/user-host/release/bitspkg"
T="$RUN_DIR/tree"
mkdir -p "$T/root/usr/bin" "$T/root/usr/lib" "$T/root/etc" "$RUN_DIR/out"
cc -O2 -Wall -Wextra -Werror -o "$T/root/usr/bin/probe" host/tests/pk/bitsrun-probe.c
# The libraries the build host's loader names (a plain file name each) and the loader itself, as the tree's own.
ldd "$T/root/usr/bin/probe" | awk '/=> \//{print $3} /^\t\/.*ld-linux/{print $1}' | while read -r l; do
    cp -L "$l" "$T/root/usr/lib/$(basename "$l")"
done
[ -f "$T/root/usr/lib/ld-linux-x86-64.so.2" ] && [ -f "$T/root/usr/lib/libc.so.6" ] || { ls "$T/root/usr/lib"; echo "RED: glibc not copied"; exit 1; }
echo "probe-conf-from-tree" > "$T/root/etc/probe.conf"
echo "treeuser:x:1000:1000::/nowhere:/bin/false" > "$T/root/etc/passwd"
cat > "$T/package.toml" <<'TOML'
format = 1
name = "probe"
version = "1"
en = "PK-3 test program"
program = "root/usr/bin/probe"

[[permission]]
kind = "path"
path = "/tmp/pkr/home/Downloads"
write = true
create = false
why = "PK-3 test: a wanted user folder, writable"

[[permission]]
kind = "path"
path = "/tmp/pkr/home/Documents"
write = false
create = false
why = "PK-3 test: a wanted user folder, read-only"
TOML
"$B" build "$T" --out "$RUN_DIR/out/pkg" > "$RUN_DIR/pkg-build.txt"
tar -cf "$RUN_DIR/pkg.tar" -C "$RUN_DIR/out" pkg

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;nvme_write=off" > "$RUN_DIR/image.log" 2>&1 \
    || { tail -30 "$RUN_DIR/image.log"; echo "RED: image"; exit 1; }
# Something already answering on the forward port would be asked instead of this QEMU.
python3 -I -c 'import socket,sys; s=socket.socket(); s.settimeout(1); sys.exit(0 if s.connect_ex(("127.0.0.1", int(sys.argv[1]))) else 1)' "$FWD" \
    || { echo "RED: port $FWD is in use (lane range $LO + 80)"; exit 1; }
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
python3 -I host/tests/pk/pk_bitsrun_check.py "$FWD" "$TOKEN" "$RUN_DIR/pkg.tar"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "PK-3 QEMU scenario: green"
