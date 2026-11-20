#!/bin/bash
# slow
# PK-6e on V2 (QEMU): `bitspkg update`. The fixture mirror (fixture-mirror.sh, then fixture-update.sh: built here in
# the run folder from the build host's pacman cache, read-only, never in git; the update states signed by TEST keys
# made with the build host's gpg in a scratch home) is served by python's http.server on 127.0.0.1 at the lane's
# port + 96, which the guest reaches as 10.0.2.2 (R9: no real mirror). The build host's pacman keyrings and the
# TEST keyring (key A) are copied in over MCP as the image's keyring. As root, typed into the console shell
# (downloads drop to nobody, triggers to uid 1000), into a scratch root under /tmp: jq installed from state a;
# `update --check` against state b reports the keyring package and "1 Teil neuer" and changes nothing; `update`
# installs the keyring and mirror list packages (checked with the old keyring), then jq's new version with only
# oniguruma fetched (checked with the new keyring, which brings key B); the installed mirror list serves a refresh;
# rollback shows "1 Teil älter"; removing the old version takes the old oniguruma's .pkgs entry (gc); state c's
# keyring package signed by an unknown key is refused and the keyring stays; state a's older database stays
# refused on the update path; removal leaves no .pkgs entry. pk_update_check.py. Ports: MCP + 95, HTTP + 96.
set -eu -o pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT="$(pwd)"
for t in python3 repo-add vercmp bsdtar gpg gpgconf zstd; do command -v $t >/dev/null || { echo "RED: $t missing on this host (this scenario needs it)"; exit 1; }; done
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/pk-95-update-fixture}"
# A crashed earlier run may have left its mirror server: stopped by its PID, only if that PID is still this run's
# http.server on this run's folder (never by pattern).
if [ -f "$RUN_DIR/http.pid" ]; then
    old="$(cat "$RUN_DIR/http.pid")"
    real="$(realpath -m "$RUN_DIR")"
    if [[ "$old" =~ ^[0-9]+$ ]] && [ -r "/proc/$old/cmdline" ] && tr '\0' ' ' < "/proc/$old/cmdline" | grep -qF -- "http.server --bind 127.0.0.1 --directory $real/mirror"; then
        kill "$old" 2>/dev/null || true
        sleep 1
    fi
fi
rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-18500}"
FWD=$((LO + 95))
HTTP=$((LO + 96))
TOKEN="pk6e-scenario-token-81c4"
NAME="pk-update"
# shellcheck source=host/tests/mcp/lib.bash
. "$ROOT/host/tests/mcp/lib.bash"

bash host/tests/pk/fixture-mirror.sh "$RUN_DIR/mirror" > "$RUN_DIR/mirror.log" 2>&1 || { cat "$RUN_DIR/mirror.log"; echo "RED: fixture mirror"; exit 1; }
bash host/tests/pk/fixture-update.sh "$RUN_DIR/mirror" "http://10.0.2.2:$HTTP" >> "$RUN_DIR/mirror.log" 2>&1 || { cat "$RUN_DIR/mirror.log"; echo "RED: fixture update states"; exit 1; }
cat "$RUN_DIR/mirror.log"
[ -f /usr/share/pacman/keyrings/archlinux.gpg ] || { echo "RED: the build host's pacman keyrings are missing"; exit 1; }
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
echo "$SPID" > "$RUN_DIR/http.pid"

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;nvme_write=off;data_img_mb=0;tz=UTC;screen=1280x800" > "$RUN_DIR/image.out" 2>&1 \
    || { tail -20 "$RUN_DIR/image.out"; echo "RED: image"; exit 1; }
log="$RUN_DIR/serial.log"
qmp="$RUN_DIR/q.qmp"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 1200 --serial-log "$log" \
    --smp 4 --mem 4G \
    -- -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 -qmp "unix:$qmp,server=on,wait=off" \
    > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
if ! python3 -I host/tests/pk/pk_update_check.py "$FWD" "$TOKEN" "$qmp" "$log" "$HTTP" "$RUN_DIR/mirror/upd/keys" | tee "$RUN_DIR/check.txt"; then
    grep -a "bitspkg\|bitsrun\|PANIC" "$log" | tail -40
    exit 1
fi
stop_run "$QPID" "$NAME"
QPID=""
if grep -aq "PANIC" "$log"; then echo "RED: the kernel panicked"; tail -20 "$log"; exit 1; fi
if grep -aq "$TOKEN" "$log"; then echo "RED: the token appears in the serial log (R8i)"; exit 1; fi
echo "PK-6e QEMU scenario: green (update --check, keyring and mirror list packages, jq's new version with only oniguruma fetched, rollback, gc, a bad keyring package refused, an older database refused)"
