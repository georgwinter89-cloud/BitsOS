#!/usr/bin/env bash
# Inner part of fb-e2e.sh (runs inside the container).
if [ ! -d /lane ]; then echo "SKIP: runs inside the cobalt-yt-v2 container (started by fb-e2e.sh)"; exit 0; fi
. /lane/user/ports/cobalt-launcher/scripts/yt-env.sh
R=$RUNS/fb-e2e; rm -rf "$R"; mkdir -p "$R/home"
fail=0
check() { if eval "$2"; then echo "ok   $1"; else echo "FAIL $1"; fail=1; fi; }
cd /lane/host/tests/yt/e2e
python3 -m http.server 29080 --bind 127.0.0.1 > "$R/http.log" 2>&1 &
HTTP=$!
mkfifo "$R/input"
export HOME=$R/home BITSOS_BACKEND=fb BITSOS_FB=$R/fb0 BITSOS_FB_FAKE=2560x1440 \
       BITSOS_EVDEV=$R/input BITSOS_KEYMAP=de
cd "$OUT"
./loader_app --evergreen_lite --disable-dev-shm-usage --remote-debugging-port=$YT_PORT \
  --url=http://127.0.0.1:29080/keys.html > "$R/cobalt.log" 2>&1 &
APP=$!
for i in $(seq 1 40); do
  python3 $S/cdp.py "document.title" 2>/dev/null | grep -q "input test" && break; sleep 1
done
sleep 3
EV() { python3 $S/evsend.py "$R/input" --delay 0.3 "$@"; }
logjs() { python3 $S/cdp.py "window.__log.join('|')"; }
python3 $S/fbshot.py "$R/fb0" 2560x1440 "$R/page.png" --half >/dev/null
px() { python3 - "$R/fb0" "$1" "$2" <<'PY'
import sys
f = open(sys.argv[1], "rb"); x, y = int(sys.argv[2]), int(sys.argv[3])
f.seek((y * 2560 + x) * 4); b = f.read(4); print("%02x%02x%02x" % (b[2], b[1], b[0]))
PY
}
check "page background on screen (#3366cc)" '[ "$(px 1800 1000)" = 3366cc ]'
check "orange box scaled 4/3 (480x270 -> 640x360)" '[ "$(px 630 350)" = ff8800 ] && [ "$(px 650 370)" = 3366cc ]'
EV Up Down Left Right Return Escape; sleep 1
L=$(logjs); echo "  keys: $L" | cut -c1-300
check "six TV keys" 'echo "$L" | grep -q "down ArrowUp 38.*down ArrowDown 40.*down ArrowLeft 37.*down ArrowRight 39.*down Enter 13.*down Escape 27"'
python3 $S/cdp.py "window.__log=[]" >/dev/null
EV "type:yz"; sleep 1
L=$(logjs)
check "QWERTZ: y and z typed as y and z" 'echo "$L" | grep -q "down y 89.*down z 90"'
python3 $S/cdp.py "window.__log=[]" >/dev/null
EV pad:a pad:b hat:right stick:up; sleep 1
L=$(logjs); echo "  pad: $L" | cut -c1-300
check "gamepad A/B/hat/stick as Enter/Escape/arrows" 'echo "$L" | grep -q "down Enter 13.*down Escape 27.*down ArrowRight 39.*down ArrowUp 38"'
python3 $S/cdp.py "window.__log=[]" >/dev/null
# The pointer starts in the middle of the screen (1280,720) = window (960,540);
# 400 screen pixels right and 200 up = window +300,-150.
EV mouse:400,-200 click wheel:-1; sleep 1
L=$(logjs); echo "  mouse: $L"
check "click lands at window (1260,390)" 'echo "$L" | grep -Eq "mousedown 0 12(59|60|61),3(89|90|91)"'
check "wheel down" 'echo "$L" | grep -q "wheel 1"'
python3 $S/fbshot.py "$R/fb0" 2560x1440 "$R/pointer.png" --half >/dev/null
check "pointer drawn at (1680,520)" '[ "$(px 1680 520)" = 000000 ] && [ "$(px 1682 526)" = ffffff ]'
grep -q "screen fake 2560x1440" "$R/cobalt.log"; check "log names the screen" '[ $? = 0 ]'
EV Escape; python3 $S/evsend.py "$R/input" sleep:0.2 2>/dev/null
python3 - "$R/input" <<'PY'
import os, struct, sys
fd = os.open(sys.argv[1], os.O_WRONLY | os.O_NONBLOCK)
r = lambda t, c, v: struct.pack("<qqHHi", 0, 0, t, c, v)
syn = r(0, 0, 0)
for code in (29, 56):  # Ctrl, Alt held
    os.write(fd, r(1, code, 1) + syn)
os.write(fd, r(1, 14, 1) + syn + r(1, 14, 0) + syn)  # Backspace
os.close(fd)
PY
for i in $(seq 1 20); do kill -0 $APP 2>/dev/null || break; sleep 1; done
check "Ctrl+Alt+Backspace ends Cobalt" '! kill -0 $APP 2>/dev/null'
kill $APP 2>/dev/null; kill $HTTP 2>/dev/null
wait $APP 2>/dev/null
check "no FATAL in the log" '! grep -q "FATAL\|Check failed" "$R/cobalt.log"'
[ $fail = 0 ] && echo "FB E2E: ALL PASSED" || echo "FB E2E: FAILED"
exit $fail
