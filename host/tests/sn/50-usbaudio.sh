#!/bin/bash
# SN-8b: USB audio (crates/drivers/usb-audio) through the kernel mixer on QEMU's usb-audio (UAC1, full speed,
# 48 kHz s16 stereo) on a qemu-xhci, beside an intel-hda + hda-duplex as the jack. Sound only into WAV files.
#  A: sn.usb.bound (bound, alternate setting made at probe), sn.usb.tone (Auto picks the headset; 2 s of
#     testsig c) -> usbcheck.py tone: every frame once, in order, within QEMU's 240/255 volume; the jack silent.
#  B: sn.usb.unplug (usb_audio_unplug_test=on): QMP device_del pulls the usb-audio while a client plays, the
#     mixer goes back to the jack, device_add plugs one in again (its own WAV), the mixer follows ->
#     usbcheck.py unplug: headset, jack, headset in order, nothing twice, nothing garbled.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
LANE="${BITSOS_LANE:-sn}"
RUN="${BITSOS_RUN_DIR:-target/xtask/runs/sn-50-usbaudio}"
mkdir -p "$RUN"
RUN="$(realpath "$RUN")"
HERE="$PWD/host/tests/sn"
SOCK="$RUN/qmp.sock"
fail=0
check() { if grep -aq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3 (no '$2')"; fail=1; fi; }
nocheck() { if grep -aq -- "$2" "$1"; then echo "FAIL $3:"; grep -a -- "$2" "$1" | head -5; fail=1; else echo "ok   $3"; fi; }

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
wait_line() { # pattern log seconds
    local n=$(( $3 * 10 ))
    for _ in $(seq 1 "$n"); do grep -aq -- "$1" "$2" 2>/dev/null && return 0; sleep 0.1; done
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

boot() { # name config-extra log: starts QEMU in the background (WAVs u.wav, u2.wav, h.wav of the name)
    NAME="$LANE-$1"; local log="$3"
    rm -f "$SOCK" "$log" "$RUN/$1-u.wav" "$RUN/$1-u2.wav" "$RUN/$1-h.wav"
    cargo xtask image --name "$NAME" --ktest --config-extra "$2;loglevel=info"
    cargo xtask qemu --name "$NAME" --timeout 240 --smp 4 --net virtio --serial-log "$log" -- \
        -device qemu-xhci,id=xb \
        -audiodev "wav,id=a0,path=$RUN/$1-u.wav,out.frequency=48000,out.channels=2,out.format=s16" \
        -audiodev "wav,id=a2,path=$RUN/$1-u2.wav,out.frequency=48000,out.channels=2,out.format=s16" \
        -device usb-audio,id=snd1,audiodev=a0,bus=xb.0,port=1 \
        -audiodev "wav,id=a1,path=$RUN/$1-h.wav,out.frequency=48000,out.channels=2,out.format=s16" \
        -device intel-hda,id=h0 -device hda-duplex,bus=h0.0,audiodev=a1,mixer=off \
        -qmp "unix:$SOCK,server=on,wait=off" &
    QPID=$!
}
done_wait() { # log
    wait_line "KTEST done" "$1" 200 || { echo "FAIL $NAME: ktests did not finish"; fail=1; }
    wait "$QPID" || true
    QPID=""
    touch "$1"
    grep -a "KTEST\|I ktest\|usb_audio uac\.\(bound\|stream\|unbound\)\|snd mix\.\(open\|switch\|output_lost\)" "$1" | cut -c1-220 || true
}

# --- A ---
LOG="$RUN/a.log"
boot a "ktest=sn.usb.bound,sn.usb.tone,sn.usb.capture,sn.mix.state" "$LOG"
done_wait "$LOG"
check "$LOG" 'KTEST done passed=3 failed=0 skipped=1' "A: bound, tone, state pass; capture skips (no mic in QEMU)"
check "$LOG" 'usb_audio uac.bound device=xhci[0-9]*-[0-9]* vendor=0x46f4 product=0x2 control=0 playback=1 capture=0' "A: QEMU's usb-audio bound"
check "$LOG" 'usb_audio uac.stream .* interface=1 alt=1 endpoint=0x1 dir=playback channels=2 bits=16' "A: its 48 kHz stereo setting"
check "$LOG" 'snd mix.open endpoint=usb:xhci[0-9]*-[0-9]*/usb/0:0x1' "A: Auto opened the headset"
check "$LOG" 'usb_audio uac.play_open .* window_us=20000 packets_per_request=4' "A: 20 ms on the bus in 4 ms requests"
nocheck "$LOG" 'snd mix.open endpoint=hda' "A: the jack stays closed while the headset is there"
nocheck "$LOG" ' E snd \| E usb_audio \| E usb \| E ksync\|PANIC\|lock.violation' "A: no error events"
python3 -I "$HERE/usbcheck.py" tone "$RUN/a-u.wav" 96000 || { echo "FAIL A: tone WAV"; fail=1; }

# --- B ---
LOG="$RUN/b.log"
boot b "ktest=sn.usb.unplug;usb_audio_unplug_test=on" "$LOG"
if wait_line "I ktest usb_playing" "$LOG" 150; then
    qmp '{"execute":"device_del","arguments":{"id":"snd1"}}' || { echo "FAIL B: device_del"; fail=1; }
    if wait_line "I ktest usb_unplugged" "$LOG" 90; then
        qmp '{"execute":"device_add","arguments":{"driver":"usb-audio","id":"snd2","audiodev":"a2","bus":"xb.0","port":"1"}}' \
            || { echo "FAIL B: device_add"; fail=1; }
    else
        echo "FAIL B: the mixer never went back to the jack"; fail=1
    fi
else
    echo "FAIL B: never played on the headset"; fail=1
fi
done_wait "$LOG"
check "$LOG" 'KTEST ok sn.usb.unplug' "B: unplug and replug while playing"
check "$LOG" 'usb_audio uac.unbound device=xhci[0-9]*-[0-9]* control=0 gone=true' "B: unbound on the pull"
check "$LOG" 'snd mix.switch from=hda0/jack/.* to=usb:xhci[0-9]*-[0-9]*/usb/0:0x1 mode=make_before_break why=plug' "B: back on the headset without a break"
nocheck "$LOG" ' E snd \| E usb_audio \| E ksync\|PANIC\|lock.violation\|xhci.failed\|xhci.slot_leaked' "B: no error events"
python3 -I "$HERE/usbcheck.py" unplug "$RUN/b-u.wav" "$RUN/b-h.wav" "$RUN/b-u2.wav" || { echo "FAIL B: unplug WAVs"; fail=1; }
exit $fail
