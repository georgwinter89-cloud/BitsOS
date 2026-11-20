#!/bin/bash
# slow
# PK-6a on V2 (QEMU): `bitspkg refresh` against the fixture mirror (fixture-mirror.sh, built here in target/ from
# the build host's pacman cache) served by python's http.server on 127.0.0.1 at the lane's port + 71, which the
# guest reaches as 10.0.2.2 over QEMU's user network (R9: no real mirror). As root (typed into the console shell,
# so the download really drops to nobody and root takes the files over) into a scratch root under /tmp:
#   - fixture (unsigned database, allowed for it) is loaded; `search jq` finds it, `info --repo jq` describes it;
#   - bad (a broken database signature) is refused with its reason; until PK-6d wires PK-6b's verifier that reason
#     is "Signaturprüfung noch nicht eingebaut", the real verdict on this signature is PK-6d's QEMU run;
#   - nosig (signature required, none on the mirror) is refused: "fehlt";
#   - an older database from a second mirror path is refused ("älter"), the loaded one stays; with both paths the
#     fallback reaches the good one;
#   - the sync folder holds only root's files, no download folder is left.
# Also as uid 1000 over MCP: `bitspkg sources` with settingsd (D1's six sources, nothing fetched: D5) and vercmp.
# pk_refresh_check.py. Ports: MCP at the lane's port + 70, HTTP + 71.
set -eu -o pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT="$(pwd)"
for t in python3 repo-add vercmp bsdtar; do command -v $t >/dev/null || { echo "RED: $t missing on this host (this scenario needs it)"; exit 1; }; done
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/pk-70-refresh-fixture}"
rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-18500}"
FWD=$((LO + 70))
HTTP=$((LO + 71))
TOKEN="pk6a-scenario-token-91c4"
NAME="pk-refresh"
# shellcheck source=host/tests/mcp/lib.bash
. "$ROOT/host/tests/mcp/lib.bash"

bash host/tests/pk/fixture-mirror.sh "$RUN_DIR/mirror" > "$RUN_DIR/mirror.log" 2>&1 || { cat "$RUN_DIR/mirror.log"; echo "RED: fixture mirror"; exit 1; }
cat "$RUN_DIR/mirror.log"

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

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;nvme_write=off;data_img_mb=0;tz=UTC" > "$RUN_DIR/image.out" 2>&1 \
    || { tail -20 "$RUN_DIR/image.out"; echo "RED: image"; exit 1; }
log="$RUN_DIR/serial.log"
qmp="$RUN_DIR/q.qmp"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 420 --serial-log "$log" \
    -- -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 -qmp "unix:$qmp,server=on,wait=off" \
    > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
if ! python3 -I host/tests/pk/pk_refresh_check.py "$FWD" "$TOKEN" "$qmp" "$log" "$HTTP" | tee "$RUN_DIR/check.txt"; then
    grep -a "bitspkg\|PANIC" "$log" | tail -30
    exit 1
fi
stop_run "$QPID" "$NAME"
QPID=""
if grep -aq "PANIC" "$log"; then echo "RED: the kernel panicked"; tail -20 "$log"; exit 1; fi
if grep -aq "$TOKEN" "$log"; then echo "RED: the token appears in the serial log (R8i)"; exit 1; fi
echo "PK-6a QEMU scenario: green (refresh from the fixture mirror as root through nobody, search, bad and unsigned refused, older database refused)"
