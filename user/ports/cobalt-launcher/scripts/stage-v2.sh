#!/usr/bin/env bash
# YT-2: builds the tree that `cargo xtask image` puts into \BITSOS2\cobalt\ on the stick (manifest line
# `tree cobalt host:/var/tmp/claude-v2/yt/cobalt/v2-stick/cobalt`) from YT-1's build output:
#   loader_app                    debug info stripped (37.6 -> 12.0 MB; the symbol table stays for backtraces)
#   app/cobalt/lib/libcobalt.so   as built (already without debug info, 123.7 MB)
#   app/cobalt/content/...        cobalt_shell.pak, fonts, ssl/certs (empty directories are left out by xtask)
#   fonts/                        the system font directory Starboard names next to the loader (<exe dir>/fonts,
#                                 starboard/linux/shared/system_get_path.cc:257-265; YT-4a: without it Cobalt logs
#                                 "Failed to open .../fonts/fonts.xml" and nplb's FontsTest fails); same 136 files
#                                 as the app's own, ~23 MB, as in the Linux build directory
#   LICENSES/                     YT-5: the licence texts of everything in libcobalt.so and loader_app (Chromium's
#                                 licenses.py over their GN dependencies, made by licenses-v2.sh into <copy>/lic) and
#                                 of the fonts; Crashpad's handler stays out (decision in plan/notes/yt-YT-5.md)
# xtask adds lib/ with the libraries loader_app needs beyond the initramfs set (libX11 & co., libasound).
# Run it again after a Cobalt rebuild (no rebuild here). The stage is replaced as a whole (new dir, rename).
set -euo pipefail
OUT=${COBALT_OUT:-/var/tmp/claude-v2/yt/cobalt/cobalt/src/out/evergreen-x64-bitsos_qa}
DEST=${COBALT_STAGE:-/var/tmp/claude-v2/yt/cobalt/v2-stick/cobalt}
LIC=${COBALT_LIC:-/var/tmp/claude-v2/yt/cobalt/lic}
for f in loader_app app/cobalt/lib/libcobalt.so app/cobalt/content/cobalt_shell.pak fonts/fonts.xml; do
  [ -f "$OUT/$f" ] || { echo "stage-v2: $OUT/$f is missing (YT-1's build)" >&2; exit 1; }
done
for f in libcobalt.txt loader_app.txt fonts.txt; do
  [ -s "$LIC/$f" ] || { echo "stage-v2: $LIC/$f is missing (run scripts/licenses-v2.sh)" >&2; exit 1; }
done
NEW=$DEST.new
rm -rf "$NEW" "$DEST.old"
mkdir -p "$NEW/app"
strip --strip-debug -o "$NEW/loader_app" "$OUT/loader_app"
cp -r --reflink=auto "$OUT/app/cobalt" "$NEW/app/cobalt"
cp -r --reflink=auto "$OUT/fonts" "$NEW/fonts"
# FFmpeg 7.1 for VP9/H.264/AAC (user/ports/ffmpeg-min), dlopened by Cobalt from its own lib/ (RPATH $ORIGIN/lib).
bash "$(dirname "$0")/../../ffmpeg-min/build.sh" >&2
mkdir -p "$NEW/lib"
cp -a "${FFMPEG_OUT:-${FFMPEG_SRC:-/var/tmp/claude-v2/ffmpeg}/out}"/lib/lib{avcodec,avformat,avutil,swresample}.so* "$NEW/lib/"
mkdir "$NEW/LICENSES"
cp "$LIC/libcobalt.txt" "$LIC/loader_app.txt" "$LIC/fonts.txt" "$NEW/LICENSES/"
# FFmpeg is LGPL 2.1+: its licence text goes with the libraries (desk W7 merge of the FFmpeg stage with YT-5).
tar xOf "${FFMPEG_SRC:-/var/tmp/claude-v2/ffmpeg}/ffmpeg-7.1.2.tar.xz" ffmpeg-7.1.2/COPYING.LGPLv2.1 >"$NEW/LICENSES/ffmpeg.txt"
cat >"$NEW/LICENSES/README.txt" <<'TXT'
Cobalt (YouTube on TV devices, https://cobalt.dev), version 27.lts.3, built for BitsOS V2 with the patches in
user/ports/cobalt-launcher/patches of the BitsOS V2 tree.
  libcobalt.txt   licences of Cobalt/Chromium and every third-party part built into app/cobalt/lib/libcobalt.so
  loader_app.txt  licences of Starboard and every third-party part built into loader_app
  fonts.txt       the fonts in fonts/ and app/cobalt/content/fonts/
lib/libav*.so and libswresample.so are FFmpeg 7.1.2 (LGPL 2.1+, ffmpeg.txt), built by user/ports/ffmpeg-min.
The other libraries in lib/ come from CachyOS packages (their licences: /usr/share/licenses on CachyOS).
TXT
if [ -e "$DEST" ]; then mv "$DEST" "$DEST.old"; fi
mv "$NEW" "$DEST"
rm -rf "$DEST.old"
echo "stage-v2: $DEST: $(find "$DEST" -type f | wc -l) files, $(du -sm "$DEST" | cut -f1) MB"
