# Sourced inside the lane container (cobalt-yt-v2). Lane YT ports and names:
# DevTools 29222, Xvfb display :290 (both in the YT range 29000-29499).
export YT_PORT=${YT_PORT:-29222}
export DISPLAY=:290
export OUT=/work/cobalt/src/out/evergreen-x64-bitsos_qa
export RUNS=/work/runs-yt
export S=/lane/user/ports/cobalt-launcher/scripts
export XDG_RUNTIME_DIR=/tmp/xdg-yt
mkdir -p "$RUNS" "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
yt_x() {  # virtual X screen for the X11 debug backend (nothing on Georg's desktop)
  pgrep -f "Xvfb :290" >/dev/null || { Xvfb :290 -screen 0 1920x1080x24 -nolisten tcp >"$RUNS/xvfb.log" 2>&1 & sleep 2; }
}
yt_pulse() {  # silent PulseAudio null sink; without it Starboard falls back to ALSA, then to a stub
  pgrep -x pulseaudio >/dev/null || pulseaudio --system=false --exit-idle-time=-1 -n \
    --load="module-null-sink sink_name=nul" --load="module-native-protocol-unix" --daemonize=yes \
    --log-target=file:"$RUNS/pulse.log" 2>/dev/null
  sleep 1
}
