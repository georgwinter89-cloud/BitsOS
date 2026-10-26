#!/usr/bin/env bash
# YT-5: the A/V sync test clip, made on the host with CachyOS's ffmpeg (nothing downloaded).
#   make-clip.sh OUTDIR [WxH] [FPS] [SECONDS]      default 1280x720 30 20
# OUTDIR/video.webm (VP9, black; one full white frame at every whole second from 1 s on) and
# OUTDIR/audio.webm (Opus 48 kHz stereo, silence; a 10 ms 1 kHz burst at every whole second from 1 s on),
# two files as YouTube serves them (one SourceBuffer each in avsync.html). The flash is frame number
# N = k * FPS exactly (geq's N, no float time), the burst starts at sample k * 48000.
set -euo pipefail
OUT=$1; SIZE=${2:-1280x720}; FPS=${3:-30}; SECS=${4:-20}
mkdir -p "$OUT"
ffmpeg -hide_banner -loglevel error -y -f lavfi -i "nullsrc=s=$SIZE:r=$FPS:d=$SECS,format=yuv420p" \
  -vf "geq=lum='if(eq(mod(N\,$FPS)\,0)*gte(N\,$FPS)\,235\,16)':cb=128:cr=128" \
  -c:v libvpx-vp9 -b:v 1M -deadline realtime -cpu-used 8 -g "$FPS" -keyint_min "$FPS" \
  -color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv \
  -an -f webm -dash 1 "$OUT/video.webm"
ffmpeg -hide_banner -loglevel error -y -f lavfi \
  -i "aevalsrc=exprs='0.5*sin(2*PI*1000*t)*lt(t-floor(t)\,0.01)*gte(t\,1)':s=48000:c=stereo:d=$SECS" \
  -c:a libopus -b:a 128k -vn -f webm -dash 1 "$OUT/audio.webm"
echo "make-clip: $OUT ($SIZE, $FPS fps, $SECS s): $(stat -c %s "$OUT/video.webm") + $(stat -c %s "$OUT/audio.webm") bytes"
