// BitsOS V2 Cobalt backend: pixel work for the framebuffer path.
//
// Plain C++17, no Starboard and no Linux headers, so the host tests build it
// with any compiler. Pixels of the window side (UI canvas, video frames, the
// composed frame) are 32-bit little-endian BGRA: A<<24 | R<<16 | G<<8 | B.

#ifndef STARBOARD_SHARED_BITSOS_FB_FB_PIXELS_H_
#define STARBOARD_SHARED_BITSOS_FB_FB_PIXELS_H_

#include <stddef.h>
#include <stdint.h>

#include <vector>

namespace bitsos_fb {

struct Rect {
  int x = 0;
  int y = 0;
  int w = 0;
  int h = 0;

  bool Empty() const { return w <= 0 || h <= 0; }
  bool operator==(const Rect& o) const {
    return x == o.x && y == o.y && w == o.w && h == o.h;
  }
};

Rect MakeRect(int x, int y, int w, int h);
Rect Intersect(const Rect& a, const Rect& b);
// Smallest rectangle holding both; an empty input is ignored.
Rect Union(const Rect& a, const Rect& b);

// How a framebuffer pixel is laid out. Only 32 bpp and 16 bpp with channel
// fields of at most 8 bits are supported (Validate says why not).
struct PixelFormat {
  int bits_per_pixel = 32;
  int red_offset = 16;
  int red_length = 8;
  int green_offset = 8;
  int green_length = 8;
  int blue_offset = 0;
  int blue_length = 8;

  bool Validate(const char** why) const;
  bool IsXrgb8888() const;
  int BytesPerPixel() const { return bits_per_pixel / 8; }
  uint32_t Pack(uint32_t bgrx) const;
};

// Where the window lands on the screen: scaled to fit, aspect kept, centered.
// 1920x1080 on 2560x1440 gives the full screen (factor 4/3).
struct Placement {
  int x = 0;
  int y = 0;
  int w = 0;
  int h = 0;
};
Placement FitWindow(int win_w, int win_h, int scr_w, int scr_h);

// Bilinear scaling of the composed window image onto the screen.
class Scaler {
 public:
  // False (and nothing set up) for sizes <= 0 or an empty placement.
  bool Init(int src_w, int src_h, const Placement& placement);

  int src_w() const { return src_w_; }
  int src_h() const { return src_h_; }
  const Placement& placement() const { return placement_; }

  // Screen pixels whose value depends on any source pixel in |src|.
  Rect SrcToDst(const Rect& src) const;

  // Screen coordinates -> window coordinates (for the pointer).
  void DstToSrc(double dx, double dy, double* sx, double* sy) const;

  // Writes the screen pixels of |dst| (clipped to the placement) from |src|
  // (src_w x src_h BGRA, |src_stride| pixels per row) into |fb|. A pixel's
  // value does not depend on |dst|, so partial updates leave no seams.
  // Rows are mixed first, then columns; 1080p -> 1440p (exactly 4/3) uses a
  // fixed-weight SSE2 kernel.
  void Render(const uint32_t* src,
              int src_stride,
              const Rect& dst,
              uint8_t* fb,
              int fb_pitch,
              const PixelFormat& format) const;

 private:
  int src_w_ = 0;
  int src_h_ = 0;
  Placement placement_;
  bool identity_ = false;
  bool four_thirds_ = false;
  // Per destination column/row (relative to the placement): the two source
  // indices and the weight of the second one (0..256).
  std::vector<int> x0_, x1_, wx_;
  std::vector<int> y0_, y1_, wy_;

  // Screen columns [from, from + n) (relative to the placement) of one row;
  // |alpha| is ORed into every result (0xFF000000 for an XRGB screen).
  void ScaleRow(const uint32_t* row, int from, int n, uint32_t* out,
                uint32_t alpha) const;

  // Row buffers, kept between calls (Render runs on one thread only).
  mutable std::vector<uint32_t> mixed_;
  mutable std::vector<uint32_t> line_;
};

// Fills |r| of a BGRA image with |value|.
void Fill(uint32_t* img, int stride, const Rect& r, uint32_t value);

// Draws a BGRA picture (|pic|, pic_w x pic_h, |pic_pitch| bytes per row)
// stretched into |dst| of the image, nearest neighbour, only inside |clip|.
// Used for video frames, which normally arrive 1:1.
void DrawStretched(const uint8_t* pic,
                   int pic_w,
                   int pic_h,
                   int pic_pitch,
                   const Rect& dst,
                   const Rect& clip,
                   uint32_t* img,
                   int stride);

// out = ui + out * (255 - ui.alpha) / 255 over |r|; |ui| is premultiplied.
// The result's alpha byte is 0.
void BlendUiOver(const uint32_t* ui, int ui_stride, uint32_t* out,
                 int out_stride, const Rect& r);

// Fills a rectangle of the framebuffer with black.
void ClearFb(uint8_t* fb, int fb_pitch, const PixelFormat& format,
             const Rect& r);

// The mouse pointer, drawn by the backend (the web app draws none).
// Hot spot is the top-left pixel; |scale| enlarges it (2 on 1440p).
constexpr int kCursorW = 12;
constexpr int kCursorH = 19;
int CursorScaleFor(int screen_h);
Rect CursorRect(int x, int y, int scale);
void DrawCursor(uint8_t* fb, int fb_pitch, const PixelFormat& format,
                int x, int y, int scale, const Rect& clip);

}  // namespace bitsos_fb

#endif  // STARBOARD_SHARED_BITSOS_FB_FB_PIXELS_H_
