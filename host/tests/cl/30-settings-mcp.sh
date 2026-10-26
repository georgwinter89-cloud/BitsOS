#!/bin/bash
# slow
# CL-2 on V2 (QEMU): settingsd's MCP tools (MC-6). The static musl settingsd is sent with write_file and started
# from the console's BusyBox shell (uid 0; init will start it after CL-1 CR2); it registers settings_* with the
# kernel. Through those tools every key is set, read back and undone, R8 switches are refused with CONFIG.TXT
# byte-identical, and the tools vanish with a killed settingsd and come back with a new one. One call goes through
# mcp-call.mjs, the command Claude Code on the Gaming-PC uses (exit 0 ok, 1 refused). Ports: lane range + 30.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT="$(pwd)"
for t in python3 node mtype; do command -v $t >/dev/null || { echo "SKIP: $t missing"; exit 0; }; done
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/cl-30-settings-mcp}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30000}"
FWD=$((LO + 30))
TOKEN="cl2-scenario-token-91c4"
LOG="$RUN_DIR/serial.log"
NAME=cl-settings-mcp

(cd user/services/settingsd && CARGO_TARGET_DIR="$ROOT/target" cargo build --offline --release --target x86_64-unknown-linux-musl) \
    > "$RUN_DIR/build.log" 2>&1 || { tail -20 "$RUN_DIR/build.log"; echo "RED: musl build"; exit 1; }
BIN="$ROOT/target/x86_64-unknown-linux-musl/release/settingsd"

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;wlan=off;nvme_write=off"
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
python3 -I host/tests/cl/settings_mcp_check.py "$FWD" "$TOKEN" "$BIN" "$RUN_DIR"
export BITSOS_TOKEN="$TOKEN"
node host/mcp/mcp-call.mjs "127.0.0.1:$FWD" settings_get '{"key":"theme.accent"}' > "$RUN_DIR/cli-get.txt" \
    || { cat "$RUN_DIR/cli-get.txt"; echo "RED: mcp-call settings_get"; exit 1; }
grep -q '"value":"#ff00aa"' "$RUN_DIR/cli-get.txt" || { cat "$RUN_DIR/cli-get.txt"; echo "RED: mcp-call value"; exit 1; }
rc=0
node host/mcp/mcp-call.mjs "127.0.0.1:$FWD" settings_set '{"key":"nvme_write","value":true}' > "$RUN_DIR/cli-r8.txt" 2>&1 || rc=$?
[ "$rc" = 1 ] && grep -q refused_r8 "$RUN_DIR/cli-r8.txt" || { cat "$RUN_DIR/cli-r8.txt"; echo "RED: mcp-call R8 exit $rc"; exit 1; }
echo "ok: mcp-call.mjs settings_get / settings_set refused (exit 1)"
unset BITSOS_TOKEN
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
grep -aq "settingsd mcp.registered" "$LOG" || { echo "RED: no mcp.registered event in the kernel log"; exit 1; }
stop
trap - EXIT
IMG="target/xtask/$NAME.img"
mtype -i "$IMG" ::BITSOS2/settings/theme.toml > "$RUN_DIR/theme.toml" 2>&1 || { cat "$RUN_DIR/theme.toml"; echo "RED: no theme.toml on the image"; exit 1; }
grep -q 'theme.accent = "#ff00aa"' "$RUN_DIR/theme.toml" || { cat "$RUN_DIR/theme.toml"; echo "RED: theme.toml content"; exit 1; }
mtype -i "$IMG" ::BITSOS2/CONFIG.TXT | grep -Eq "^nvme_write=off" || { echo "RED: nvme_write line changed"; exit 1; }
echo "settingsd MCP tools on V2 (QEMU): green"
