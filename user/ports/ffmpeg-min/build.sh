#!/usr/bin/env bash
# A small FFmpeg 7.1 (LGPL, decoders only) for Cobalt on V2: Cobalt dlopens libavcodec/libavformat/libavutil for
# VP9, H.264 and AAC (YouTube live streams, some uploads) and accepts FFmpeg up to 7.x (its ffmpeg_includes end at
# 7.1.1); CachyOS ships 9.x. AV1 and Opus need nothing from here (Cobalt has dav1d and libopus inside).
# Source: https://ffmpeg.org/releases/ffmpeg-7.1.2.tar.xz, signature checked against the FFmpeg release key
# FCF9 86EA 15E6 E293 A564 4F10 B432 2F04 D676 58D8 (Georg allowed the download on 21.10.2026). The tarball is kept
# outside the repo in $FFMPEG_SRC (default /var/tmp/claude-v2/ffmpeg).
# Result: $FFMPEG_OUT/lib/{libavcodec,libavformat,libavutil,libswresample}.so*, about 5 MB, needing only libc/libm.
set -euo pipefail
SRC=${FFMPEG_SRC:-/var/tmp/claude-v2/ffmpeg}
OUT=${FFMPEG_OUT:-$SRC/out}
VER=7.1.2
WORK=$SRC/build
[ -f "$SRC/ffmpeg-$VER.tar.xz" ] || { echo "ffmpeg-min: $SRC/ffmpeg-$VER.tar.xz is missing" >&2; exit 1; }
if [ -f "$OUT/lib/libavcodec.so.61" ] && [ "$OUT/lib/libavcodec.so.61" -nt "$0" ]; then echo "ffmpeg-min: $OUT is current"; exit 0; fi
mkdir -p "$WORK"
tar xf "$SRC/ffmpeg-$VER.tar.xz" -C "$WORK"
cd "$WORK/ffmpeg-$VER"
./configure --prefix="$OUT" --enable-shared --disable-static --enable-pic --disable-autodetect --disable-everything \
  --disable-programs --disable-doc --disable-avdevice --disable-swscale --disable-postproc --disable-avfilter \
  --disable-network --disable-iconv \
  --enable-decoder=vp9,h264,aac,aac_latm --enable-parser=vp9,h264,aac \
  --enable-bsf=vp9_superframe_split,h264_mp4toannexb,aac_adtstoasc --enable-demuxer=mov,matroska --enable-protocol=file >/dev/null
make -j"$(nproc)" >/dev/null
make install >/dev/null
echo "ffmpeg-min: $OUT/lib: $(du -sh "$OUT/lib" | cut -f1)"
