// BitsOS V2 Cobalt backend: the Starboard application for fbdev + evdev.
//
// Same job as ApplicationX11, without X: windows are rectangles on the
// framebuffer (only the first is shown), input comes from evdev nodes, the
// UI arrives through dev.bitsos.Canvas and video frames through punch-out
// (AcceptFrame), and FbCompositor puts them together on the screen.

#ifndef STARBOARD_SHARED_BITSOS_FB_APPLICATION_BITSOS_FB_H_
#define STARBOARD_SHARED_BITSOS_FB_APPLICATION_BITSOS_FB_H_

#include <stdint.h>

#include <deque>
#include <map>
#include <memory>
#include <mutex>
#include <vector>

#include "starboard/common/rect.h"
#include "starboard/player.h"
#include "starboard/shared/bitsos/fb/evdev_decoder.h"
#include "starboard/shared/bitsos/fb/evdev_devices.h"
#include "starboard/shared/bitsos/fb/fb_compositor.h"
#include "starboard/shared/bitsos/fb/fb_config.h"
#include "starboard/shared/bitsos/fb/fb_device.h"
#include "starboard/shared/internal_only.h"
#include "starboard/shared/linux/time_zone_monitor.h"
#include "starboard/shared/starboard/player/filter/video_frame_internal.h"
#include "starboard/shared/starboard/queue_application.h"
#include "starboard/window.h"

namespace starboard {
namespace bitsos {

class ApplicationBitsosFb : public QueueApplication {
 public:
  explicit ApplicationBitsosFb(SbEventHandleCallback sb_event_handle_callback);
  ~ApplicationBitsosFb() override;

  static ApplicationBitsosFb* Get() {
    return static_cast<ApplicationBitsosFb*>(Application::Get());
  }

  SbWindow CreateWindow(const SbWindowOptions* options);
  bool DestroyWindow(SbWindow window);
  bool GetWindowSize(SbWindow window, SbWindowSize* size);
  void* GetPlatformHandle(SbWindow window);
  void PlayerSetBounds(SbPlayer player, int z_index, const Rect& rect);

  // dev.bitsos.Canvas Present (Chromium's compositor thread).
  void Present(void* window_handle, const void* pixels, int32_t stride,
               int32_t width, int32_t height, int32_t damage_x,
               int32_t damage_y, int32_t damage_width, int32_t damage_height);

 protected:
  void AcceptFrame(SbPlayer player,
                   const scoped_refptr<VideoFrame>& frame,
                   int z_index,
                   const Rect& rect) override;

  bool IsStartImmediate() override { return !HasPreloadSwitch(); }
  bool IsPreloadImmediate() override { return HasPreloadSwitch(); }

  void Initialize() override;
  void Teardown() override;

  bool MayHaveSystemEvents() override;
  Event* WaitForSystemEventWithTimeout(int64_t time) override;
  void WakeSystemEventWait() override;

 private:
  struct Window;

  std::shared_ptr<bitsos_fb::FbCompositor> Compositor();
  Event* TakePending();
  void SetGeometryLocked(int w, int h, const bitsos_fb::Placement& p);
  void SetKdGraphics(bool on);

  const bitsos_fb::Config config_;
  bitsos_fb::FbDevice fb_;
  bitsos_fb::EvdevDecoder decoder_;
  std::unique_ptr<bitsos_fb::EvdevDevices> inputs_;
  int wake_fd_ = -1;
  int tty_fd_ = -1;
  int64_t next_rescan_ = 0;
  std::deque<bitsos_fb::InEvent> pending_;

  // Windows and the compositor are reached from several threads.
  std::mutex mutex_;
  std::vector<Window*> windows_;
  std::shared_ptr<bitsos_fb::FbCompositor> compositor_;
  uint32_t next_handle_ = 1;
  bool fb_failed_ = false;
  // Set by CreateWindow/DestroyWindow (any thread), applied to the decoder
  // by the event thread, which owns it.
  bool geometry_changed_ = false;
  int geometry_w_ = 0;
  int geometry_h_ = 0;
  bitsos_fb::Placement geometry_placement_;
  int geometry_screen_w_ = 0;
  int geometry_screen_h_ = 0;
  // The frame last handed to the compositor, per player.
  std::map<uintptr_t, scoped_refptr<VideoFrame>> last_frames_;

  std::unique_ptr<::starboard::shared::linux::TimeZoneMonitor>
      time_zone_monitor_;
};

}  // namespace bitsos
}  // namespace starboard

#endif  // STARBOARD_SHARED_BITSOS_FB_APPLICATION_BITSOS_FB_H_
