#!/usr/bin/env bash
# usage: stop-yt.sh <name> : stops what start-yt.sh started for that run (by its PIDs only)
. /lane/user/ports/cobalt-launcher/scripts/yt-env.sh
R=$RUNS/$1
for f in inject.pid loader.pid; do
  [ -f "$R/$f" ] || continue
  p=$(cat "$R/$f"); kill "$p" 2>/dev/null
  for i in $(seq 1 20); do kill -0 "$p" 2>/dev/null || break; sleep 0.5; done
  kill -9 "$p" 2>/dev/null; rm -f "$R/$f"
done
echo "stop-yt: $1 stopped"
