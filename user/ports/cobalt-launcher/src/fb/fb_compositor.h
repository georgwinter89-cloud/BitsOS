// BitsOS V2 Cobalt backend: puts UI, video and pointer on the framebuffer.
//
// Cobalt hands over the UI (premultiplied BGRA, the window size, with a
// damage rectangle) from its compositor thread and video frames from the
// player thread. Both calls only copy or store and wake this compositor's
// own thread, which does the work for the damaged part:
//   black -> video frames (z order, punch-out) -> UI over it
//   -> scaled into the screen (4/3 for 1080p on 1440p) -> pointer on top.
// No thread of Cobalt ever waits for the screen beyond one short copy.

#ifndef STARBOARD_SHARED_BITSOS_FB_FB_COMPOSITOR_H_
#define STARBOARD_SHARED_BITSOS_FB_FB_COMPOSITOR_H_

#include <stdint.h>

#include <chrono>
#include <condition_variable>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#include "starboard/shared/bitsos/fb/fb_config.h"
#include "starboard/shared/bitsos/fb/fb_device.h"
#include "starboard/shared/bitsos/fb/fb_pixels.h"
#include "starboard/shared/bitsos/fb/fb_yuv.h"

namespace bitsos_fb {

// One decoded video frame, read on the compositor thread. GetYuv (preferred)
// hands out the YV12 planes; otherwise Get returns BGRA pixels (it may
// convert on first use). Both stay valid while the object lives; false if
// the frame cannot be shown that way.
class VideoImage {
 public:
  virtual ~VideoImage() = default;
  virtual bool GetYuv(YuvPlanes* planes) {
    (void)planes;
    return false;
  }
  virtual bool Get(const uint8_t** data, int* width, int* height,
                   int* pitch) = 0;
  // Presentation time in microseconds, < 0 if unknown (av_stats.h).
  virtual int64_t timestamp() const { return -1; }
  // The decoded frame's size (0 x 0 if unknown), for the numbers only.
  virtual void FrameSize(int* width, int* height) const {
    *width = 0;
    *height = 0;
  }
};

class FbCompositor {
 public:
  struct Stats {
    uint64_t presents = 0;
    uint64_t video_frames = 0;
    uint64_t passes = 0;
    uint64_t screen_pixels = 0;
    uint64_t compose_us = 0;  // building the window image
    uint64_t scale_us = 0;    // writing the screen
    uint64_t max_pass_us = 0;
    uint64_t pans = 0;   // the visible page moved and was drawn again
    bool lost = false;   // the screen mode changed: nothing drawn any more
  };
  typedef void (*LogFn)(const std::string& line);

  // |fb| must be open and outlive the compositor. With |log|, a line of
  // numbers is written every 10 s while something changes.
  FbCompositor(FbDevice* fb, int win_w, int win_h, CursorMode cursor,
               LogFn log = nullptr);
  FbCompositor(const FbCompositor&) = delete;
  FbCompositor& operator=(const FbCompositor&) = delete;
  ~FbCompositor();

  void Start();
  void Stop();

  int window_w() const { return win_w_; }
  int window_h() const { return win_h_; }
  const Scaler& scaler() const { return scaler_; }

  // dev.bitsos.Canvas Present: copies the damaged part of |pixels|.
  void Present(const void* pixels, int stride, int width, int height,
               const Rect& damage);

  // Video layers, keyed by the player. Bounds are window pixels; an empty
  // rectangle hides the layer. A null image removes the frame.
  void SetVideoBounds(uintptr_t player, int z_index, const Rect& bounds);
  void SetVideoFrame(uintptr_t player, std::shared_ptr<VideoImage> image);
  void RemoveVideo(uintptr_t player);

  // Pointer in screen pixels; |seen| = a mouse has been used.
  void SetPointer(int screen_x, int screen_y, bool seen);

  // Blocks until all handed-over work is on the screen (tests, shutdown).
  void WaitIdle();

  Stats stats();

 private:
  struct Video {
    int z = 0;
    Rect bounds;
    std::shared_ptr<VideoImage> image;
    bool fresh = false;  // |image| has not been on the screen yet
  };

  void Loop();
  // Follows a pan of the real framebuffer (lock held).
  void RecheckScreenLocked();
  void RenderDst(const Rect& dst);
  void DamageLocked(const Rect& src);
  void MaybeLogLocked();
  // Puts one video layer into composed_ inside |clip|; false if nothing
  // could be drawn.
  bool DrawVideo(const Video& v, const Rect& clip);
  bool CursorVisibleLocked() const;

  FbDevice* const fb_;
  const int win_w_;
  const int win_h_;
  const CursorMode cursor_mode_;
  const int cursor_scale_;
  Scaler scaler_;

  std::mutex mutex_;
  std::condition_variable wake_;
  std::condition_variable idle_;
  bool started_ = false;
  bool stop_ = false;
  bool busy_ = false;
  Rect damage_;
  std::vector<uint32_t> ui_;
  std::map<uintptr_t, Video> videos_;
  int cursor_x_ = 0;
  int cursor_y_ = 0;
  bool cursor_seen_ = false;
  bool cursor_dirty_ = false;
  Stats stats_;
  Stats logged_;  // stats_ at the last log line
  std::chrono::steady_clock::time_point last_log_;
  const LogFn log_;

  // Compositor thread only.
  std::vector<uint32_t> composed_;
  // A scaled video frame converted once (tile previews).
  std::vector<uint32_t> scratch_;
  std::shared_ptr<VideoImage> scratch_image_;  // held: no address reuse
  std::chrono::steady_clock::time_point last_recheck_;
  bool screen_lost_ = false;
  bool cursor_on_ = false;
  int cursor_on_x_ = 0;
  int cursor_on_y_ = 0;
  std::thread thread_;
};

}  // namespace bitsos_fb

#endif  // STARBOARD_SHARED_BITSOS_FB_FB_COMPOSITOR_H_
