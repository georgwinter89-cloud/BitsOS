#!/bin/bash
# slow: YT-5: Cobalt plays the click/flash clip (host/tests/yt/avsync/make-clip.sh, 720p30 VP9 + Opus, 20 s) on V2 in
# QEMU (-smp 4, 4 GB, intel-hda + hda-duplex into a WAV file: nothing plays on the host); the page and clip come
# from a web server on the host's loopback (port LO+80, QEMU's 10.0.2.2). Then:
#   - Cobalt's own numbers (`cobalt --stats`, BITSOS_AV_STATS): frames shown and dropped, picture vs audio clock;
#   - the end-to-end offset: when each white frame reached the screen (av-trace.txt) against when its click is in
#     the WAV (the DAC stream), both in guest time (avsync.py; the WAV's start is `snd mix.open` in the serial log).
# Judgement: the clip plays to its end, every flash is in the trace and every click in the WAV, the format answers
# are as on BitsOS (caps.html: no 4K/8K, no HDR, no ec-3/ac-3). The timing numbers are printed, never judged
# (QEMU; the Mini-PC check list in plan/notes/yt-YT-5.md has the real measurement).
# AVSYNC_WAV_OPTS adds QEMU wav audiodev options (e.g. ",out.buffer-length=5000,timer-period=1000": QEMU's own
# buffers lie between the guest's DMA position and the WAV and are invisible to V2's delay; see yt-YT-5.md).
# Missing Cobalt stage or ffmpeg: a SKIP line (AVSYNC_REQUIRE=1 makes it red). Run with --script-timeout 900.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN="${BITSOS_RUN_DIR:-target/xtask/runs/yt-20-avsync}"
mkdir -p "$RUN"
RUN="$(realpath "$RUN")"
HERE="$(pwd)/host/tests/yt"
# shellcheck source=host/tests/mcp/lib.bash
. "$(pwd)/host/tests/mcp/lib.bash"
LANE="${BITSOS_LANE:-YT}"
LO="${BITSOS_PORT_LO:-29000}"
FWD=$((LO + 11))
WEB=$((LO + 80))
TOKEN="yt5-avsync-scenario-41c9"
NAME="$LANE-avsync"
SECS=20
COBALT_STAGE="${COBALT_STAGE:-/var/tmp/claude-v2/yt/cobalt/v2-stick/cobalt}"

if [ ! -f "$COBALT_STAGE/loader_app" ] || ! command -v ffmpeg >/dev/null; then
    echo "SKIP: no Cobalt stage (user/ports/cobalt-launcher/scripts/stage-v2.sh) or no ffmpeg on this host"
    [ "${AVSYNC_REQUIRE:-0}" = 1 ] && { echo "FAIL: AVSYNC_REQUIRE=1 and no stage/ffmpeg"; exit 1; }
    exit 0
fi
rm -rf "$RUN/clip"
bash "$HERE/avsync/make-clip.sh" "$RUN/clip" 1280x720 30 "$SECS"
cp "$HERE/avsync/avsync.html" "$HERE/avsync/caps.html" "$RUN/clip/"
V2_STICK_TREES=cobalt cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"

stop_qemu "$NAME"
LOG="$RUN/serial.log"
rm -f "$LOG" "$RUN/out.wav" "$RUN"/av-*.txt "$RUN/cobalt.log" "$RUN/caps.log"
python3 -I -m http.server "$WEB" --bind 127.0.0.1 --directory "$RUN/clip" > "$RUN/http.log" 2>&1 &
HPID=$!
cargo xtask qemu --name "$NAME" --net user --hostfwd "$FWD:8765" --timeout 540 --smp 4 --mem 4G --serial-log "$LOG" -- \
    -audiodev "wav,id=a0,path=$RUN/out.wav,out.frequency=48000,out.channels=2,out.format=s16${AVSYNC_WAV_OPTS:-}" \
    -device intel-hda,id=h0 -device hda-duplex,bus=h0.0,audiodev=a0,mixer=off > "$RUN/qemu.out" 2>&1 &
QPID=$!
trap 'stop_run "$QPID" "$NAME"; kill "$HPID" 2>/dev/null || true' EXIT
fail=0
python3 -I "$HERE/yt_mcp.py" "$FWD" "$TOKEN" wait 120
# The format answers first (a short run of their own: the page logs and stays).
python3 -I "$HERE/yt_mcp.py" "$FWD" "$TOKEN" avsync "$RUN/caps" "http://10.0.2.2:$WEB/caps.html" 0 > "$RUN/caps.out" 2>&1 || true
python3 -I "$HERE/yt_mcp.py" "$FWD" "$TOKEN" avsync "$RUN" "http://10.0.2.2:$WEB/avsync.html" "$SECS" || fail=1
stop_run "$QPID" "$NAME"
kill "$HPID" 2>/dev/null || true
trap - EXIT

red=0
[ "$fail" = 0 ] || { echo "FAIL: the clip did not play to its end (cobalt.log: $RUN/cobalt.log)"; red=1; }
[ -s "$RUN/av-trace.txt" ] || { echo "FAIL: no av-trace.txt from V2"; exit 1; }
grep -a '"avsync: ' "$RUN/cobalt.log" 2>/dev/null | sed 's/.*"avsync: /  page: /; s/", source.*//' | tail -3 || true
if grep -aq '"avsync: error\|"avsync: media error' "$RUN/cobalt.log" 2>/dev/null; then echo "FAIL: the page reported an error"; red=1; fi
python3 -I "$HERE/avsync/avsync.py" "$RUN/av-trace.txt" "$RUN/out.wav" "$LOG" "$SECS" | tee "$RUN/avsync.txt" || red=1
grep -q "flashes seen $((SECS - 1))/$((SECS - 1))" "$RUN/avsync.txt" || { echo "FAIL: not every flash reached the screen"; red=1; }
grep -q "clicks found $((SECS - 1))\b" "$RUN/avsync.txt" || { echo "FAIL: not every click is in the WAV"; red=1; }
# caps.html's answers (Cobalt's log of that run, copied as caps/cobalt.log).
C="$RUN/caps/cobalt.log"
if [ -s "$C" ]; then
    grep -a '"caps: ' "$C" | sed 's/.*"caps: /  caps: /; s/", source.*//' > "$RUN/caps.txt"
    cat "$RUN/caps.txt"
    for want in 'yes video/webm; codecs="vp9"; width=1920; height=1080; framerate=60' \
                'yes video/webm; codecs="vp9"; width=1080; height=1920; framerate=30' \
                'yes video/webm; codecs="vp9"; width=1920; height=1088; framerate=30' \
                'no video/webm; codecs="vp9"; width=1921; height=1080; framerate=30' \
                'no video/webm; codecs="vp9"; width=2560; height=1440; framerate=30' \
                'no video/webm; codecs="vp9"; width=3840; height=2160; framerate=30' \
                'no video/webm; codecs="vp9"; width=7680; height=4320; framerate=30' \
                'no video/webm; codecs="vp09.02.51.10.01.09.16.09.00"; width=1920; height=1080' \
                'no audio/mp4; codecs="ec-3"' 'no audio/mp4; codecs="ac-3"' 'yes audio/webm; codecs="opus"'; do
        grep -qxF "  caps: $want" "$RUN/caps.txt" || { echo "FAIL: caps: want \"$want\""; red=1; }
    done
else
    echo "FAIL: no Cobalt log of the caps.html run"; red=1
fi
[ "$red" = 0 ] || exit 1
echo "yt avsync: green (numbers above are QEMU's: reported, not judged)"
