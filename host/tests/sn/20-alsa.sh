#!/bin/bash
# SN-4: ALSA on V2 in QEMU, sound only into a WAV file (hda-duplex with a `wav` audiodev; nothing plays on the
# host). A normal boot with MCP over hostfwd: alsa-hw selftest (raw ioctls, transcript = alsa-hw.expected; SN-12: no word is only the usage), CachyOS's
# aplay -D hw:0,0 (S16 and FLOAT_LE 48 kHz), aplay -D default (44.1 kHz via plug), aplay -l, speaker-test;
# mixcheck.py alsa: alsa-hw selftest's, s48's and f48's second each sample-exact in the WAV, s441 a clean 1 kHz.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN="${BITSOS_RUN_DIR:-target/xtask/runs/sn-20-alsa}"
mkdir -p "$RUN"
RUN="$(realpath "$RUN")"
HERE="$(pwd)/host/tests/sn"
# shellcheck source=host/tests/mcp/lib.bash
. "$(pwd)/host/tests/mcp/lib.bash"
LANE="${BITSOS_LANE:-SN}"
LO="${BITSOS_PORT_LO:-27500}"
FWD=$((LO + 40))
TOKEN="sn4-alsa-scenario-5c1e"
NAME="$LANE-alsa"
LOG="$RUN/alsa.log"
W0="$RUN/alsa0.wav"

if [ ! -x /usr/bin/aplay ] || [ ! -f /usr/share/alsa/alsa.conf ]; then
    echo "SKIP: no alsa-utils/alsa-lib on this host (aplay and alsa.conf are packaged from it)"
    exit 0
fi
python3 -I "$HERE/mixcheck.py" alsa-gen "$RUN"
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
stop_qemu "$NAME"
rm -f "$LOG" "$W0"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 300 --smp 4 --serial-log "$LOG" -- \
    -audiodev "wav,id=a0,path=$W0,out.frequency=48000,out.channels=2,out.format=s16" \
    -device intel-hda,id=h0 -device hda-duplex,bus=h0.0,audiodev=a0,mixer=off > "$RUN/qemu.out" 2>&1 &
QPID=$!
trap 'stop_run "$QPID" "$NAME"' EXIT
fail=0
python3 -I "$HERE/alsa_mcp.py" "$FWD" "$TOKEN" "$RUN" "$HERE/alsa-hw.expected" || fail=1
# The output closes 2 s after the last client; QEMU writes the WAV's sizes when it ends.
sleep 3
stop_run "$QPID" "$NAME"
trap - EXIT
grep -a " snd \(pcm\|ctl\)\.\| snd client\.xrun\| snd mix\.thread" "$LOG" | head -60 || true
if grep -aq " E snd \| E ksync" "$LOG"; then echo "FAIL error events:"; grep -a " E snd \| E ksync" "$LOG" | head -5; fail=1; fi
python3 -I "$HERE/mixcheck.py" alsa "$W0" || fail=1
[ "$fail" = 0 ] && echo "sn alsa: green (alsa-hw transcript as Linux, aplay hw and default, speaker-test, WAV sample-exact)"
exit "$fail"
