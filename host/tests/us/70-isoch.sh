#!/bin/bash
# SN-8a: isochronous OUT through V2's xHCI into QEMU's usb-audio (full speed UAC1, 48 kHz s16 stereo) with a wav
# audiodev only: nothing plays on the host's sound devices. Boot 1: `us.isoch.stream` sets the streaming
# interface's alternate setting and streams its pattern for 3 s; afterwards isowav.py checks the WAV sample by
# sample (QEMU's default volume 240/255 taken into account). Boot 2: `us.isoch.unplug` streams, says
# `ktest.iso_streaming`, QMP device_del pulls the usb-audio, the ktest checks that every request ended and the
# controller runs on; on `ktest.iso_replug` QMP device_add plugs a usb-audio in again and it streams again. Timing is never judged in QEMU.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/us-70-isoch}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
SOCK="$RUN_DIR/qmp.sock"
BOOT_LINE="BdsDxe: loading"

qmp() { # $1 = JSON command
    python3 -I -c '
import json, socket, sys
s = socket.socket(socket.AF_UNIX); s.settimeout(20); s.connect(sys.argv[1]); f = s.makefile("rw")
f.readline()
for c in ({"execute": "qmp_capabilities"}, json.loads(sys.argv[2])):
    f.write(json.dumps(c) + "\n"); f.flush()
    while True:
        r = json.loads(f.readline())
        if "return" in r or "error" in r:
            break
    if "error" in r:
        sys.exit("qmp: %s" % r)
' "$SOCK" "$1"
}
wait_line() { # $1 = pattern, $2 = log, $3 = seconds
    local n=$(( $3 * 10 ))
    for _ in $(seq 1 "$n"); do grep -aq "$1" "$2" 2>/dev/null && return 0; sleep 0.1; done
    return 1
}
QPID=""
NAME=""
# Only our own processes: the QEMU this xtask started (its pid file) and the xtask itself.
stop_own() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
        local q; q=$(cat "target/xtask/qemu-$NAME.pid" 2>/dev/null || true)
        if [ -n "$q" ]; then kill "$q" 2>/dev/null || true; fi
        kill "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    QPID=""
}
trap stop_own EXIT
fail() { echo "RED: $*"; exit 1; }

boot() { # $1 = name, $2 = config extra
    NAME="$1"; local log="$RUN_DIR/$NAME.log" wav="$RUN_DIR/$NAME.wav"
    cargo xtask image --name "$NAME" --ktest --config-extra "$2"
    for attempt in 1 2; do
        rm -f "$SOCK" "$log" "$wav"
        cargo xtask qemu --name "$NAME" --timeout 240 --serial-log "$log" -- \
            -device qemu-xhci,id=xb \
            -audiodev "wav,id=a0,path=$wav,out.frequency=48000,out.channels=2,out.format=s16" \
            -device usb-audio,id=snd1,audiodev=a0,bus=xb.0,port=1 -qmp "unix:$SOCK,server=on,wait=off" &
        QPID=$!
        if wait_line "$BOOT_LINE" "$log" 60; then return 0; fi
        echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
        stop_own
    done
    fail "$NAME never booted"
}
finish() { # $1 = ktest name
    local log="$RUN_DIR/$NAME.log"
    wait_line "KTEST done" "$log" 120 || fail "$NAME: ktests did not finish"
    wait "$QPID" || true
    QPID=""
    grep -a "KTEST\|ktest\.\|xhci\.iso\|xhci\.alt\|device\.alt\|device\.detached" "$log" || true
    grep -aq "KTEST ok $1" "$log" || fail "$NAME: $1 did not pass"
    if grep -aq "xhci.failed\|xhci.slot_leaked\|lock.violation\|PANIC" "$log"; then fail "$NAME: failure line"; fi
}

boot us-isoch-stream "ktest=us.isoch.stream;usb_audio=off;loglevel=info"
finish us.isoch.stream
frames=$(grep -ao "ktest.iso_stream .*frames=[0-9]*" "$RUN_DIR/$NAME.log" | sed 's/.*frames=//' | head -1)
[ -n "$frames" ] || fail "no frame count in the log"
python3 -I host/tests/us/isowav.py "$RUN_DIR/$NAME.wav" --min-frames $(( frames / 2 )) || fail "WAV check"

boot us-isoch-unplug "ktest=us.isoch.unplug;usb_iso_test=on;loglevel=info"
wait_line "ktest.iso_streaming" "$RUN_DIR/$NAME.log" 90 || fail "$NAME: not streaming"
qmp '{"execute":"device_del","arguments":{"id":"snd1"}}'
wait_line "ktest.iso_replug" "$RUN_DIR/$NAME.log" 90 || fail "$NAME: unplug not handled"
qmp '{"execute":"device_add","arguments":{"driver":"usb-audio","id":"snd2","audiodev":"a0","bus":"xb.0","port":"1"}}'
finish us.isoch.unplug
grep -aq "ktest.iso_replug_done" "$RUN_DIR/$NAME.log" || fail "$NAME: no stream after the replug"
echo "us-70-isoch: green"
