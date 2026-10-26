#!/bin/bash
# slow
# DP-1: what QEMU really shows equals the kernel's RAM frame (QMP screendump against QMP pmemsave of the frame),
# for the boot layout (BGR) with a full and a damage-only present, then through an RGB layout and two bit-mask
# layouts presented over the same scanout (scene_check.py models each layout independently of the kernel).
# Two boots: the native 2560x1440 (default since BT-2) and 1024x768 (screen=). The ktest dp.scene.screendump
# talks to this script over a second UART (COM2 socket); in the normal gate it skips.
set -eu
cd "$BITSOS_ROOT"
export CARGO_TARGET_DIR="$BITSOS_ROOT/target"
RUN="$BITSOS_RUN_DIR"
FAILED=0

scene() { # tag, extra config
    local tag="$1" extra="$2" name="$BITSOS_LANE-scene-$1"
    local log="$RUN/$tag.log" qmp="$RUN/$tag-qmp.sock" com2="$RUN/$tag-com2.sock" out="$RUN/$tag"
    mkdir -p "$out"
    rm -f "$qmp" "$com2"
    cargo xtask image --name "$name" --ktest --config-extra "ktest=dp.scene;$extra" > "$RUN/$tag-image.txt" 2>&1 \
        || { cat "$RUN/$tag-image.txt"; echo "RED $tag: image build failed"; FAILED=1; return; }
    cargo xtask qemu --name "$name" --timeout 240 --boot-retries 9 --net none --serial-log "$log" -- \
        -qmp "unix:$qmp,server,nowait" -serial "unix:$com2,server,nowait" > "$RUN/$tag-qemu.out" 2>&1 &
    local qpid=$!
    echo "--- $tag ($extra)"
    if ! python3 -I host/tests/dp/scene_check.py "$qmp" "$com2" "$out" --timeout 220; then
        FAILED=1
    fi
    # xtask ends with QEMU (the kernel exits through isa-debug-exit after the ktest) or at its own timeout.
    for _ in $(seq 1 60); do kill -0 "$qpid" 2>/dev/null || break; sleep 1; done
    if kill -0 "$qpid" 2>/dev/null; then kill "$qpid" 2>/dev/null || true; fi
    wait "$qpid" 2>/dev/null || true
    if grep -aq "KTEST ok dp.scene.screendump" "$log"; then
        echo "  ok: ktest dp.scene.screendump passed"
    else
        echo "  RED $tag: ktest dp.scene.screendump did not pass"
        grep -a "KTEST\|gopfb\|display " "$log" | tail -8 || true
        FAILED=1
    fi
    grep -a "gopfb mapped" "$log" | tail -1 || true
}

scene native ""
scene 1024 "screen=1024x768"
exit $FAILED
