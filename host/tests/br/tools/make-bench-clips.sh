#!/usr/bin/env bash
# BR-4a: the clips for page/bench.html ?mode=video (BR-4 on the Mini-PC: YouTube-like 1080p60 VP9 and AV1 with
# Opus, played by Brave's software decoders). Made locally by ffmpeg from its test sources (moving pattern, so
# every frame differs); never downloaded. Usage: make-bench-clips.sh [out-dir] [seconds]
# Default out-dir: /var/tmp/claude-v2/br-bench (serve it with page/bench.html copied next to it).
set -euo pipefail
out=${1:-/var/tmp/claude-v2/br-bench}
secs=${2:-70}
here=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$out"
cp "$here/page/bench.html" "$out/"
src=(-f lavfi -i "testsrc2=size=1920x1080:rate=60:duration=$secs" -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=$secs")
# YouTube's 1080p60 VP9 runs at about 4-6 Mbit/s; the same target for both codecs.
[ -f "$out/clip-vp9-1080p60.webm" ] || ffmpeg -hide_banner -loglevel error -y "${src[@]}" -c:v libvpx-vp9 -b:v 5M \
  -deadline realtime -cpu-used 8 -row-mt 1 -g 120 -c:a libopus -b:a 128k "$out/clip-vp9-1080p60.webm"
[ -f "$out/clip-av1-1080p60.webm" ] || ffmpeg -hide_banner -loglevel error -y "${src[@]}" -c:v libsvtav1 -b:v 5M \
  -preset 10 -g 120 -c:a libopus -b:a 128k "$out/clip-av1-1080p60.webm"
ls -l "$out"
