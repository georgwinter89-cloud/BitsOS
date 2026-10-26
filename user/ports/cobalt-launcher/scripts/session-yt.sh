#!/usr/bin/env bash
# usage (in the container): session-yt.sh <name> <x11|fb> <play-seconds>
# One measured session: start -> guest -> search -> results (idle 30 s) -> open the Blender
# upload -> play <play-seconds> with stats every 30 s -> screenshots -> stop.
. /lane/user/ports/cobalt-launcher/scripts/yt-env.sh
NAME=$1; BACK=$2; PLAY=${3:-120}
R=$RUNS/$NAME
log() { echo "$*" | tee -a "$R/measure.txt"; }
bash $S/start-yt.sh "$NAME" "$BACK" 1080 > /tmp/start-$NAME.txt 2>&1 || { cat /tmp/start-$NAME.txt; exit 1; }
cat /tmp/start-$NAME.txt
. $S/keys.sh
export PROC=loader_app
if ! bash $S/nav-yt.sh "$NAME" > "$R/nav.txt" 2>&1; then log "navigation failed: $(tail -3 "$R/nav.txt")"; bash $S/stop-yt.sh "$NAME"; exit 1; fi
log "== on results; idle 30 s"; python3 $S/thr.py 30 | head -1 | tee -a "$R/measure.txt"
K Return 1; log "== watch page opened $(date +%T)"
n=0
while [ $n -lt $PLAY ]; do sleep 30; n=$((n+30)); log "play +$n s: $(stats)"; done
SHOT "$R/playing.png"
log "== playing, cores over 20 s"; python3 $S/thr.py 20 | head -4 | tee -a "$R/measure.txt"
K Escape 3; SHOT "$R/after-back.png"
grep "SbPlayerCreate() called with" "$R/cobalt.log" | tail -1 | grep -o 'video mime "[^"]*"' | tee -a "$R/measure.txt"
kill -0 "$(cat "$R/loader.pid")" 2>/dev/null && log "loader_app still running" || log "loader_app NOT running"
grep -c "FATAL\|Check failed" "$R/cobalt.log" | xargs -I{} echo "fatal lines: {}" | tee -a "$R/measure.txt"
bash $S/stop-yt.sh "$NAME"
