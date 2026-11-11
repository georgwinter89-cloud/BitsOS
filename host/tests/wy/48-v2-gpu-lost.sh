#!/bin/bash
# slow
# WY-21 on V2 in QEMU (-smp 4, 1280x800 GOP, drm=fake, bitscomp=auto, the gpu build with llvmpipe): the GPU desktop's
# way back when the device is lost. /bin/bitscomp hands over to the gpu build and waits beside it; two animated
# windows on two workspaces. First boot, drm_fake_hang=lose: abi-drm-jobs hangs the fake, which then answers a new open
# of renderD128 with ENODEV as the chip does when GX-9a's ladder cannot recover. The gpu build sees it within seconds,
# draws on with Pixman in the same process, both windows stay and the picture keeps changing. Then a SIGABRT to the gpu
# build (Mesa's abort on a non-robust context): the waiting rescue build takes the screen, both workspaces and both
# programs come back, the picture moves. Last, a new /bin/bitscomp with bitscomp=auto stays with the rescue build: the
# node is there, the GPU is lost. Second boot: `ctl stall` holds the gpu build's main loop as a present stuck in the
# driver would: the stall watch ends it, the rescue build takes over with the session. Timing is never judged here:
# how long each way took is printed, the bound is generous.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wy-48-gpu-lost}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30500}"
FWD=$((LO + 48))
TOKEN="wy21-scenario-token-3c8b"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}
export WY_GPU=auto

boot() { # name mode config
    local name=$1 mode=$2 extra=$3
    local log="$RUN_DIR/$mode-serial.log" qmp="$RUN_DIR/$mode-qmp.sock"
    cargo xtask image --name "$name" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;screen=1280x800;drm=fake;bitscomp=auto$extra" > "$RUN_DIR/$mode-image.txt" 2>&1 || { tail -20 "$RUN_DIR/$mode-image.txt"; exit 1; }
    bash host/tests/wy/tools/gpu-image.sh image "$name"
    rm -f "$log" "$qmp"
    mkdir -p "$RUN_DIR/$mode"
    cargo xtask qemu --name "$name" --net virtio --hostfwd "$FWD:8765" --timeout 900 --serial-log "$log" \
        -- -smp 4 -qmp "unix:$qmp,server=on,wait=off" > "$RUN_DIR/$mode-qemu.out" 2>&1 &
    QPID=$!
    NAME=$name
    trap stop EXIT
    python3 -I host/tests/wy/v2_gpu_lost.py "$FWD" "$TOKEN" "$qmp" "$RUN_DIR/$mode" "$mode"
    if grep -aq "PANIC" "$log"; then echo "RED: the kernel panicked"; tail -20 "$log"; exit 1; fi
    stop
    trap - EXIT
}
stop() {
    kill "$QPID" 2>/dev/null || true
    pf="target/xtask/qemu-$NAME.pid"
    if [ -f "$pf" ]; then
        pid="$(head -1 "$pf")"
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then kill "$pid" 2>/dev/null || true; fi
    fi
    wait "$QPID" 2>/dev/null || true
}
boot wy-gpu-lost lost ";drm_fake_hang=lose"
boot wy-gpu-stall stall ""
echo "GREEN: a lost GPU leaves the desktop on Pixman with its windows; a crash or a stuck loop hands it to the rescue build with the session"
