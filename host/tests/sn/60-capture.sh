#!/bin/bash
# SN-9: recording in QEMU. intel-hda + hda-duplex on a `none` audiodev (its input side records silence by the
# clock; `wav` has no input side, and no host microphone is ever used). A normal boot with MCP over hostfwd:
# alsa-hw capture (raw ioctls; its transcript without "# " lines = capture.expected), CachyOS's arecord (aplay
# run as argv[0] "arecord") -D hw:0,0 2 s, -D default 1 s at 44.1 kHz mono and with arecord's own defaults
# (U8 8 kHz mono) through plug -> mmap_emul (asound.conf as $HOME/.asoundrc), arecord -l, the capture
# controls, the WAVs' sizes.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN="${BITSOS_RUN_DIR:-target/xtask/runs/sn-60-capture}"
mkdir -p "$RUN"
RUN="$(realpath "$RUN")"
HERE="$(pwd)/host/tests/sn"
# shellcheck source=host/tests/mcp/lib.bash
. "$(pwd)/host/tests/mcp/lib.bash"
LANE="${BITSOS_LANE:-SN}"
LO="${BITSOS_PORT_LO:-27500}"
FWD=$((LO + 60))
TOKEN="sn9-capture-scenario-7a2d"
NAME="$LANE-capture"
LOG="$RUN/capture.log"

if [ ! -x /usr/bin/aplay ] || [ ! -f /usr/share/alsa/alsa.conf ]; then
    echo "SKIP: no alsa-utils/alsa-lib on this host (aplay and alsa.conf are packaged from it)"
    exit 0
fi
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
stop_qemu "$NAME"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 300 --smp 4 --serial-log "$LOG" -- \
    -audiodev "none,id=a0" -device intel-hda,id=h0 -device hda-duplex,bus=h0.0,audiodev=a0,mixer=off > "$RUN/qemu.out" 2>&1 &
QPID=$!
trap 'stop_run "$QPID" "$NAME"' EXIT
fail=0
python3 -I "$HERE/cap_mcp.py" "$FWD" "$TOKEN" "$HERE/capture.expected" || fail=1
stop_run "$QPID" "$NAME"
trap - EXIT
grep -a " snd \(pcm\|cap\|hda\.open_input\|hda\.overrun\|ctl\.capture\)" "$LOG" | head -60 || true
if grep -aq " E snd \| E ksync" "$LOG"; then echo "FAIL error events:"; grep -a " E snd \| E ksync" "$LOG" | head -5; fail=1; fi
[ "$fail" = 0 ] && echo "sn capture: green (alsa-hw capture as Linux, arecord hw and default, arecord -l, capture controls)"
exit "$fail"
