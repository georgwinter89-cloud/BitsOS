#!/bin/bash
# slow
# DP-2: an fbdev program's picture equals MCP `screen`'s picture equals what QEMU shows. The ktest
# dp.fbscene.screendump runs /bin/abi-fbdev draw (double-buffered frames with pans, then stores into the shown
# buffer and a pwrite band), checks display::snapshot() (MCP screen's source) against the program's pattern and
# names that copy on COM2; scene_check.py (DP-1's) compares QMP screendump with it pixel by pixel. Then the program
# is killed and the console must have the screen back. Two boots: native 2560x1440 and screen=1024x768 (a row is
# exactly one page there, at 1440p a page holds 0.4 rows). Needs the musl sysroot (abi-fbdev in the image).
set -eu
cd "$BITSOS_ROOT"
export CARGO_TARGET_DIR="$BITSOS_ROOT/target"
RUN="$BITSOS_RUN_DIR"
FAILED=0

scene() { # tag, extra config
    local tag="$1" extra="$2" name="$BITSOS_LANE-fbscene-$1"
    local log="$RUN/fb-$tag.log" qmp="$RUN/fb-$tag-qmp.sock" com2="$RUN/fb-$tag-com2.sock" out="$RUN/fb-$tag"
    mkdir -p "$out"
    rm -f "$qmp" "$com2"
    cargo xtask image --name "$name" --ktest --config-extra "ktest=dp.fbscene;$extra" > "$RUN/fb-$tag-image.txt" 2>&1 \
        || { cat "$RUN/fb-$tag-image.txt"; echo "RED $tag: image build failed"; FAILED=1; return; }
    cargo xtask qemu --name "$name" --timeout 240 --boot-retries 9 --net none --serial-log "$log" -- \
        -qmp "unix:$qmp,server,nowait" -serial "unix:$com2,server,nowait" > "$RUN/fb-$tag-qemu.out" 2>&1 &
    local qpid=$!
    echo "--- fbdev $tag ($extra)"
    if ! python3 -I host/tests/dp/scene_check.py "$qmp" "$com2" "$out" --timeout 220; then
        FAILED=1
    fi
    for _ in $(seq 1 60); do kill -0 "$qpid" 2>/dev/null || break; sleep 1; done
    if kill -0 "$qpid" 2>/dev/null; then kill "$qpid" 2>/dev/null || true; fi
    wait "$qpid" 2>/dev/null || true
    if grep -aq "KTEST ok dp.fbscene.screendump" "$log"; then
        echo "  ok: ktest dp.fbscene.screendump passed"
    else
        echo "  RED $tag: ktest dp.fbscene.screendump did not pass"
        grep -a "KTEST\|fbdev \|display " "$log" | tail -8 || true
        FAILED=1
    fi
    grep -a "fbdev ready" "$log" | tail -1 || true
}

scene native ""
scene 1024 "screen=1024x768"
exit $FAILED
