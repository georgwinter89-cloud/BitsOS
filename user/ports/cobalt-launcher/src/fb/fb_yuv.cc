// BitsOS V2 Cobalt backend: YV12 (I420) video frames -> BGRX.

#include "starboard/shared/bitsos/fb/fb_yuv.h"

#include <string.h>

#if defined(__SSE2__)
#include <emmintrin.h>
#endif

namespace bitsos_fb {
namespace {

// BT.709 limited range, as starboard's ConvertTo:
//   r = 1.164 (y-16) + 1.793 (v-128)
//   g = 1.164 (y-16) - 0.213 (u-128) - 0.533 (v-128)
//   b = 1.164 (y-16) + 2.112 (u-128)
// Each term is (x << 6) * (coefficient * 1024) >> 16, which is exactly what
// _mm_mulhi_epi16 computes, so scalar and SSE2 agree to the bit.
constexpr int kY = 1192;
constexpr int kVr = 1836;
constexpr int kUg = 218;
constexpr int kVg = 546;
constexpr int kUb = 2163;

inline int Term(int x, int k) {
  return (x * 64 * k) >> 16;  // x * 64: shifting a negative is undefined
}

inline uint32_t Clamp8(int v) {
  return static_cast<uint32_t>(v < 0 ? 0 : (v > 255 ? 255 : v));
}

}  // namespace

bool YuvPlanes::Valid() const {
  const int cw = width / 2 + (width & 1);
  const int ch = height / 2 + (height & 1);
  return y && u && v && width > 0 && height > 0 && y_pitch >= width &&
         u_pitch >= cw && v_pitch >= cw && y_rows >= height && u_rows >= ch &&
         v_rows >= ch;
}

uint32_t YuvToBgrx(int y, int u, int v) {
  const int yc = Term(y - 16, kY);
  const int d = u - 128;
  const int e = v - 128;
  const int r = yc + Term(e, kVr);
  const int g = yc - Term(d, kUg) - Term(e, kVg);
  const int b = yc + Term(d, kUb);
  return (Clamp8(r) << 16) | (Clamp8(g) << 8) | Clamp8(b);
}

void ConvertYuvRect(const YuvPlanes& f, const Rect& src_in, uint32_t* out,
                    int out_stride, int dst_x, int dst_y) {
  if (!f.Valid()) {
    return;
  }
  const Rect src = Intersect(src_in, MakeRect(0, 0, f.width, f.height));
  if (src.Empty()) {
    return;
  }
  for (int row = src.y; row < src.y + src.h; ++row) {
    const uint8_t* yp = f.y + static_cast<size_t>(row) * f.y_pitch;
    const uint8_t* up = f.u + static_cast<size_t>(row / 2) * f.u_pitch;
    const uint8_t* vp = f.v + static_cast<size_t>(row / 2) * f.v_pitch;
    uint32_t* o = out + static_cast<size_t>(dst_y + row - src.y) * out_stride +
                  (dst_x - src.x);
    int x = src.x;
    const int end = src.x + src.w;
    if (x & 1) {
      o[x] = YuvToBgrx(yp[x], up[x / 2], vp[x / 2]);
      ++x;
    }
#if defined(__SSE2__)
    const __m128i z = _mm_setzero_si128();
    const __m128i k16 = _mm_set1_epi16(16);
    const __m128i k128 = _mm_set1_epi16(128);
    const __m128i ky = _mm_set1_epi16(kY);
    const __m128i kvr = _mm_set1_epi16(kVr);
    const __m128i kug = _mm_set1_epi16(kUg);
    const __m128i kvg = _mm_set1_epi16(kVg);
    const __m128i kub = _mm_set1_epi16(kUb);
    for (; x + 8 <= end; x += 8) {
      int u4, v4;
      memcpy(&u4, up + x / 2, 4);
      memcpy(&v4, vp + x / 2, 4);
      __m128i yy = _mm_unpacklo_epi8(
          _mm_loadl_epi64(reinterpret_cast<const __m128i*>(yp + x)), z);
      __m128i uu = _mm_cvtsi32_si128(u4);
      __m128i vv = _mm_cvtsi32_si128(v4);
      uu = _mm_unpacklo_epi8(_mm_unpacklo_epi8(uu, uu), z);
      vv = _mm_unpacklo_epi8(_mm_unpacklo_epi8(vv, vv), z);
      yy = _mm_slli_epi16(_mm_sub_epi16(yy, k16), 6);
      uu = _mm_slli_epi16(_mm_sub_epi16(uu, k128), 6);
      vv = _mm_slli_epi16(_mm_sub_epi16(vv, k128), 6);
      const __m128i yc = _mm_mulhi_epi16(yy, ky);
      const __m128i r = _mm_adds_epi16(yc, _mm_mulhi_epi16(vv, kvr));
      const __m128i g = _mm_subs_epi16(
          _mm_subs_epi16(yc, _mm_mulhi_epi16(uu, kug)),
          _mm_mulhi_epi16(vv, kvg));
      const __m128i b = _mm_adds_epi16(yc, _mm_mulhi_epi16(uu, kub));
      const __m128i b8 = _mm_packus_epi16(b, b);
      const __m128i g8 = _mm_packus_epi16(g, g);
      const __m128i r8 = _mm_packus_epi16(r, r);
      const __m128i bg = _mm_unpacklo_epi8(b8, g8);
      const __m128i ra = _mm_unpacklo_epi8(r8, z);
      _mm_storeu_si128(reinterpret_cast<__m128i*>(o + x),
                       _mm_unpacklo_epi16(bg, ra));
      _mm_storeu_si128(reinterpret_cast<__m128i*>(o + x + 4),
                       _mm_unpackhi_epi16(bg, ra));
    }
#endif
    for (; x < end; ++x) {
      o[x] = YuvToBgrx(yp[x], up[x / 2], vp[x / 2]);
    }
  }
}

}  // namespace bitsos_fb
