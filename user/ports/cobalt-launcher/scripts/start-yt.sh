#!/usr/bin/env bash
# usage (in the container): start-yt.sh <name> <x11|fb> [cap-lines] [loader_app args...]
# Starts loader_app + our libcobalt on a fresh profile with the chosen backend and
# opens YouTube TV with formats above <cap-lines> (default 1080) hidden.
#   x11: X11 debug backend on Xvfb :290 (BITSOS_STUB_EGL=1, as in V1's M0)
#   fb:  V2 backend on a fake framebuffer <run>/fb0 (2560x1440 BGRX file) and a
#        fake evdev FIFO <run>/input (keys: evsend.py; picture: fbshot.py)
set -uo pipefail
. /lane/user/ports/cobalt-launcher/scripts/yt-env.sh
NAME=$1; BACK=$2; CAP=${3:-1080}; shift $(( $# >= 3 ? 3 : $# ))
R=$RUNS/$NAME
bash $S/stop-yt.sh "$NAME" >/dev/null 2>&1
rm -rf "$R"; mkdir -p "$R/home/.config/pulse"
yt_pulse
export HOME=$R/home
case $BACK in
  x11) yt_x; export BITSOS_STUB_EGL=1 ;;
  fb)  mkfifo "$R/input"
       export BITSOS_BACKEND=fb BITSOS_FB=$R/fb0 BITSOS_FB_FAKE=${YT_FB_SIZE:-2560x1440} \
              BITSOS_EVDEV=$R/input BITSOS_KEYMAP=de ;;
  *)   echo "backend x11 or fb" >&2; exit 2 ;;
esac
echo "$BACK" > "$R/backend"
cd "$OUT"
setsid ./loader_app --evergreen_lite --disable-dev-shm-usage --remote-debugging-port=$YT_PORT \
  --url=about:blank "$@" > "$R/cobalt.log" 2>&1 &
echo $! > "$R/loader.pid"
for i in $(seq 1 40); do curl -s localhost:$YT_PORT/json >/dev/null && break; sleep 1; done
curl -s localhost:$YT_PORT/json >/dev/null || { echo "start-yt: no DevTools on $YT_PORT"; tail -20 "$R/cobalt.log"; exit 1; }
sleep 2
sed "s/__CAP_H__/$CAP/; s#__BLOCK__#null#" $S/cap.js > "$R/cap.js"
setsid python3 $S/inject.py "$R/cap.js" https://www.youtube.com/tv > "$R/inject.log" 2>&1 &
echo $! > "$R/inject.pid"
sleep 3; cat "$R/inject.log"
echo "start-yt: $NAME running, backend $BACK, pid $(cat "$R/loader.pid")"
