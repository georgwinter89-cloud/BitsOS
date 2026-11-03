#!/bin/bash
# slow
# MC-8a: MCP as the user-space service bitsmcp (init starts it under CONFIG.TXT mcp=both|service).
#   boot 1, mcp=both: the kernel's server on 8765 and the service on 8766 (hostfwd LO+100, LO+101): tools/list
#     entries and every offered tool's answer equal the kernel server's (service_check.py), dropped tools are
#     refused with their reason, installer tools answer EPERM, the service runs as uid 961 under seccomp; the
#     service is killed by pid (the kernel's kill), the kernel answers on, init starts it again, the console
#     mirror keeps sending (UDP listener on LO+160), every call is a kernel event with tool, peer and result that
#     bitsjournal keeps (found with events since=1 through both servers); `bitsmcp filter-probe` (MCP run) shows
#     the service's seccomp filter on V2: open and socket EPERM, a thread allowed, execve and fork end it (SIGSYS);
#   boot 2, mcp=service: the kernel server is not started, the service answers on 8765 (MC-10: a token over plain
#     HTTP gets 403 there, TLS only after a pairing at the keyboard: 95-access.sh), the mirror sends; the console
#     shell (typed on through QEMU's USB keyboard and QMP) kills the service, init starts it again.
# Needs init's glue (plan/notes/mc-MC-8a.md, change request 1): without it the service never starts and the
# scenario is RED (MC_ALLOW_NO_GLUE=1 turns that into a SKIP, for the lane branch before the desk applied it).
set -eu -o pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mc-60-service}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/mcp"
# shellcheck source=host/tests/mcp/lib.bash
. "$HERE/lib.bash"
LO="${BITSOS_PORT_LO:-25000}"
KFWD=$((LO + 100))
SFWD=$((LO + 101))
MPORT=$((LO + 160))
TOKEN="mc8a-scenario-token-51c2"

if ! grep -q "fn bitsmcp" user/init/src/main.rs; then
    if [ "${MC_ALLOW_NO_GLUE:-}" = 1 ]; then echo "SKIP: user/init/src/main.rs does not start bitsmcp yet (plan/notes/mc-MC-8a.md, change request 1)"; exit 0; fi
    echo "RED: user/init lacks the bitsmcp glue (plan/notes/mc-MC-8a-init.patch, change request 1)"
    exit 1
fi

LPID=""
QPID=""
QNAME=""
QEXTRA=()
cleanup() {
    [ -n "$QPID" ] && stop_run "$QPID" "$QNAME"
    if [ -n "$LPID" ] && kill -0 "$LPID" 2>/dev/null; then kill "$LPID" 2>/dev/null || true; wait "$LPID" 2>/dev/null || true; fi
}
trap cleanup EXIT

# The console mirror's datagrams, one per line, with the host time in front.
listen() { # $1 = output file
    rm -f "$1"
    python3 -I -c '
import socket, sys, time
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(("127.0.0.1", int(sys.argv[1])))
out = open(sys.argv[2], "a", buffering=1)
out.write("listening\n")
while True:
    d, _ = s.recvfrom(65535)
    out.write("%.3f %s\n" % (time.time(), d.decode("utf-8", "replace").rstrip("\n")))
' "$MPORT" "$1" &
    LPID=$!
    for _ in $(seq 50); do grep -q listening "$1" 2>/dev/null && return 0; sleep 0.1; done
    echo "RED: the UDP listener did not start"; exit 1
}

boot() { # $1 = name, $2 = mcp mode, $3.. = hostfwd pairs
    local name="$1" mode="$2"; shift 2
    QNAME="$name"
    # data_img_mb: a /data disk, so bitsjournal runs and keeps the service's calls.
    cargo xtask image --name "$name" --config-extra "token=$TOKEN;mcp_port=8765;mcp=$mode;loglevel=info;mirror=10.0.2.2:$MPORT;data_img_mb=64" | tail -1
    # The data image needs room on the stick beyond the kernel's reserve (as rb/70-journal.sh).
    local img="target/xtask/$name.img" kib
    kib=$(( $(stat -c %s "$img") / 1024 + 700 * 1024 ))
    rm -f "$img"
    mkfs.fat -F 32 -n BITSOS -C "$img" "$kib" > /dev/null
    mcopy -s -i "$img" "target/xtask/esp-$name/EFI" "target/xtask/esp-$name/BITSOS2" ::/
    stop_qemu "$name"
    local fwd=()
    for f in "$@"; do fwd+=(--hostfwd "$f"); done
    rm -f "$RUN_DIR/$name.serial"
    cargo xtask qemu --name "$name" --net virtio "${fwd[@]}" --timeout 300 --serial-log "$RUN_DIR/$name.serial" ${QEXTRA[@]+-- "${QEXTRA[@]}"} > "$RUN_DIR/$name.qemu" 2>&1 &
    QPID=$!
}

served_or_skip() { # $1 = serial log: the service came up, or init has no binary / the glue is missing
    for _ in $(seq 120); do
        grep -asq "bitsmcp: serving MCP on port" "$1" && return 0
        sleep 1
    done
    echo "RED: bitsmcp did not report serving within 120 s"; grep -a "bitsmcp\|gate\.\|mcp server" "$1" | tail -20; exit 1
}

# --- boot 1: mcp=both -------------------------------------------------------------------------------------
listen "$RUN_DIR/mirror-both.txt"
boot mc-svc-both both "$KFWD:8765" "$SFWD:8766"
LOG="$RUN_DIR/mc-svc-both.serial"
served_or_skip "$LOG"
grep -aq "mcp gate.armed mode=both port=8766" "$LOG" || { echo "RED: no gate.armed event"; exit 1; }
python3 -I "$HERE/service_check.py" both "$KFWD" "$SFWD" "$TOKEN" | tee "$RUN_DIR/check-both.txt"
before=$(wc -l < "$RUN_DIR/mirror-both.txt")
sleep 3
after=$(wc -l < "$RUN_DIR/mirror-both.txt")
grep -a "bitsmcp\|svc.call" "$RUN_DIR/mirror-both.txt" | tail -3 || true
[ "$after" -gt 1 ] || { echo "RED: the console mirror sent nothing"; exit 1; }
grep -aq "init: bitsmcp (pid [0-9]*) ended" "$LOG" || { echo "RED: init did not see the service end"; exit 1; }
[ "$(grep -ac "init: started the MCP service" "$LOG")" -ge 2 ] || { echo "RED: init did not start the service again"; exit 1; }
grep -aq "mcp svc.call tool=hw_info peer=10.0.2.2:[0-9]* result=ok" "$LOG" || { echo "RED: no kernel event for a call through the service"; exit 1; }
grep -aEq "mcp svc.install_refused tool=reload|mcp svc.install_decided id=[0-9]+ tool=reload decision=no" "$LOG" || { echo "RED: no event for the refused installer request"; exit 1; }
grep -aq "init: bitsmcp (pid [0-9]*) ended" "$RUN_DIR/mirror-both.txt" || { echo "RED: the mirror did not carry init's line about the killed service"; exit 1; }
if grep -aq "$TOKEN" "$LOG" "$RUN_DIR/mirror-both.txt"; then echo "RED: the token appears in the log (R8i)"; exit 1; fi
echo "mirror lines: $before then $after"
stop_run "$QPID" mc-svc-both; QPID=""
kill "$LPID" 2>/dev/null || true; wait "$LPID" 2>/dev/null || true; LPID=""

# --- boot 2: mcp=service ----------------------------------------------------------------------------------
listen "$RUN_DIR/mirror-service.txt"
# A USB keyboard and QMP: the only way into the machine besides the service (the kill below).
QMP="$RUN_DIR/mc-svc-only.qmp"
rm -f "$QMP"
QEXTRA=(-machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 -qmp "unix:$QMP,server=on,wait=off")
boot mc-svc-only service "$KFWD:8765"
QEXTRA=()
LOG="$RUN_DIR/mc-svc-only.serial"
served_or_skip "$LOG"
grep -aq "mcp server.not_started reason=\"mcp=service\"" "$LOG" || { echo "RED: the kernel's server was started under mcp=service"; exit 1; }
if grep -aq "mcp server.start port=8765" "$LOG"; then echo "RED: the kernel's server listens under mcp=service"; exit 1; fi
python3 -I "$HERE/service_check.py" service "$KFWD" "$TOKEN" | tee "$RUN_DIR/check-service.txt"
# The desk's own client against the strict service (MC-10): the token alone over plain HTTP gets 403.
if command -v node > /dev/null; then
    rc=0; out=$(BITSOS_TOKEN="$TOKEN" node host/mcp/mcp-call.mjs "127.0.0.1:$KFWD" hw_info '{"section":"cpu"}' 2>&1) || rc=$?
    [ "$rc" = 2 ] && echo "$out" | grep -q "HTTP 403" || { echo "RED: mcp-call.mjs without --tls under mcp=service: exit $rc: $out"; exit 1; }
    echo "ok: host/mcp/mcp-call.mjs without --tls gets HTTP 403 under mcp=service (exit 2)"
else
    echo "note: node is not installed: mcp-call.mjs not tried"
fi
python3 -I "$HERE/service_check.py" kill "$KFWD" "$TOKEN" "$QMP" "$LOG" | tee "$RUN_DIR/kill-service.txt"
if grep -aq "mcp server.start port=8765" "$LOG"; then echo "RED: the kernel's server started after the service ended"; exit 1; fi
sleep 2
grep -aq "init: bitsmcp (pid [0-9]*) ended" "$RUN_DIR/mirror-service.txt" || { echo "RED: the mirror did not carry init's line about the killed service"; exit 1; }
grep -aq "bitsmcp: serving MCP on port 8765" "$RUN_DIR/mirror-service.txt" || { echo "RED: the mirror did not carry the service's start line"; exit 1; }
if grep -aq "$TOKEN" "$LOG" "$RUN_DIR/mirror-service.txt"; then echo "RED: the token appears in the log (R8i)"; exit 1; fi
echo "mc 60-service: green (mcp=both: answers equal tool by tool, kill + restart, kernel alive, mirror; filter probe; mcp=service: service alone on mcp_port, kill from the console + restart, mirror)"
