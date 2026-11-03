#!/bin/bash
# slow
# DS-1 on V2 in QEMU (-smp 4, 1280x800 GOP): bitscomp started through MCP `run` (fbdev/evdev, fixed status values)
# with two wyclients reads the design v2 theme from V2's settingsd. Through CL-2's MCP tools: Absicht dunkel is the
# default, settings_set theme.mode light gives Absicht hell with its own accent, a user's accent (cyan) replaces it,
# settings_undo restores the first picture exactly, Exo 2 can be chosen as the interface font. Each picture is compared
# (MCP screen and QEMU screendump) with a reference drawn by headless bitscomp on the host (tolerance, the pointer's
# box left out). Before CR DS-1-1 lands, settingsd still carries the WY-5 palette: the client then sets the design's
# values first (and says so). Timing in QEMU is recorded, only the 1 s bound of WY-5 is checked.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/ds-20-theme}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-15000}"
FWD=$((LO + 20))
TOKEN="ds1-scenario-token-4c7d"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=ds-theme
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}

echo "== host references: dark, light, light with a user's accent, dark with Exo 2"
(cd user/services/bitscomp && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)
REF_DIR="$RUN_DIR/ref"
rm -rf "$REF_DIR"
mkdir -m 700 -p "$REF_DIR"
python3 -I host/tests/ds/ds_reference.py "$CARGO_TARGET_DIR/release/bitscomp" "$CARGO_TARGET_DIR/release/wyclient" "$REF_DIR" \
    1280x800 dark "light:theme.mode=light" "accent:theme.accent=#00e5ff" \
    "exo:theme.mode=dark;theme.accent=#7fcfa0;theme.font.ui=Exo 2" > "$RUN_DIR/ref.out"
tail -4 "$RUN_DIR/ref.out"

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;screen=1280x800" > "$RUN_DIR/image.txt" 2>&1 || { tail -20 "$RUN_DIR/image.txt"; exit 1; }
rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 420 --serial-log "$LOG" \
    -- -smp 4 -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu.out" 2>&1 &
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
python3 -I host/tests/ds/ds_theme.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR" "$REF_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: design v2 theme applies live on V2"
