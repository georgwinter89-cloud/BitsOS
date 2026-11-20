#!/bin/bash
# slow
# PK-6f on V2 (QEMU, CONFIG.TXT mcp=both, mcp_confirmer=test): bitspkgd, started by init after settingsd, on
# /run/bitsos/pkg.sock and as MCP tools. The fixture mirror (fixture-mirror.sh: built here in the run folder from the
# build host's pacman cache, read-only, never in git) is served by python's http.server on 127.0.0.1 at the lane's
# port + 99, the slow mirror (pk_slow_mirror.py: 20 s, then 404) at + 100; the guest reaches both as 10.0.2.2 (R9: no
# real mirror). Root (typed into the console shell) points settingsd's pkg.sources.* keys at the fixture. The first
# refresh is Claude's, over bitsmcp: programs_refresh without a confirmer and after the test confirmer's no is
# refused and nothing is fetched (the mirror's log), after its yes the list is loaded (kind program, PK-FONTS item
# 2). Then root asks the socket: refresh (unchanged), search, plan, do (progress lines in order, ending with done),
# list, remove. uid 1000 (MCP run, not the compositor) may read and plan but do, switch, rollback and refresh are
# refused. Over bitsmcp: programs_search answers without a dialog; program_install without a confirmer is refused;
# with the test confirmer's yes it installs jq (kind program). A refresh hanging on the slow mirror does not block
# `list`. pk_daemon_check.py.
# Ports: kernel MCP + 97, bitsmcp + 98, HTTP + 99, slow mirror + 100.
set -eu -o pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT="$(pwd)"
for t in python3 repo-add bsdtar zstd; do command -v $t >/dev/null || { echo "RED: $t missing on this host (this scenario needs it)"; exit 1; }; done
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/pk-96-bitspkgd}"
# A crashed earlier run may have left its servers: stopped by their PIDs, only if each still is this run's (never by
# pattern).
for f in http slow; do
    if [ -f "$RUN_DIR/$f.pid" ]; then
        old="$(cat "$RUN_DIR/$f.pid")"
        real="$(realpath -m "$RUN_DIR")"
        if [[ "$old" =~ ^[0-9]+$ ]] && [ -r "/proc/$old/cmdline" ] && tr '\0' ' ' < "/proc/$old/cmdline" | grep -qE -- "http.server --bind 127.0.0.1 --directory $real/mirror|pk_slow_mirror.py"; then
            kill "$old" 2>/dev/null || true
            sleep 1
        fi
    fi
done
mkdir -p "$RUN_DIR"
# Everything of an earlier run goes, except the runner's own output.log (cargo xtask scenario writes it into this
# folder before the script starts: removing the folder would lose the reason of a red run).
find "$RUN_DIR" -mindepth 1 -maxdepth 1 ! -name output.log -exec rm -rf {} +
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-18500}"
KFWD=$((LO + 97))
SFWD=$((LO + 98))
HTTP=$((LO + 99))
SLOW=$((LO + 100))
TOKEN="pk6f-scenario-token-3d91"
NAME="pk-daemon"
# shellcheck source=host/tests/mcp/lib.bash
. "$ROOT/host/tests/mcp/lib.bash"

bash host/tests/pk/fixture-mirror.sh "$RUN_DIR/mirror" > "$RUN_DIR/mirror.log" 2>&1 || { cat "$RUN_DIR/mirror.log"; echo "RED: fixture mirror"; exit 1; }
[ -f /usr/share/pacman/keyrings/archlinux.gpg ] || { echo "RED: the build host's pacman keyrings are missing"; exit 1; }
for p in "$KFWD" "$SFWD" "$HTTP" "$SLOW"; do
    python3 -I -c 'import socket,sys; s=socket.socket(); s.settimeout(1); sys.exit(0 if s.connect_ex(("127.0.0.1", int(sys.argv[1]))) else 1)' "$p" \
        || { echo "RED: port $p is in use (lane range $LO)"; exit 1; }
done

QPID=""
SPID=""
WPID=""
cleanup() {
    if [ -n "$QPID" ]; then stop_run "$QPID" "$NAME"; fi
    for p in "$SPID" "$WPID"; do
        if [ -n "$p" ]; then kill "$p" 2>/dev/null || true; wait "$p" 2>/dev/null || true; fi
    done
    rm -f "target/xtask/$NAME.img"
}
trap cleanup EXIT
python3 -I -m http.server --bind 127.0.0.1 --directory "$RUN_DIR/mirror" "$HTTP" > "$RUN_DIR/http.log" 2>&1 &
SPID=$!
echo "$SPID" > "$RUN_DIR/http.pid"
python3 -I "$ROOT/host/tests/pk/pk_slow_mirror.py" "$SLOW" 20 > "$RUN_DIR/slow.log" 2>&1 &
WPID=$!
echo "$WPID" > "$RUN_DIR/slow.pid"

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;mcp=both;mcp_confirmer=test;loglevel=info;nvme_write=off;data_img_mb=0;tz=UTC;screen=1280x800" > "$RUN_DIR/image.out" 2>&1 \
    || { tail -20 "$RUN_DIR/image.out"; echo "RED: image"; exit 1; }
log="$RUN_DIR/serial.log"
qmp="$RUN_DIR/q.qmp"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$KFWD:8765" --hostfwd "$SFWD:8766" --timeout 1200 --serial-log "$log" \
    --smp 4 --mem 4G \
    -- -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 -qmp "unix:$qmp,server=on,wait=off" \
    > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
if ! python3 -I host/tests/pk/pk_daemon_check.py "$KFWD" "$SFWD" "$TOKEN" "$qmp" "$log" "$HTTP" "$SLOW" | tee "$RUN_DIR/check.txt"; then
    grep -a "bitspkg\|pkg\.\|install\|PANIC" "$log" | tail -60
    exit 1
fi
stop_run "$QPID" "$NAME"
QPID=""
if grep -aq "PANIC" "$log"; then echo "RED: the kernel panicked"; tail -20 "$log"; exit 1; fi
if grep -aq "$TOKEN" "$log"; then echo "RED: the token appears in the serial log (R8i)"; exit 1; fi
echo "PK-6f QEMU scenario: green (Claude's first refresh only after the yes, bitspkgd ops against the fixture mirror, progress in order, uid 1000 only reads, program_install refused without a confirmer and installed after the test confirmer's yes, list answers while a refresh hangs on a slow mirror)"
