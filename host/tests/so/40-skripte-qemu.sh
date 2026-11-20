#!/bin/bash
# slow
# EINS-D on V2 (QEMU): the set "skripte"'s Python from the repositories, run as a command. The fixture mirror
# (skripte-mirror.sh: python's closure from the build host's pacman cache, read only; never in git) is served on
# 127.0.0.1 at the lane's port + 41 (the guest's 10.0.2.2; R9: no real mirror). Root (typed into the console shell)
# refreshes and installs python into a scratch root under /tmp; uid 1000 (MCP) runs /bin/python3, the image's link
# to bits, in its command view: ssl, sqlite3, zlib, ctypes, curses, readline, lzma, bz2, decimal and the de_DE
# locale of the closure's glibc; the shared /tmp and the home; the module bitsos calling bits inside the view.
# skripte_check.py. Ports: MCP at the lane's port + 40, HTTP + 41.
set -eu -o pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT="$(pwd)"
for t in python3 repo-add vercmp bsdtar; do command -v $t >/dev/null || { echo "RED: $t missing on this host (this scenario needs it)"; exit 1; }; done
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/so-40-skripte-qemu}"
mkdir -p "$RUN_DIR"
find "$RUN_DIR" -mindepth 1 -maxdepth 1 ! -name output.log -exec rm -rf {} +
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-10500}"
FWD=$((LO + 40))
HTTP=$((LO + 41))
TOKEN="einsd-scenario-token-5c2a"
NAME="${BITSOS_LANE:-so}-skripte"
# shellcheck source=host/tests/mcp/lib.bash
. "$ROOT/host/tests/mcp/lib.bash"

bash host/tests/so/skripte-mirror.sh "$RUN_DIR/mirror" > "$RUN_DIR/mirror.log" 2>&1 || { cat "$RUN_DIR/mirror.log"; echo "RED: fixture mirror"; exit 1; }
cat "$RUN_DIR/mirror.log"
[ -f /usr/share/pacman/keyrings/archlinux.gpg ] && [ -f /usr/share/pacman/keyrings/cachyos.gpg ] || { echo "RED: the build host's pacman keyrings are missing (install archlinux-keyring, cachyos-keyring)"; exit 1; }
for p in "$FWD" "$HTTP"; do
    python3 -I -c 'import socket,sys; s=socket.socket(); s.settimeout(1); sys.exit(0 if s.connect_ex(("127.0.0.1", int(sys.argv[1]))) else 1)' "$p" \
        || { echo "RED: port $p is in use (lane range $LO)"; exit 1; }
done

QPID=""
SPID=""
cleanup() {
    if [ -n "$QPID" ]; then stop_run "$QPID" "$NAME"; fi
    if [ -n "$SPID" ]; then kill "$SPID" 2>/dev/null || true; wait "$SPID" 2>/dev/null || true; fi
    rm -f "target/xtask/$NAME.img"
}
trap cleanup EXIT
python3 -I -m http.server --bind 127.0.0.1 --directory "$RUN_DIR/mirror" "$HTTP" > "$RUN_DIR/http.log" 2>&1 &
SPID=$!

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;nvme_write=off;data_img_mb=0;tz=UTC;screen=1280x800" > "$RUN_DIR/image.out" 2>&1 \
    || { tail -20 "$RUN_DIR/image.out"; echo "RED: image"; exit 1; }
log="$RUN_DIR/serial.log"
qmp="$RUN_DIR/q.qmp"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 1200 --serial-log "$log" \
    --smp 4 --mem 4G \
    -- -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 -qmp "unix:$qmp,server=on,wait=off" \
    > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
if ! python3 -I host/tests/so/skripte_check.py "$FWD" "$TOKEN" "$qmp" "$log" "$HTTP" | tee "$RUN_DIR/check.txt"; then
    grep -a "bitspkg\|bitsrun\|bits:\|PANIC" "$log" | tail -40
    exit 1
fi
stop_run "$QPID" "$NAME"
QPID=""
if grep -aq "PANIC" "$log"; then echo "RED: the kernel panicked"; tail -20 "$log"; exit 1; fi
if grep -aq "$TOKEN" "$log"; then echo "RED: the token appears in the serial log (R8i)"; exit 1; fi
echo "GREEN: 40-skripte-qemu (python from the fixture mirror, run as /bin/python3 through bits's command view)"
