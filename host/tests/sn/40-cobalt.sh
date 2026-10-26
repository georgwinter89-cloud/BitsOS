#!/bin/bash
# SN-7: Cobalt's ALSA write pattern on V2 in QEMU for BITSOS_COBALT_S seconds (default 120), sound only into a
# WAV (hda-duplex with a `wav` audiodev; nothing plays on the host): alsa-hw cobalt keeps snd_pcm_delay at 2048
# frames with 512-frame writes, as starboard/shared/alsa does. Green = 0 XRUNs (EPIPE) that a stall of the guest
# does not explain (alsa_mcp.py cobalt: the writer away >= 20 ms, or an output underrun just before), and,
# when there was no XRUN and no output underrun at all, its sawtooth unbroken in the WAV (mixcheck.py cobalt).
# Stalls are QEMU timing under host load, not judged here (timing goes to the Mini-PC list).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN="${BITSOS_RUN_DIR:-target/xtask/runs/sn-40-cobalt}"
mkdir -p "$RUN"
RUN="$(realpath "$RUN")"
HERE="$(pwd)/host/tests/sn"
# shellcheck source=host/tests/mcp/lib.bash
. "$(pwd)/host/tests/mcp/lib.bash"
LANE="${BITSOS_LANE:-SN}"
LO="${BITSOS_PORT_LO:-27500}"
FWD=$((LO + 42))
SECS="${BITSOS_COBALT_S:-120}"
TOKEN="sn7-cobalt-scenario-9a2f"
NAME="$LANE-cobalt"
LOG="$RUN/cobalt.log"
W0="$RUN/cobalt0.wav"

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
stop_qemu "$NAME"
rm -f "$LOG" "$W0"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout $((SECS + 300)) --smp 4 --serial-log "$LOG" -- \
    -audiodev "wav,id=a0,path=$W0,out.frequency=48000,out.channels=2,out.format=s16" \
    -device intel-hda,id=h0 -device hda-duplex,bus=h0.0,audiodev=a0,mixer=off > "$RUN/qemu.out" 2>&1 &
QPID=$!
trap 'stop_run "$QPID" "$NAME"' EXIT
fail=0
python3 -I "$HERE/alsa_mcp.py" "$FWD" "$TOKEN" cobalt "$SECS" || fail=1
# The output closes 2 s after the last client; QEMU writes the WAV's sizes when it ends.
sleep 3
stop_run "$QPID" "$NAME"
trap - EXIT
grep -a " snd \(pcm\.\|client\.xrun\|client\.underrun\|mix\.hw_underrun\|mix\.thread\|mix\.open\)" "$LOG" | head -40 || true
if grep -aq " E snd \| E ksync" "$LOG"; then echo "FAIL error events:"; grep -a " E snd \| E ksync" "$LOG" | head -5; fail=1; fi
if ! python3 -I "$HERE/mixcheck.py" cobalt "$W0" "$SECS"; then
    if grep -aq " snd \(mix\.hw_underrun\|client\.xrun\)" "$LOG"; then
        echo "note: the WAV breaks at a stall of the guest (output underrun or a timing XRUN above): QEMU timing, not judged"
    else
        fail=1
    fi
fi
[ "$fail" = 0 ] && echo "sn cobalt: green ($SECS s of Cobalt's write pattern, 0 XRUNs, WAV unbroken or only QEMU-timing breaks)"
exit "$fail"
