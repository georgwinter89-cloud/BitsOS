// BitsOS V2 Cobalt backend: pixel work for the framebuffer path.

#include "starboard/shared/bitsos/fb/fb_pixels.h"

#include <string.h>

#include <algorithm>

#if defined(__SSE2__)
#include <emmintrin.h>
#endif

namespace bitsos_fb {
namespace {

// Rectangles come from other threads and other programs; all arithmetic on
// them is done in 64 bits so hostile values cannot overflow.
Rect FromEdges(long long l, long long t, long long r, long long b) {
  if (r <= l || b <= t) {
    return Rect();
  }
  return MakeRect(static_cast<int>(l), static_cast<int>(t),
                  static_cast<int>(r - l), static_cast<int>(b - t));
}

// Weighted mix of two BGRX pixels, |w| = weight of |b| in 0..256. Red and blue
// are mixed together in one multiply: each lane stays below 2^16.
inline uint32_t Lerp(uint32_t a, uint32_t b, uint32_t w) {
  const uint32_t iw = 256 - w;
  const uint32_t rb = ((a & 0xFF00FFu) * iw + (b & 0xFF00FFu) * w) >> 8;
  const uint32_t g = ((a & 0x00FF00u) * iw + (b & 0x00FF00u) * w) >> 8;
  return (rb & 0xFF00FFu) | (g & 0x00FF00u);
}

inline void Store(uint8_t* p, const PixelFormat& f, uint32_t bgrx) {
  if (f.bits_per_pixel == 32) {
    const uint32_t v = f.Pack(bgrx);
    memcpy(p, &v, 4);
  } else {
    const uint16_t v = static_cast<uint16_t>(f.Pack(bgrx));
    memcpy(p, &v, 2);
  }
}

// out[i] = mix of a[i] and b[i], |w| = weight of b in 0..256. Same rounding
// as Lerp, but the alpha byte is mixed too (it is ignored later).
void LerpRows(const uint32_t* a, const uint32_t* b, uint32_t w, uint32_t* out,
              int n) {
  int i = 0;
#if defined(__SSE2__)
  const __m128i z = _mm_setzero_si128();
  const __m128i wb = _mm_set1_epi16(static_cast<short>(w));
  const __m128i wa = _mm_set1_epi16(static_cast<short>(256 - w));
  for (; i + 4 <= n; i += 4) {
    const __m128i va = _mm_loadu_si128(reinterpret_cast<const __m128i*>(a + i));
    const __m128i vb = _mm_loadu_si128(reinterpret_cast<const __m128i*>(b + i));
    const __m128i lo = _mm_srli_epi16(
        _mm_add_epi16(_mm_mullo_epi16(_mm_unpacklo_epi8(va, z), wa),
                      _mm_mullo_epi16(_mm_unpacklo_epi8(vb, z), wb)),
        8);
    const __m128i hi = _mm_srli_epi16(
        _mm_add_epi16(_mm_mullo_epi16(_mm_unpackhi_epi8(va, z), wa),
                      _mm_mullo_epi16(_mm_unpackhi_epi8(vb, z), wb)),
        8);
    _mm_storeu_si128(reinterpret_cast<__m128i*>(out + i),
                     _mm_packus_epi16(lo, hi));
  }
#endif
  for (; i < n; ++i) {
    out[i] = Lerp(a[i], b[i], w);
  }
}

// ui (premultiplied) over d; the result's alpha byte is 0.
inline uint32_t BlendOne(uint32_t s, uint32_t d) {
  const uint32_t a = s >> 24;
  if (a == 255) {
    return s & 0xFFFFFFu;
  }
  if (a == 0) {
    // Fully transparent (the video hole): keep what is below. A colour here
    // would break the premultiplied rule, so it is ignored.
    return d & 0xFFFFFFu;
  }
  const uint32_t inv = 255 - a;
  uint32_t rb = (d & 0xFF00FFu) * inv + 0x800080u;
  rb = ((rb + ((rb >> 8) & 0xFF00FFu)) >> 8) & 0xFF00FFu;
  uint32_t g = ((d >> 8) & 0xFFu) * inv + 0x80u;
  g = ((g + (g >> 8)) >> 8) & 0xFFu;
  // Premultiplied: each colour of |s| is <= a, so no channel overflows.
  return ((s & 0xFFFFFFu) + (rb | (g << 8))) & 0xFFFFFFu;
}

void BuildAxis(int src, int dst, std::vector<int>* i0, std::vector<int>* i1,
               std::vector<int>* w) {
  i0->resize(dst);
  i1->resize(dst);
  w->resize(dst);
  const double step = static_cast<double>(src) / dst;
  for (int d = 0; d < dst; ++d) {
    double pos = (d + 0.5) * step - 0.5;
    if (pos < 0) {
      pos = 0;
    }
    int a = static_cast<int>(pos);
    if (a > src - 1) {
      a = src - 1;
    }
    int weight = static_cast<int>((pos - a) * 256.0 + 0.5);
    int b = a + 1;
    if (b > src - 1) {
      b = src - 1;
      weight = 0;
    }
    if (weight >= 256) {
      a = b;
      weight = 0;
    }
    (*i0)[d] = a;
    (*i1)[d] = weight ? b : a;
    (*w)[d] = weight;
  }
}

// First and last index d with i1[d] >= lo and i0[d] <= hi; false if none.
bool AxisRange(const std::vector<int>& i0, const std::vector<int>& i1, int lo,
               int hi, int* first, int* last) {
  auto f = std::lower_bound(i1.begin(), i1.end(), lo);
  auto l = std::upper_bound(i0.begin(), i0.end(), hi);
  if (f == i1.end() || l == i0.begin()) {
    return false;
  }
  *first = static_cast<int>(f - i1.begin());
  *last = static_cast<int>(l - i0.begin()) - 1;
  return *first <= *last;
}

const char* const kCursor[kCursorH] = {
    "X           ", "XX          ", "X.X         ", "X..X        ",
    "X...X       ", "X....X      ", "X.....X     ", "X......X    ",
    "X.......X   ", "X........X  ", "X.........X ", "X......XXXXX",
    "X...X..X    ", "X..XX..X    ", "X.X  X..X   ", "XX   X..X   ",
    "X     X..X  ", "      X..X  ", "       XX   ",
};

}  // namespace

Rect MakeRect(int x, int y, int w, int h) {
  Rect r;
  r.x = x;
  r.y = y;
  r.w = w;
  r.h = h;
  return r;
}

Rect Intersect(const Rect& a, const Rect& b) {
  if (a.Empty() || b.Empty()) {
    return Rect();
  }
  const long long l = std::max<long long>(a.x, b.x);
  const long long t = std::max<long long>(a.y, b.y);
  const long long r =
      std::min<long long>(static_cast<long long>(a.x) + a.w,
                          static_cast<long long>(b.x) + b.w);
  const long long bo =
      std::min<long long>(static_cast<long long>(a.y) + a.h,
                          static_cast<long long>(b.y) + b.h);
  return FromEdges(l, t, r, bo);
}

Rect Union(const Rect& a, const Rect& b) {
  if (a.Empty()) {
    return b.Empty() ? Rect() : b;
  }
  if (b.Empty()) {
    return a;
  }
  const long long l = std::min<long long>(a.x, b.x);
  const long long t = std::min<long long>(a.y, b.y);
  const long long r =
      std::max<long long>(static_cast<long long>(a.x) + a.w,
                          static_cast<long long>(b.x) + b.w);
  const long long bo =
      std::max<long long>(static_cast<long long>(a.y) + a.h,
                          static_cast<long long>(b.y) + b.h);
  // Clamp to int: a union of two valid int rectangles may not fit.
  const long long kMax = 0x3FFFFFFF;
  return FromEdges(std::max(l, -kMax), std::max(t, -kMax), std::min(r, kMax),
                   std::min(bo, kMax));
}

bool PixelFormat::Validate(const char** why) const {
  const char* reason = nullptr;
  if (bits_per_pixel != 32 && bits_per_pixel != 16) {
    reason = "only 32 and 16 bits per pixel are supported";
  } else {
    const int off[3] = {red_offset, green_offset, blue_offset};
    const int len[3] = {red_length, green_length, blue_length};
    for (int i = 0; i < 3 && !reason; ++i) {
      if (len[i] < 1 || len[i] > 8 || off[i] < 0 ||
          off[i] + len[i] > bits_per_pixel) {
        reason = "colour field outside the pixel or wider than 8 bits";
      }
    }
  }
  if (why) {
    *why = reason;
  }
  return reason == nullptr;
}

bool PixelFormat::IsXrgb8888() const {
  return bits_per_pixel == 32 && red_offset == 16 && red_length == 8 &&
         green_offset == 8 && green_length == 8 && blue_offset == 0 &&
         blue_length == 8;
}

uint32_t PixelFormat::Pack(uint32_t bgrx) const {
  if (IsXrgb8888()) {
    // The top byte is opaque in case the device reads it as alpha.
    return bgrx | 0xFF000000u;
  }
  const uint32_t r = (bgrx >> 16) & 0xFF;
  const uint32_t g = (bgrx >> 8) & 0xFF;
  const uint32_t b = bgrx & 0xFF;
  uint32_t v = ((r >> (8 - red_length)) << red_offset) |
               ((g >> (8 - green_length)) << green_offset) |
               ((b >> (8 - blue_length)) << blue_offset);
  if (bits_per_pixel == 32) {
    // Unused bits set, as for XRGB above.
    uint32_t used = (((1u << red_length) - 1) << red_offset) |
                    (((1u << green_length) - 1) << green_offset) |
                    (((1u << blue_length) - 1) << blue_offset);
    v |= ~used & 0xFF000000u;
  }
  return v;
}

Placement FitWindow(int win_w, int win_h, int scr_w, int scr_h) {
  Placement p;
  if (win_w <= 0 || win_h <= 0 || scr_w <= 0 || scr_h <= 0) {
    return p;
  }
  // Compare win_w/win_h with scr_w/scr_h without rounding.
  const long long lhs = static_cast<long long>(scr_w) * win_h;
  const long long rhs = static_cast<long long>(scr_h) * win_w;
  if (lhs <= rhs) {
    p.w = scr_w;
    p.h = static_cast<int>(static_cast<long long>(scr_w) * win_h / win_w);
  } else {
    p.h = scr_h;
    p.w = static_cast<int>(static_cast<long long>(scr_h) * win_w / win_h);
  }
  p.w = std::max(1, std::min(p.w, scr_w));
  p.h = std::max(1, std::min(p.h, scr_h));
  p.x = (scr_w - p.w) / 2;
  p.y = (scr_h - p.h) / 2;
  return p;
}

bool Scaler::Init(int src_w, int src_h, const Placement& placement) {
  src_w_ = src_h_ = 0;
  if (src_w <= 0 || src_h <= 0 || placement.w <= 0 || placement.h <= 0) {
    return false;
  }
  src_w_ = src_w;
  src_h_ = src_h;
  placement_ = placement;
  identity_ = src_w == placement.w && src_h == placement.h;
  four_thirds_ = static_cast<long long>(src_w) * 4 ==
                     static_cast<long long>(placement.w) * 3 &&
                 src_w >= 8;
  BuildAxis(src_w, placement.w, &x0_, &x1_, &wx_);
  BuildAxis(src_h, placement.h, &y0_, &y1_, &wy_);
  return true;
}

Rect Scaler::SrcToDst(const Rect& src) const {
  const Rect s = Intersect(src, MakeRect(0, 0, src_w_, src_h_));
  if (s.Empty()) {
    return Rect();
  }
  int x0, x1, y0, y1;
  if (!AxisRange(x0_, x1_, s.x, s.x + s.w - 1, &x0, &x1) ||
      !AxisRange(y0_, y1_, s.y, s.y + s.h - 1, &y0, &y1)) {
    return Rect();
  }
  return MakeRect(placement_.x + x0, placement_.y + y0, x1 - x0 + 1,
                  y1 - y0 + 1);
}

void Scaler::DstToSrc(double dx, double dy, double* sx, double* sy) const {
  if (src_w_ <= 0) {
    *sx = *sy = 0;
    return;
  }
  double x = (dx - placement_.x) * src_w_ / placement_.w;
  double y = (dy - placement_.y) * src_h_ / placement_.h;
  *sx = std::min(std::max(x, 0.0), static_cast<double>(src_w_ - 1));
  *sy = std::min(std::max(y, 0.0), static_cast<double>(src_h_ - 1));
}

void Scaler::ScaleRow(const uint32_t* row, int from, int n, uint32_t* out,
                      uint32_t alpha) const {
  int d = from;
  const int end = from + n;
#if defined(__SSE2__)
  if (four_thirds_) {
    // Screen pixels 4k..4k+3 mix source pairs (3k-1,3k) .. (3k+2,3k+3)
    // with the second one weighted 7/8, 5/8, 3/8, 1/8: two overlapping
    // loads give all four pairs at once. Groups at the edges, where the
    // tables clamp, go through the general loop below.
    const __m128i z = _mm_setzero_si128();
    const __m128i a_or = _mm_set1_epi32(static_cast<int>(alpha));
    const __m128i wb_lo = _mm_set_epi16(160, 160, 160, 160, 224, 224, 224, 224);
    const __m128i wa_lo = _mm_set_epi16(96, 96, 96, 96, 32, 32, 32, 32);
    const __m128i wb_hi = _mm_set_epi16(32, 32, 32, 32, 96, 96, 96, 96);
    const __m128i wa_hi = _mm_set_epi16(224, 224, 224, 224, 160, 160, 160, 160);
    const int last_group = (src_w_ - 4) / 3;  // 3k + 3 <= src_w - 1
    for (; d < end && (d % 4 != 0 || d < 4); ++d) {
      out[d - from] = Lerp(row[x0_[d]], row[x1_[d]], wx_[d]) | alpha;
    }
    for (; d + 4 <= end && d / 4 <= last_group; d += 4) {
      const int s = 3 * (d / 4);
      const __m128i a =
          _mm_loadu_si128(reinterpret_cast<const __m128i*>(row + s - 1));
      const __m128i b =
          _mm_loadu_si128(reinterpret_cast<const __m128i*>(row + s));
      const __m128i lo = _mm_srli_epi16(
          _mm_add_epi16(_mm_mullo_epi16(_mm_unpacklo_epi8(a, z), wa_lo),
                        _mm_mullo_epi16(_mm_unpacklo_epi8(b, z), wb_lo)),
          8);
      const __m128i hi = _mm_srli_epi16(
          _mm_add_epi16(_mm_mullo_epi16(_mm_unpackhi_epi8(a, z), wa_hi),
                        _mm_mullo_epi16(_mm_unpackhi_epi8(b, z), wb_hi)),
          8);
      _mm_storeu_si128(reinterpret_cast<__m128i*>(out + (d - from)),
                       _mm_or_si128(_mm_packus_epi16(lo, hi), a_or));
    }
  }
#endif
  for (; d < end; ++d) {
    out[d - from] = Lerp(row[x0_[d]], row[x1_[d]], wx_[d]) | alpha;
  }
}

namespace {

// out[i] = in[i] | alpha.
void CopyOr(const uint32_t* in, uint32_t* out, int n, uint32_t alpha) {
  int i = 0;
#if defined(__SSE2__)
  const __m128i a = _mm_set1_epi32(static_cast<int>(alpha));
  for (; i + 4 <= n; i += 4) {
    _mm_storeu_si128(
        reinterpret_cast<__m128i*>(out + i),
        _mm_or_si128(_mm_loadu_si128(reinterpret_cast<const __m128i*>(in + i)),
                     a));
  }
#endif
  for (; i < n; ++i) {
    out[i] = in[i] | alpha;
  }
}

}  // namespace

void Scaler::Render(const uint32_t* src,
                    int src_stride,
                    const Rect& dst,
                    uint8_t* fb,
                    int fb_pitch,
                    const PixelFormat& format) const {
  if (src_w_ <= 0) {
    return;
  }
  const Rect area = MakeRect(placement_.x, placement_.y, placement_.w,
                             placement_.h);
  const Rect d = Intersect(dst, area);
  if (d.Empty()) {
    return;
  }
  const int bpp = format.BytesPerPixel();
  // An XRGB screen is written directly; other layouts go through line_.
  const bool xrgb = format.IsXrgb8888();
  const uint32_t alpha = xrgb ? 0xFF000000u : 0;
  const int rx0 = d.x - placement_.x;
  // Source columns this rectangle reads.
  const int c0 = identity_ ? rx0 : x0_[rx0];
  const int c1 = identity_ ? rx0 + d.w - 1 : x1_[rx0 + d.w - 1];
  if (!identity_ && mixed_.size() < static_cast<size_t>(src_w_)) {
    mixed_.resize(src_w_);
  }
  if (!xrgb && line_.size() < static_cast<size_t>(d.w)) {
    line_.resize(d.w);
  }
  for (int y = d.y; y < d.y + d.h; ++y) {
    const int ry = y - placement_.y;
    uint8_t* out = fb + static_cast<size_t>(y) * fb_pitch +
                   static_cast<size_t>(d.x) * bpp;
    uint32_t* target = xrgb ? reinterpret_cast<uint32_t*>(out) : line_.data();
    if (identity_) {
      CopyOr(src + static_cast<size_t>(ry) * src_stride + rx0, target, d.w,
             alpha);
    } else {
      const uint32_t* row = src + static_cast<size_t>(y0_[ry]) * src_stride;
      if (wy_[ry]) {
        LerpRows(row + c0, src + static_cast<size_t>(y1_[ry]) * src_stride + c0,
                 static_cast<uint32_t>(wy_[ry]), mixed_.data() + c0,
                 c1 - c0 + 1);
        row = mixed_.data();
      }
      ScaleRow(row, rx0, d.w, target, alpha);
    }
    if (!xrgb) {
      for (int i = 0; i < d.w; ++i, out += bpp) {
        Store(out, format, line_[i]);
      }
    }
  }
}

void Fill(uint32_t* img, int stride, const Rect& r, uint32_t value) {
  for (int y = r.y; y < r.y + r.h; ++y) {
    std::fill_n(img + static_cast<size_t>(y) * stride + r.x, r.w, value);
  }
}

void DrawStretched(const uint8_t* pic,
                   int pic_w,
                   int pic_h,
                   int pic_pitch,
                   const Rect& dst,
                   const Rect& clip,
                   uint32_t* img,
                   int stride) {
  if (!pic || pic_w <= 0 || pic_h <= 0 || pic_pitch < pic_w * 4) {
    return;
  }
  const Rect c = Intersect(dst, clip);
  if (c.Empty()) {
    return;
  }
  std::vector<int> sx(c.w);
  for (int i = 0; i < c.w; ++i) {
    sx[i] = static_cast<int>(static_cast<long long>(c.x + i - dst.x) * pic_w /
                             dst.w);
  }
  const bool same_w = pic_w == dst.w;
  for (int y = c.y; y < c.y + c.h; ++y) {
    const int syy = static_cast<int>(
        static_cast<long long>(y - dst.y) * pic_h / dst.h);
    const uint32_t* row = reinterpret_cast<const uint32_t*>(
        pic + static_cast<size_t>(syy) * pic_pitch);
    uint32_t* out = img + static_cast<size_t>(y) * stride + c.x;
    if (same_w) {
      memcpy(out, row + sx[0], static_cast<size_t>(c.w) * 4);
    } else {
      for (int i = 0; i < c.w; ++i) {
        out[i] = row[sx[i]];
      }
    }
  }
}

void BlendUiOver(const uint32_t* ui, int ui_stride, uint32_t* out,
                 int out_stride, const Rect& r) {
  for (int y = r.y; y < r.y + r.h; ++y) {
    const uint32_t* u = ui + static_cast<size_t>(y) * ui_stride + r.x;
    uint32_t* o = out + static_cast<size_t>(y) * out_stride + r.x;
    int i = 0;
#if defined(__SSE2__)
    // Most of a frame is opaque UI or the fully transparent video hole.
    const __m128i amask = _mm_set1_epi32(static_cast<int>(0xFF000000u));
    const __m128i rgb = _mm_set1_epi32(0x00FFFFFF);
    const __m128i zero = _mm_setzero_si128();
    for (; i + 4 <= r.w; i += 4) {
      const __m128i s =
          _mm_loadu_si128(reinterpret_cast<const __m128i*>(u + i));
      const __m128i a = _mm_and_si128(s, amask);
      __m128i* p = reinterpret_cast<__m128i*>(o + i);
      if (_mm_movemask_epi8(_mm_cmpeq_epi32(a, amask)) == 0xFFFF) {
        _mm_storeu_si128(p, _mm_and_si128(s, rgb));
      } else if (_mm_movemask_epi8(_mm_cmpeq_epi32(a, zero)) == 0xFFFF) {
        _mm_storeu_si128(p, _mm_and_si128(_mm_loadu_si128(p), rgb));
      } else {
        for (int k = 0; k < 4; ++k) {
          o[i + k] = BlendOne(u[i + k], o[i + k]);
        }
      }
    }
#endif
    for (; i < r.w; ++i) {
      o[i] = BlendOne(u[i], o[i]);
    }
  }
}

void ClearFb(uint8_t* fb, int fb_pitch, const PixelFormat& format,
             const Rect& r) {
  const int bpp = format.BytesPerPixel();
  for (int y = r.y; y < r.y + r.h; ++y) {
    uint8_t* out = fb + static_cast<size_t>(y) * fb_pitch +
                   static_cast<size_t>(r.x) * bpp;
    for (int x = 0; x < r.w; ++x, out += bpp) {
      Store(out, format, 0);
    }
  }
}

int CursorScaleFor(int screen_h) {
  return std::max(1, std::min(4, screen_h / 720));
}

Rect CursorRect(int x, int y, int scale) {
  return MakeRect(x, y, kCursorW * scale, kCursorH * scale);
}

void DrawCursor(uint8_t* fb, int fb_pitch, const PixelFormat& format,
                int x, int y, int scale, const Rect& clip) {
  scale = std::max(1, scale);
  const Rect c = Intersect(CursorRect(x, y, scale), clip);
  const int bpp = format.BytesPerPixel();
  for (int py = c.y; py < c.y + c.h; ++py) {
    const char* line = kCursor[(py - y) / scale];
    for (int px = c.x; px < c.x + c.w; ++px) {
      const char ch = line[(px - x) / scale];
      if (ch == ' ') {
        continue;
      }
      Store(fb + static_cast<size_t>(py) * fb_pitch +
                static_cast<size_t>(px) * bpp,
            format, ch == 'X' ? 0x000000u : 0xFFFFFFu);
    }
  }
}

}  // namespace bitsos_fb
