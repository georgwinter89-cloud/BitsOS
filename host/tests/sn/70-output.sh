#!/bin/bash
# SN-10: the output switch the Linux way and over MCP, in QEMU (no display engine, so no HDMI): an intel-hda +
# hda-duplex (the jack) and a usb-audio on a qemu-xhci, sound only into WAV files. A normal boot with MCP over
# hostfwd; out_mcp.py drives the MCP tool `snd` and reads /sys/bitsos/snd_output (+ _status) through busybox sh (MCP's
# programs are uid 1000, so their write is EACCES; root's writes are ktest sn.out.sysfs): jack and usb switch, a
# playing alsa-hw cobalt moves from the jack to the headset, hdmi and hdmi-off are refused with the target unchanged,
# a bad value is a bad argument; then the log: the switch, no error events. SN-11: CONFIG stores snd_output=hdmi,
# so the boot switch (snd/boot-out) must log the fallback (no GPU HD Audio function) and the target start auto.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN="${BITSOS_RUN_DIR:-target/xtask/runs/sn-70-output}"
mkdir -p "$RUN"
RUN="$(realpath "$RUN")"
HERE="$(pwd)/host/tests/sn"
# shellcheck source=host/tests/mcp/lib.bash
. "$(pwd)/host/tests/mcp/lib.bash"
LANE="${BITSOS_LANE:-SN}"
LO="${BITSOS_PORT_LO:-27500}"
FWD=$((LO + 70))
TOKEN="sn10-output-scenario-7a2d"
NAME="$LANE-output"
LOG="$RUN/output.log"

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;snd_output=hdmi"
stop_qemu "$NAME"
rm -f "$LOG" "$RUN/h.wav" "$RUN/u.wav"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 300 --smp 4 --serial-log "$LOG" -- \
    -audiodev "wav,id=a0,path=$RUN/h.wav,out.frequency=48000,out.channels=2,out.format=s16" \
    -device intel-hda,id=h0 -device hda-duplex,bus=h0.0,audiodev=a0,mixer=off \
    -device qemu-xhci,id=xb \
    -audiodev "wav,id=a1,path=$RUN/u.wav,out.frequency=48000,out.channels=2,out.format=s16" \
    -device usb-audio,id=snd1,audiodev=a1,bus=xb.0,port=1 > "$RUN/qemu.out" 2>&1 &
QPID=$!
trap 'stop_run "$QPID" "$NAME"' EXIT
fail=0
python3 -I "$HERE/out_mcp.py" "$FWD" "$TOKEN" || fail=1
sleep 3
stop_run "$QPID" "$NAME"
trap - EXIT
grep -a " snd snd\.output\| snd mix\.\(open\|switch\)" "$LOG" | cut -c1-220 | head -40 || true
check() { if grep -aq -- "$1" "$LOG"; then echo "ok   $2"; else echo "FAIL $2 (no '$1')"; fail=1; fi; }
check 'snd snd.output_boot outcome=auto source=boot why="no GPU HD Audio function"' "log: stored hdmi without HDMI falls back to auto"
check 'snd snd.output_boot_done waited_ms=0 outcome=auto' "log: the boot switch finished"
check 'snd snd.output target=Prefer(Jack) source=mcp' "log: jack over MCP"
check 'snd snd.output_refused source=mcp request=Target(Prefer(Hdmi)) errno=19' "log: hdmi over MCP refused (ENODEV)"
check 'snd snd.output_refused source=mcp request=HdmiOff errno=19' "log: hdmi-off over MCP refused"
check 'snd mix.switch from=hda0/jack/.* to=usb:xhci[0-9]*-[0-9]*/usb/0:0x1' "log: the playing client moved from the jack to the headset"
if grep -aq " E snd \| E usb_audio \| E ksync\|PANIC\|lock.violation" "$LOG"; then
    echo "FAIL error events:"; grep -a " E snd \| E usb_audio \| E ksync\|PANIC\|lock.violation" "$LOG" | head -5; fail=1
fi
[ "$fail" = 0 ] && echo "sn output: green (snd tool and /sys/bitsos/snd_output: stored hdmi falls back, jack, usb, auto, HDMI refused, a live switch)"
exit "$fail"
