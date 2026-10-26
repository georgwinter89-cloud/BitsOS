// BitsOS V2 Cobalt backend: YV12 (I420) video frames -> BGRX.
//
// Same colour formula as Starboard's CpuVideoFrame::ConvertTo (BT.709,
// limited range), but without a per-frame 8 MB allocation and with SSE2, and
// straight into the composed window image, only for the rows and columns
// that are needed.

#ifndef STARBOARD_SHARED_BITSOS_FB_FB_YUV_H_
#define STARBOARD_SHARED_BITSOS_FB_FB_YUV_H_

#include <stdint.h>

#include "starboard/shared/bitsos/fb/fb_pixels.h"

namespace bitsos_fb {

struct YuvPlanes {
  const uint8_t* y = nullptr;
  const uint8_t* u = nullptr;
  const uint8_t* v = nullptr;
  int y_pitch = 0;
  int u_pitch = 0;
  int v_pitch = 0;
  int width = 0;   // of the luma plane; chroma is half size, rounded up
  int height = 0;
  // Rows each plane really has (its buffer holds rows * pitch bytes).
  int y_rows = 0;
  int u_rows = 0;
  int v_rows = 0;

  // The planes hold every pixel of width x height (chroma: half, rounded
  // up), so that no read can go past them.
  bool Valid() const;
};

// Converts the frame pixels |src| (frame coordinates, clipped to the frame)
// into |out| at (dst_x, dst_y) for the top-left of |src|.
void ConvertYuvRect(const YuvPlanes& f, const Rect& src, uint32_t* out,
                    int out_stride, int dst_x, int dst_y);

// One pixel, scalar; the SSE2 path gives exactly the same values.
uint32_t YuvToBgrx(int y, int u, int v);

}  // namespace bitsos_fb

#endif  // STARBOARD_SHARED_BITSOS_FB_FB_YUV_H_
