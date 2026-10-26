#!/usr/bin/env bash
# usage: nav-yt.sh <name> [open] : from the fresh-profile start screen: Get started -> Watch as
# guest -> search "big buck bunny blender" -> focus the official Blender 60fps upload
# (screenshot results.png); with "open" press Return to start it. Exit 1 if not found.
. /lane/user/ports/cobalt-launcher/scripts/yt-env.sh
R=$RUNS/$1; . $S/keys.sh
sleep ${NAV_WAIT:-20}
SHOT "$R/start.png"
K Return 5; K Down; K Down; K Down; K Return 9
SHOT "$R/guest.png"
for i in 1 2 3 4 5; do
  f=$(focus)
  echo "$f" | grep -q "YTLR-TEXT-BOX" && break
  if echo "$f" | grep -qi "voice\|mic"; then K Right 1.5; else K Up 1.5; fi
done
K Return 3
TYPE "big buck bunny blender"; sleep 7
found=0
for i in $(seq 1 9); do
  K Down 1
  if focus | grep -q "PLAYBACK-PREVIEW.*Big Buck Bunny 60fps 4K - Official Blender"; then found=1; break; fi
done
SHOT "$R/results.png"
[ $found = 1 ] || { echo "nav-yt: tile not found"; focus; exit 1; }
echo "nav-yt: tile focused"
[ "${2:-}" = open ] && { K Return 1; echo "nav-yt: opened"; }
exit 0
