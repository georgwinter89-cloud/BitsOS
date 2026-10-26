#!/bin/bash
# slow
# BR-3: Brave on V2 (QEMU -smp 4, 6 GiB, 1280x800 GOP) in bitscomp's fbdev backend, started by the stick's launcher
# (\BITSOS2\bin\brave, tree \BITSOS2\brave\ from user/ports/brave-launcher/scripts/stage-v2.sh) as uid 1000 with
# --no-sandbox. Pages come from an HTTP server on the host (127.0.0.1, seen from the guest as 10.0.2.2 on QEMU's
# user network, by the name br3.test; every other name is NOTFOUND: no internet). render.html is compared with the
# host's picture (tools/reference.sh: headless bitscomp, the same tree and flags); index.html runs BR-1's subsystem
# checks; the profile goes onto the stick as a tar and comes back. Brave's log, faults and ENOSYS are checked
# (v2_brave.py). Needs the stage: `bash user/ports/brave-launcher/scripts/stage-v2.sh` (7 s, copies /opt/brave-bin).
# BR-3 CR 1 (/proc/<pid>/task link count) is merged since INT-W10.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT=$PWD
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/br-10-start}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-31000}"
FWD=$((LO + 10))
PORT=$((LO + 20))
TOKEN="br3-scenario-token-5a1c"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=br-start
STAGE=${BRAVE_STAGE:-/var/tmp/claude-v2/stage/brave}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
[ -x "$STAGE/brave" ] || { echo "RED: no staged Brave in $STAGE (bash user/ports/brave-launcher/scripts/stage-v2.sh)"; exit 1; }

# The pages, from the host. A server left on the port would serve some other page without any error: refuse a busy
# port, then wait until ours answers with the page's own file.
PAGE=$ROOT/host/tests/br/page
if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then echo "RED: port $PORT already in use"; exit 1; fi
python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$PAGE" >"$RUN_DIR/http.log" 2>&1 &
HTTP=$!
QPID=""
stop() {
    [ -n "$QPID" ] && kill "$QPID" 2>/dev/null || true
    pf="target/xtask/qemu-$NAME.pid"
    if [ -n "$QPID" ] && [ -f "$pf" ]; then
        pid="$(head -1 "$pf")"
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then kill "$pid" 2>/dev/null || true; fi
    fi
    kill "$HTTP" 2>/dev/null || true
    wait 2>/dev/null || true
}
trap stop EXIT
up=0
for _ in $(seq 50); do
    kill -0 "$HTTP" 2>/dev/null || break
    if curl -sf --max-time 2 "http://127.0.0.1:$PORT/render.html" | cmp -s - "$PAGE/render.html"; then up=1; break; fi
    sleep 0.2
done
[ "$up" = 1 ] || { echo "RED: http server on $PORT did not serve $PAGE"; exit 1; }

echo "== host reference: headless bitscomp, the staged Brave, render.html at 1280x800"
(cd user/services/bitscomp && cargo build --release -q --offline)
bash host/tests/br/tools/reference.sh "$RUN_DIR/ref" "$PORT" "http://br3.test:$PORT/render.html" "BR-3 render ready" \
    --test-type "--host-resolver-rules=MAP br3.test 127.0.0.1, MAP * ~NOTFOUND" \
    "--unsafely-treat-insecure-origin-as-secure=http://br3.test:$PORT"

V2_STICK_TREES=brave cargo xtask image --name "$NAME" \
    --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn,syscall=info;screen=1280x800"
rm -f "$LOG" "$QMP"
# 6 GiB: Brave's reservations, ~100 thread stacks and its commits want more than the default 2 GiB (BR-1 row 6).
cargo xtask qemu --name "$NAME" --mem 6G --net virtio --hostfwd "$FWD:8765" --timeout 900 --serial-log "$LOG" \
    -- -smp 4 -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
python3 -I host/tests/br/v2_brave.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR" "$RUN_DIR/ref" "$PORT"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: Brave on V2"
