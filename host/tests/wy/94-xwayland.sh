#!/bin/bash
# slow
# WY-3 on V2 in QEMU (-smp 4, 1280x800 GOP): X11 programs through Xwayland (software, no glamor). bitscomp holds
# X display :0 and starts Xwayland (\BITSOS2\xwayland\, built from the X.Org release by
# user/ports/xwayland/scripts/build-v2.sh) at the first X11 client; its window manager gives each X11 window a place
# and frame on the desk. Checked: xdpyinfo, xrandr (the one output), xinput list (XInput2 seat devices), xeyes and
# xclock as framed windows, keys into xterm, the clipboard both ways (xclip, wl-copy/wl-paste), close through bitscomp.
# The gpu build (WY_GPU=1, Pixman; the default here, since Xwayland runs with the Mesa tree's glibc) or WY_GPU=gles.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wy-94-xwayland}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30500}"
FWD=$((LO + 94))
TOKEN="wy3-scenario-token-6c1d"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=wy-xwayland
case ${WY_GPU:-0} in 0|"") export WY_GPU=1 ;; esac
export WY_XWAYLAND=1
WY_GPU_CFG=$(bash host/tests/wy/tools/gpu-image.sh config)
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;screen=1280x800$WY_GPU_CFG" > "$RUN_DIR/image.txt" 2>&1 || { tail -20 "$RUN_DIR/image.txt"; exit 1; }
bash host/tests/wy/tools/gpu-image.sh image "$NAME"
rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 600 --mem 4G --serial-log "$LOG" \
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
python3 -I host/tests/wy/v2_xwayland.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: X11 programs through Xwayland on V2 (WY_GPU=$WY_GPU)"
