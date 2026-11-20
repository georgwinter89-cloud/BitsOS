#!/bin/bash
# slow
# PK-6d on V2 (QEMU): programs from the repositories. The fixture mirror (fixture-mirror.sh, built here in the run
# folder from the build host's pacman cache, read-only; never in git) is served by python's http.server on
# 127.0.0.1 at the lane's port + 91, which the guest reaches as 10.0.2.2 (R9: no real mirror). The build host's
# pacman keyrings are copied in over MCP (the image's own copy is a CR to the desk). As root, typed into the console
# shell (downloads drop to nobody, triggers to uid 1000): refresh, install jq (CLI) and foot (GUI) into a scratch
# root under /tmp, a flipped byte and a wrong package signature refused, the space limit (space_max_gb = 0)
# refused before any download. As uid 1000 over MCP: jq through bitsrun; foot's shell in foot's view through
# `bitsrun --exec` (de_DE.UTF-8 from the emulated locale-gen; the system's fonts, which every view shows read-only: no
# font and no fonts.conf is placed by the test, PK-FONTS; QEMU has no desktop for foot's window, Georg 18.11.), its
# output read back. Then removal of both:
# the scratch root is exactly the tree before (PK-2's clean check), `verify --store` clean. pk_install_check.py.
# Ports: MCP at the lane's port + 90, HTTP + 91.
set -eu -o pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT="$(pwd)"
for t in python3 repo-add vercmp bsdtar; do command -v $t >/dev/null || { echo "RED: $t missing on this host (this scenario needs it)"; exit 1; }; done
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/pk-90-install-fixture}"
mkdir -p "$RUN_DIR"
# Everything of an earlier run goes, except the runner's own output.log (cargo xtask scenario writes it into this
# folder before the script starts: removing the folder would lose the reason of a red run).
find "$RUN_DIR" -mindepth 1 -maxdepth 1 ! -name output.log -exec rm -rf {} +
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-18500}"
FWD=$((LO + 90))
HTTP=$((LO + 91))
TOKEN="pk6d-scenario-token-3b7e"
NAME="pk-install"
# shellcheck source=host/tests/mcp/lib.bash
. "$ROOT/host/tests/mcp/lib.bash"

bash host/tests/pk/fixture-mirror.sh "$RUN_DIR/mirror" > "$RUN_DIR/mirror.log" 2>&1 || { cat "$RUN_DIR/mirror.log"; echo "RED: fixture mirror"; exit 1; }
cat "$RUN_DIR/mirror.log"
[ -f /usr/share/pacman/keyrings/archlinux.gpg ] && [ -f /usr/share/pacman/keyrings/cachyos.gpg ] || { echo "RED: the build host's pacman keyrings are missing"; exit 1; }
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
if ! python3 -I host/tests/pk/pk_install_check.py "$FWD" "$TOKEN" "$qmp" "$log" "$HTTP" | tee "$RUN_DIR/check.txt"; then
    grep -a "bitspkg\|bitsrun\|PANIC" "$log" | tail -40
    exit 1
fi
stop_run "$QPID" "$NAME"
QPID=""
if grep -aq "PANIC" "$log"; then echo "RED: the kernel panicked"; tail -20 "$log"; exit 1; fi
if grep -aq "$TOKEN" "$log"; then echo "RED: the token appears in the serial log (R8i)"; exit 1; fi
echo "PK-6d QEMU scenario: green (jq and foot from the fixture mirror, run through bitsrun, refusals, removal leaves the tree as before)"
