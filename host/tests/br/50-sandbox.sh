#!/bin/bash
# minipc: left the QEMU runs (Georg 30.10., CONTRACTS section 4); its checks run on the Mini-PC by the check list in plan/notes/br-BR-6b.md
# slow
# BR-6: Brave on V2 with its own sandbox (no --no-sandbox): QEMU -smp 4, 6 GiB, bitscomp's fbdev backend, the
# stick's launcher as uid 1000. chrome://sandbox (typed, as Chromium opens no chrome:// page from its command line)
# is compared row by row with the host's (tools/reference.sh: headless bitscomp, the same tree and flags, CachyOS's
# kernel); every renderer has seccomp mode 2; chrome://crash/ ends its tab only (the other tab and Brave live on, a
# page with BR-1's checks still loads), chrome://gpucrash likewise; faults only in the processes crashed on purpose,
# no seccomp kill, no ENOSYS beyond the known. v2_sandbox.py says GREEN/RED. Needs the stage
# (`bash user/ports/brave-launcher/scripts/stage-v2.sh`) and SX-1 + SX-2 in the kernel.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT=$PWD
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/br-50-sandbox}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-31000}"
FWD=$((LO + 60))
PORT=$((LO + 70))
TOKEN="br6-scenario-token-7d2e"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=br-sandbox
STAGE=${BRAVE_STAGE:-/var/tmp/claude-v2/stage/brave}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
[ -x "$STAGE/brave" ] || { echo "RED: no staged Brave in $STAGE (bash user/ports/brave-launcher/scripts/stage-v2.sh)"; exit 1; }

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

echo "== host reference: chrome://sandbox in the staged Brave with the launcher's flags (CachyOS's sandbox)"
(cd user/services/bitscomp && cargo build --release -q --offline)
REF_KEYS="$(python3 -I host/tests/br/dekeys.py --ctrl t 'chrome://sandbox/\n')" REF_MARK2="Sandbox Status" \
bash host/tests/br/tools/reference.sh "$RUN_DIR/ref" "$PORT" "http://br3.test:$PORT/render.html" "BR-3 render ready" \
    --test-type "--host-resolver-rules=MAP br3.test 127.0.0.1, MAP * ~NOTFOUND"
# The launcher's own flags must not switch the sandbox off, and its default is on (BR-6b: probed and said, never
# --no-sandbox; only BRAVE_SANDBOX=auto|off may add it).
grep -qx 'SANDBOX=${BRAVE_SANDBOX:-on}' user/ports/brave-launcher/bitsos/brave || { echo "RED: the launcher's sandbox default is not on"; exit 1; }
if sed -n '/^FLAGS="/,/"$/p' user/ports/brave-launcher/bitsos/brave | grep -qE -- '--no-sandbox|--disable-(seccomp-filter|namespace)-sandbox'; then
    echo "RED: the launcher's FLAGS switch the sandbox off"; exit 1
fi

# proc=info: the end of every process (`proc exit ... status=signal N`) tells the renderer's crash from Brave's.
V2_STICK_TREES=brave cargo xtask image --name "$NAME" \
    --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn,syscall=info,proc=info;screen=1280x800"
rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --mem 6G --net virtio --hostfwd "$FWD:8765" --timeout 900 --serial-log "$LOG" \
    -- -smp 4 -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
python3 -I host/tests/br/v2_sandbox.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR" "$RUN_DIR/ref" "$PORT"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: Brave on V2 in its own sandbox"
