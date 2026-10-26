// BitsOS V2 Cobalt backend: the Linux framebuffer (fbdev) or a fake one.
//
// Real: FBIOGET_VSCREENINFO/FBIOGET_FSCREENINFO, then mmap of the whole
// framebuffer memory; drawing goes to the visible page (xoffset/yoffset).
// The page is checked again from time to time (Recheck), because fbcon may
// pan a framebuffer whose virtual height is larger than the screen. One
// buffer only: no page flip and no vsync (that needs the V2 DP contract).
// Fake (BITSOS_FB_FAKE=WxH): a plain file of W*H*4 bytes, 32-bit BGRX,
// mapped shared, so a test tool can read the picture at any time.

#ifndef STARBOARD_SHARED_BITSOS_FB_FB_DEVICE_H_
#define STARBOARD_SHARED_BITSOS_FB_FB_DEVICE_H_

#include <linux/fb.h>
#include <stddef.h>
#include <stdint.h>

#include <atomic>
#include <string>

#include "starboard/shared/bitsos/fb/fb_config.h"
#include "starboard/shared/bitsos/fb/fb_pixels.h"

namespace bitsos_fb {

// Linux's framebuffer device major number (linux/major.h FB_MAJOR); V2's
// fbdev must use it too.
constexpr unsigned kFbMajor = 29;

// A usable mode: the visible page inside the mapped memory.
struct FbGeometry {
  int width = 0;
  int height = 0;
  int pitch = 0;      // bytes per row (line_length; may be padded)
  size_t offset = 0;  // first byte of the visible page in the mapping
  PixelFormat format;
};

bool SameMode(const FbGeometry& a, const FbGeometry& b);

// Checks a mode as the kernel reports it against |map_size| bytes of mapped
// memory. False with a reason for anything that would draw outside it or
// that the backend cannot draw. Pure, so the tests feed it made-up modes.
bool ValidateGeometry(const fb_var_screeninfo& var,
                      const fb_fix_screeninfo& fix,
                      size_t map_size,
                      FbGeometry* out,
                      std::string* why);

// True if |path| is, or resolves into, a device or kernel folder (/dev,
// /proc, /sys); a fake framebuffer is never put there.
bool IsSystemPath(const std::string& path);

class FbDevice {
 public:
  enum class Change { kSame, kMoved, kLost };

  FbDevice() = default;
  FbDevice(const FbDevice&) = delete;
  FbDevice& operator=(const FbDevice&) = delete;
  ~FbDevice();

  // False with a reason in |error| (nothing stays open then).
  bool Open(const Config& config, std::string* error);
  void Close();

  bool is_open() const { return map_ != nullptr; }
  bool fake() const { return fake_; }
  int width() const { return geo_.width; }
  int height() const { return geo_.height; }
  int pitch() const { return geo_.pitch; }
  const PixelFormat& format() const { return geo_.format; }
  // First byte of the visible page.
  uint8_t* pixels() const { return pixels_.load(std::memory_order_acquire); }
  // One-line description for the log.
  std::string Describe() const;

  // Reads the mode again (real device; one ioctl pair). kMoved: the visible
  // page moved (pixels() changed, everything must be drawn again). kLost:
  // the mode changed under us (size or format); stop drawing.
  Change Recheck();
  // Takes |g| as the new mode; what Recheck does after reading it. Tests
  // call it on a fake to play a pan.
  Change ApplyGeometry(const FbGeometry& g);

 private:
  bool OpenReal(const std::string& path, std::string* error);
  bool OpenFake(const Config& config, std::string* error);

  int fd_ = -1;
  bool fake_ = false;
  void* map_ = nullptr;
  size_t map_size_ = 0;
  FbGeometry geo_;
  std::atomic<uint8_t*> pixels_{nullptr};
  std::atomic<int> pending_{0};  // a Change not yet reported by Recheck
};

}  // namespace bitsos_fb

#endif  // STARBOARD_SHARED_BITSOS_FB_FB_DEVICE_H_
