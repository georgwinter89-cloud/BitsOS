#!/bin/bash
# slow: YT-6: Cobalt started from the console shell, as Georg starts it. The console shell is init's (uid 0); MCP's
# own programs are uid 1000, so the scenario types into the console through MCP's keyboard (send_keys).
#   1. `cobalt URL` in the console shell: the launcher hands over to uid 1000 (chpst, AB-13's credentials) and says
#      so; loader_app runs with Uid 1000 1000 1000 1000; the avsync clip (host/tests/yt/avsync, 720p30 VP9 + Opus)
#      plays: SbPlayerCreate in Cobalt's log, frames_shown > 0 in `cobalt --stats`; the run directory is user
#      1000's; Ctrl+Alt+Backspace ends Cobalt and the launcher keeps the profile.
#   2. A desktop (bitscomp) started from the console as root runs as uid 1000 (WY-7), and so does a terminal in it:
#      `cobalt` typed while it runs hands the desktop over (ends it, Cobalt as uid 1000, starts it again (uid 1000)).
#   3. Then the start over MCP (uid 1000, as the desk starts it) still works in the run directory root used.
# QEMU: -smp 4, 4 GB, intel-hda + hda-duplex into a WAV file (nothing plays on the host); page and clip from a
# web server on the host's loopback (LO+82, QEMU's 10.0.2.2). Timing is never judged here. Missing Cobalt stage
# or ffmpeg: a SKIP line (YT6_REQUIRE=1 makes it red). Run with --script-timeout 900.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN="${BITSOS_RUN_DIR:-target/xtask/runs/yt-30-root}"
mkdir -p "$RUN"
RUN="$(realpath "$RUN")"
HERE="$(pwd)/host/tests/yt"
# shellcheck source=host/tests/mcp/lib.bash
. "$(pwd)/host/tests/mcp/lib.bash"
LANE="${BITSOS_LANE:-YT}"
LO="${BITSOS_PORT_LO:-29000}"
FWD=$((LO + 12))
WEB=$((LO + 82))
TOKEN="yt6-root-scenario-5e21"
NAME="$LANE-root"
SECS=20
COBALT_STAGE="${COBALT_STAGE:-/var/tmp/claude-v2/yt/cobalt/v2-stick/cobalt}"

if [ ! -f "$COBALT_STAGE/loader_app" ] || ! command -v ffmpeg >/dev/null; then
    echo "SKIP: no Cobalt stage (user/ports/cobalt-launcher/scripts/stage-v2.sh) or no ffmpeg on this host"
    [ "${YT6_REQUIRE:-0}" = 1 ] && { echo "FAIL: YT6_REQUIRE=1 and no stage/ffmpeg"; exit 1; }
    exit 0
fi
rm -rf "$RUN/clip" "$RUN/console" "$RUN/desk"
bash "$HERE/avsync/make-clip.sh" "$RUN/clip" 1280x720 30 "$SECS" > "$RUN/clip.log" 2>&1
cp "$HERE/avsync/avsync.html" "$RUN/clip/"
V2_STICK_TREES=cobalt cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"

stop_qemu "$NAME"
LOG="$RUN/serial.log"
rm -f "$LOG" "$RUN/out.wav"
python3 -I -m http.server "$WEB" --bind 127.0.0.1 --directory "$RUN/clip" > "$RUN/http.log" 2>&1 &
HPID=$!
cargo xtask qemu --name "$NAME" --net user --hostfwd "$FWD:8765" --timeout 600 --smp 4 --mem 4G --serial-log "$LOG" -- \
    -audiodev "wav,id=a0,path=$RUN/out.wav,out.frequency=48000,out.channels=2,out.format=s16" \
    -device intel-hda,id=h0 -device hda-duplex,bus=h0.0,audiodev=a0,mixer=off > "$RUN/qemu.out" 2>&1 &
QPID=$!
trap 'stop_run "$QPID" "$NAME"; kill "$HPID" 2>/dev/null || true' EXIT
red=0
python3 -I "$HERE/yt_mcp.py" "$FWD" "$TOKEN" wait 120
URL="http://10.0.2.2:$WEB/avsync.html"
echo "== 1. cobalt typed into the console shell (root)"
python3 -I "$HERE/yt_mcp.py" "$FWD" "$TOKEN" console "$RUN/console" "$URL" "$SECS" | tee "$RUN/console.out" || red=1
echo "== 2. cobalt while a desktop started from the console runs (root desktop)"
python3 -I "$HERE/yt_mcp.py" "$FWD" "$TOKEN" console "$RUN/desk" "$URL" "$SECS" desk | tee "$RUN/desk.out" || red=1
echo "== 3. cobalt started over MCP (uid 1000) after the root starts"
python3 -I "$HERE/yt_mcp.py" "$FWD" "$TOKEN" avsync "$RUN/mcp" "$URL" "$SECS" > "$RUN/mcp.out" 2>&1 || red=1
tail -3 "$RUN/mcp.out"
stop_run "$QPID" "$NAME"
kill "$HPID" 2>/dev/null || true
trap - EXIT

need() { if eval "$2"; then echo "ok: $1"; else echo "FAIL: $1"; red=1; fi; }
for p in console desk; do
    need "$p: SbPlayerCreate in Cobalt's log" 'grep -aq "SbPlayerCreate() called" "$RUN/$p/cobalt.log"'
    need "$p: frames shown" 'grep -Eq "frames_shown=[1-9]" "$RUN/$p/av-stats.txt"'
    need "$p: loader_app ran as uid 1000" 'grep -q "Uid 1000 1000 1000 1000$" "$RUN/$p.out"'
    need "$p: the run directory is user 1000's" 'grep -Eq "home, log: 1000:1000/700 1000:1000/[0-7]+ 1000:1000/[0-7]+ 1000:1000/[0-7]+$" "$RUN/$p.out"'
done
need "console: the launcher said it hands over to uid 1000" 'grep -q "cobalt: started by root: Cobalt runs as uid 1000 (chpst -u 1000:1000)" "$RUN/console/console.txt"'
need "console: Cobalt ended by Ctrl+Alt+Backspace, profile kept" 'grep -q "cobalt: Cobalt ended (code 0)" "$RUN/console/console.txt" && grep -q "cobalt: profile saved on the stick" "$RUN/console/console.txt"'
need "mcp: Cobalt played after the root starts" 'grep -aq "SbPlayerCreate() called" "$RUN/mcp/cobalt.log" && grep -Eq "frames_shown=[1-9]" "$RUN/mcp/av-stats.txt"'
need "desk: the desktop was handed over and started again" 'grep -q "cobalt: desktop ended" "$RUN/desk/launcher.log" && grep -q "cobalt: desktop started again" "$RUN/desk/launcher.log"'
need "desk: the desktop runs again as uid 1000" 'grep -Eq "^desktop after Cobalt: pid [0-9 ]+ Uid 1000 1000 1000 1000$" "$RUN/desk.out"'
need "desk: the root desktop ended before the MCP start" '! grep -q "RED: the root desktop still runs" "$RUN/desk.out"'
need "desk: no desktop start line in user 1000's run directory" '! grep -q "^desk-files-in-run: [^-]" "$RUN/desk.out"'
if grep -aq "lock.violation" "$LOG"; then echo "FAIL: lock order broken"; red=1; fi
if grep -aq "PANIC" "$LOG"; then echo "FAIL: a panic"; red=1; fi
if grep -aq "$TOKEN" "$LOG"; then echo "FAIL: the token appears in the log (R8i)"; red=1; fi
[ "$red" = 0 ] || exit 1
echo "yt root: green (Cobalt from the root console and from a root desktop runs as uid 1000 and plays)"
