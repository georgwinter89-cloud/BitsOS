#!/bin/bash
# slow
# BR-9 on V2 (QEMU): Brave from the repositories starts as the stick's launcher starts Brave, proven with a fake
# brave-bin (browser-probe.c against the build host's glibc, as Arch's layout: the binary in /opt/brave-bin, the
# start script in /usr/bin; never the real Brave in QEMU). Over MCP as uid 1000 (pk_browser_check.py): bitsrun starts
# the binary, not the script, with the launcher's flags and environment; Chromium's namespace sandbox calls work
# inside the view (a user namespace nested in bwrap's, PID and network namespaces, chroot); the stick's profile is
# taken over once; a second start reaches the first one's singleton socket (TMPDIR = the runtime folder). The program
# and glibc are copied at test time into the run folder, never into git. Ports: lane range + 97.
set -eu -o pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT="$(pwd)"
for t in python3 cc ldd tar; do command -v $t >/dev/null || { echo "RED: $t missing on this host (this scenario needs it)"; exit 1; }; done
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/pk-97-browser}"
mkdir -p "$RUN_DIR"
find "$RUN_DIR" -mindepth 1 -maxdepth 1 ! -name output.log -exec rm -rf {} +
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-18500}"
FWD=$((LO + 97))
TOKEN="br9-scenario-token-5c1a"
LOG="$RUN_DIR/serial.log"
NAME=br9-browser

(cd user && env -u CARGO_TARGET_DIR -u RUSTUP_TOOLCHAIN cargo build -q --offline --release -p bitspkg --target-dir "$ROOT/target/user-host") \
    > "$RUN_DIR/build.log" 2>&1 || { tail -20 "$RUN_DIR/build.log"; echo "RED: host build"; exit 1; }
B="$ROOT/target/user-host/release/bitspkg"
T="$RUN_DIR/tree"
mkdir -p "$T/root/opt/brave-bin" "$T/root/usr/bin" "$T/root/usr/lib" "$T/root/etc" "$RUN_DIR/out"
cc -O2 -Wall -Wextra -Werror -o "$T/root/opt/brave-bin/brave" host/tests/pk/browser-probe.c
cp "$T/root/opt/brave-bin/brave" "$T/root/usr/bin/brave"
ldd "$T/root/opt/brave-bin/brave" | awk '/=> \//{print $3} /^\t\/.*ld-linux/{print $1}' | while read -r l; do
    cp -L "$l" "$T/root/usr/lib/$(basename "$l")"
done
[ -f "$T/root/usr/lib/ld-linux-x86-64.so.2" ] && [ -f "$T/root/usr/lib/libc.so.6" ] || { ls "$T/root/usr/lib"; echo "RED: glibc not copied"; exit 1; }
echo "fake" > "$T/root/etc/brave.conf"
cat > "$T/package.toml" <<'TOML'
format = 1
name = "brave-bin"
version = "1"
en = "BR-9 fake Brave"
program = "root/usr/bin/brave"

[[permission]]
kind = "path"
path = "/tmp/pkb/home/Downloads"
write = true
create = true
why = "BR-9 test: Brave's downloads"
TOML
"$B" build "$T" --out "$RUN_DIR/out/pkg" > "$RUN_DIR/pkg-build.txt"
tar -cf "$RUN_DIR/pkg.tar" -C "$RUN_DIR/out" pkg

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;nvme_write=off" > "$RUN_DIR/image.log" 2>&1 \
    || { tail -30 "$RUN_DIR/image.log"; echo "RED: image"; exit 1; }
python3 -I -c 'import socket,sys; s=socket.socket(); s.settimeout(1); sys.exit(0 if s.connect_ex(("127.0.0.1", int(sys.argv[1]))) else 1)' "$FWD" \
    || { echo "RED: port $FWD is in use (lane range $LO + 97)"; exit 1; }
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
    rm -f "target/xtask/$NAME.img"
}
trap stop EXIT
python3 -I host/tests/pk/pk_browser_check.py "$FWD" "$TOKEN" "$RUN_DIR/pkg.tar"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "BR-9 QEMU scenario: green"
