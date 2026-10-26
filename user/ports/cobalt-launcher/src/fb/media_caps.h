// BitsOS V2 Cobalt backend: the largest video Cobalt offers (YT-5).
//
// Used by starboard/linux/shared/media_is_video_supported.cc (patch 0007).
// The window is at most 1920x1080 (scaled to the screen in software) and every
// frame is decoded and converted on the CPU, so a larger stream costs only
// time; YouTube picks the largest size offered. Header-only, so the patched
// file needs no new GN dependency; host tests: host/tests/yt/media_caps_test.cc.

#ifndef STARBOARD_SHARED_BITSOS_FB_MEDIA_CAPS_H_
#define STARBOARD_SHARED_BITSOS_FB_MEDIA_CAPS_H_

namespace bitsos_fb {

constexpr int kMaxVideoLongSide = 1920;
// 1088: a 1080p stream coded in 16-pixel macroblocks may report its coded
// height. 1920 is already a multiple of 16, so the long side needs no margin.
constexpr int kMaxVideoShortSide = 1088;

// Either orientation: a 1080x1920 (vertical) stream costs what 1080p costs.
// Sizes of 0 (not known yet) pass, as upstream's rule lets them pass.
inline bool VideoSizeSupported(int width, int height) {
  const int long_side = width > height ? width : height;
  const int short_side = width > height ? height : width;
  return long_side <= kMaxVideoLongSide && short_side <= kMaxVideoShortSide;
}

}  // namespace bitsos_fb

#endif  // STARBOARD_SHARED_BITSOS_FB_MEDIA_CAPS_H_
