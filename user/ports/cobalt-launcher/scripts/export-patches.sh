#!/usr/bin/env bash
# Writes patches/0007 (the V2 framebuffer/evdev backend) from the lane's Cobalt
# copy, then proves the whole series: 0001..0007 applied to the pristine files
# of tag 27.lts.3 give exactly the files in the copy. Runs on the host; reads
# the copy's git only (nothing in the copy or in Georg's tree is changed).
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
src=${COBALT_COPY:-/var/tmp/claude-v2/yt/cobalt}/cobalt/src
pdir=$here/patches
out=$(cd "$here/../../.." && pwd)/target/yt/patches
rm -rf "$out"; mkdir -p "$out"

# Files the backend patch (0007) adds or changes.
changed=(
  starboard/linux/shared/media_is_audio_supported.cc
  starboard/linux/shared/media_is_video_supported.cc
  starboard/linux/shared/system_get_extensions.cc
  starboard/linux/x64x11/BUILD.gn
  starboard/shared/alsa/alsa_audio_sink_type.cc
  starboard/linux/x64x11/run_starboard_main.cc
  starboard/linux/x64x11/shared/BUILD.gn
  starboard/shared/bitsos/egl_gles_stub.cc
  starboard/shared/bitsos/egl_gles_stub.h
  starboard/shared/starboard/player/filter/cpu_video_frame.h
  starboard/shared/starboard/player/filter/video_frame_internal.h
  starboard/shared/starboard/player/filter/video_render_algorithm_impl.cc
  starboard/shared/x11/player_set_bounds.cc
  starboard/shared/x11/window_create.cc
  starboard/shared/x11/window_destroy.cc
  starboard/shared/x11/window_get_platform_handle.cc
  starboard/shared/x11/window_get_size.cc
)
for f in "$src"/starboard/shared/bitsos/fb/*; do
  changed+=("starboard/shared/bitsos/fb/$(basename "$f")")
done

# Every file any patch touches, from the pristine tag.
mapfile -t touched < <(cat "$pdir"/000[1-6]-*.patch | sed -n 's|^+++ b/||p'; printf '%s\n' "${changed[@]}")
mapfile -t touched < <(printf '%s\n' "${touched[@]}" | sort -u)
pristine() {  # dir: the pristine versions of all touched files (missing = new)
  mkdir -p "$1"
  for f in "${touched[@]}"; do
    if git -C "$src" cat-file -e "HEAD:$f" 2>/dev/null; then
      mkdir -p "$1/$(dirname "$f")"
      git -C "$src" show "HEAD:$f" > "$1/$f"
    fi
  done
  git -C "$1" init -q
}

# 1) base = pristine + 0001..0006; 0007 = base -> the copy's current files.
pristine "$out/base"
for p in "$pdir"/000[1-6]-*.patch; do git -C "$out/base" apply "$p"; done
mkdir -p "$out/cur"
for f in "${changed[@]}"; do
  mkdir -p "$out/cur/$(dirname "$f")"
  cp "$src/$f" "$out/cur/$f"
done
p7=$pdir/0007-starboard-v2-framebuffer-evdev-backend.patch
{
  cat <<'EOF'
BitsOS V2 (YT-1): framebuffer + evdev backend for the Linux loader_app.

BITSOS_BACKEND=fb runs the same loader_app without X11: windows on fbdev
(FBIOGET_VSCREENINFO/FSCREENINFO + mmap, or a fake file), the 1080p canvas
from dev.bitsos.Canvas plus punch-out video composed and scaled 4/3 to
1440p in software (SSE2), input from evdev nodes or a test FIFO, ALSA/Pulse
sound unchanged. Without the variable the X11 debug backend of 0006 runs.
Also: AC-3/E-AC-3 reported unsupported (no Dolby decoder; ads stalled),
video above 1920x1080 (either orientation) and HDR reported unsupported (the
window is 1080p, the screen SDR; the core count rule offered 8K on 16
threads), the ALSA sink reporting frames as consumed when played (written
minus snd_pcm_delay, as the PulseAudio sink does) instead of when written,
so the audio clock follows the speaker (YT-5), the render algorithm's A/V
note for the backend's numbers (a weak hook, YT-5), and
VideoFrame::is_cpu_frame() so the backend checks a frame's
kind before casting (Cobalt builds without RTTI).
Applies on 27.lts.3 (3befd332) after 0001-0006.

EOF
  for f in "${changed[@]}"; do
    a=$out/base/$f; b=$out/cur/$f
    [ -e "$a" ] || a=/dev/null
    (cd "$out" && git diff --no-index --no-prefix -- "${a#$out/}" "cur/$f" || true) |
      sed -e "s|^--- base/|--- a/|; s|^+++ cur/|+++ b/|; s|^diff --git base/\([^ ]*\) cur/|diff --git a/\1 b/|" \
          -e "s|^diff --git cur/\([^ ]*\) cur/|diff --git a/\1 b/|" \
          -e "s|^+++ cur/|+++ b/|"
  done
} > "$p7"

# 2) proof: pristine + 0001..0007 == the copy, file by file.
pristine "$out/check"
for p in "$pdir"/000[1-7]-*.patch; do git -C "$out/check" apply "$p"; done
bad=0
for f in "${touched[@]}"; do
  if ! cmp -s "$out/check/$f" "$src/$f"; then echo "DIFFERS: $f"; bad=1; fi
done
[ $bad = 0 ] && echo "series 0001-0007 reproduces the copy (${#touched[@]} files)"
echo "wrote $p7 ($(grep -c '^+' "$p7") added lines)"
exit $bad
