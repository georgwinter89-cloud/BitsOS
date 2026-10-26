# Sourced by the session scripts: K <key> [pause], TYPE <text>, SHOT <png> for the run's backend.
BACK=$(cat "$R/backend")
K() {
  if [ "$BACK" = fb ]; then python3 $S/evsend.py "$R/input" "$1"; else xdotool key "$1"; fi
  sleep "${2:-0.7}"
}
TYPE() {
  if [ "$BACK" = fb ]; then python3 $S/evsend.py "$R/input" --delay 0.3 "type:$1"; else xdotool type --delay 300 "$1"; fi
}
SHOT() {
  if [ "$BACK" = fb ]; then python3 $S/fbshot.py "$R/fb0" "${YT_FB_SIZE:-2560x1440}" "$1" --half >/dev/null
  else xwd -root -silent | convert xwd:- -resize 50% png:"$1"; fi
}
focus() {
  python3 $S/cdp.py "(()=>{const a=document.activeElement; return (a.tagName+' '+(a.getAttribute('aria-label')||'')+' '+a.textContent).slice(0,160)})()"
}
stats() {
  python3 $S/cdp.py "$(cat $S/stats.js)"
}
