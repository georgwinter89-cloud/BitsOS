// BitsOS V2 Cobalt backend: the Starboard application for fbdev + evdev.

#include "starboard/shared/bitsos/fb/application_bitsos_fb.h"

#include <errno.h>
#include <fcntl.h>
#include <linux/kd.h>
#include <math.h>
#include <poll.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#include <algorithm>

#include "starboard/common/log.h"
#include "starboard/common/time.h"
#include "starboard/event.h"
#include "starboard/input.h"
#include "starboard/key.h"
#include "starboard/shared/bitsos/fb/backend_fb.h"
#include "starboard/shared/bitsos/fb/console_guard.h"
#include "starboard/shared/linux/system_network_status.h"
#include "starboard/shared/starboard/audio_sink/audio_sink_internal.h"
#include "starboard/shared/starboard/player/filter/cpu_video_frame.h"

namespace starboard {
namespace bitsos {
namespace {

// The decoder speaks SbKey values without including Starboard; make sure
// they are the same numbers.
static_assert(kSbKeyReturn == 0x0D && kSbKeyEscape == 0x1B, "SbKey");
static_assert(kSbKeyLeft == 0x25 && kSbKeyUp == 0x26 && kSbKeyRight == 0x27 &&
                  kSbKeyDown == 0x28,
              "SbKey arrows");
static_assert(kSbKeyA == 0x41 && kSbKey0 == 0x30 && kSbKeyF1 == 0x70, "SbKey");
static_assert(kSbKeyMediaPlayPause == 0xB3 && kSbKeyMediaRewind == 0xE3 &&
                  kSbKeyMediaFastForward == 0xE4 && kSbKeyBrowserBack == 0xA6,
              "SbKey media");
static_assert(kSbKeyMouse1 == 0x7000 && kSbKeyMouse3 == 0x7002, "SbKey mouse");
static_assert(kSbKeyModifiersAlt == bitsos_fb::kModAlt &&
                  kSbKeyModifiersCtrl == bitsos_fb::kModCtrl &&
                  kSbKeyModifiersMeta == bitsos_fb::kModMeta &&
                  kSbKeyModifiersShift == bitsos_fb::kModShift &&
                  kSbKeyModifiersPointerButtonLeft ==
                      bitsos_fb::kModButtonLeft &&
                  kSbKeyModifiersPointerButtonRight ==
                      bitsos_fb::kModButtonRight &&
                  kSbKeyModifiersPointerButtonMiddle ==
                      bitsos_fb::kModButtonMiddle,
              "SbKeyModifiers");

constexpr int64_t kRescanUsec = 2'000'000;

enum DeviceId { kKeyboardId = 1, kMouseId = 2, kRemoteId = 3 };

void LogLine(const std::string& line) {
  SB_LOG(INFO) << "bitsos-fb: " << line;
}

// One punch-out frame; converted to BGRA once, on the compositor thread.
// Only CPU frames can be shown; any other kind is skipped (black).
class FrameImage : public bitsos_fb::VideoImage {
 public:
  explicit FrameImage(const scoped_refptr<VideoFrame>& frame)
      : frame_(frame) {}

  int64_t timestamp() const override { return frame_->timestamp(); }
  void FrameSize(int* width, int* height) const override {
    CpuVideoFrame* cpu = Cpu();
    *width = cpu ? cpu->size().width : 0;
    *height = cpu ? cpu->size().height : 0;
  }

  bool GetYuv(bitsos_fb::YuvPlanes* planes) override {
    CpuVideoFrame* cpu = Cpu();
    if (!cpu || cpu->format() != CpuVideoFrame::kYV12 ||
        cpu->GetPlaneCount() < 3) {
      return false;
    }
    const CpuVideoFrame::Plane& y = cpu->GetPlane(0);
    const CpuVideoFrame::Plane& u = cpu->GetPlane(1);
    const CpuVideoFrame::Plane& v = cpu->GetPlane(2);
    planes->y = y.data;
    planes->u = u.data;
    planes->v = v.data;
    planes->y_pitch = y.pitch_in_bytes;
    planes->u_pitch = u.pitch_in_bytes;
    planes->v_pitch = v.pitch_in_bytes;
    planes->width = cpu->size().width;
    planes->height = cpu->size().height;
    planes->y_rows = y.size.height;
    planes->u_rows = u.size.height;
    planes->v_rows = v.size.height;
    return planes->Valid();
  }

  bool Get(const uint8_t** data, int* width, int* height,
           int* pitch) override {
    if (!converted_) {
      converted_ = true;
      CpuVideoFrame* cpu = Cpu();
      if (!cpu) {
        return false;
      }
      if (cpu->format() == CpuVideoFrame::kBGRA32) {
        bgra_ = cpu;
      } else if (cpu->format() == CpuVideoFrame::kYV12) {
        bgra_ = cpu->ConvertTo(CpuVideoFrame::kBGRA32);
      }
    }
    if (!bgra_ || bgra_->GetPlaneCount() < 1) {
      return false;
    }
    const CpuVideoFrame::Plane& plane = bgra_->GetPlane(0);
    if (plane.size.width <= 0 || plane.size.height <= 0 ||
        plane.pitch_in_bytes < plane.size.width * 4) {
      return false;
    }
    *data = plane.data;
    *width = plane.size.width;
    *height = plane.size.height;
    *pitch = plane.pitch_in_bytes;
    return *data != nullptr;
  }

 private:
  CpuVideoFrame* Cpu() const {
    return frame_ && frame_->is_cpu_frame()
               ? static_cast<CpuVideoFrame*>(frame_.get())
               : nullptr;
  }

  scoped_refptr<VideoFrame> frame_;
  bool converted_ = false;
  scoped_refptr<CpuVideoFrame> bgra_;
};

bitsos_fb::Rect ToRect(const Rect& r) {
  return bitsos_fb::MakeRect(r.x, r.y, r.size.width, r.size.height);
}

SbKeyLocation Location(const bitsos_fb::InEvent& e) {
  if (e.right_side) {
    return kSbKeyLocationRight;
  }
  switch (e.key) {
    case kSbKeyShift:
    case kSbKeyControl:
    case kSbKeyMenu:
    case kSbKeyLwin:
      return kSbKeyLocationLeft;
  }
  return kSbKeyLocationUnspecified;
}

}  // namespace

struct ApplicationBitsosFb::Window {
  uint32_t handle = 0;
  int width = 0;
  int height = 0;
};

ApplicationBitsosFb::ApplicationBitsosFb(
    SbEventHandleCallback sb_event_handle_callback)
    : QueueApplication(sb_event_handle_callback),
      config_(FbConfig()),
      decoder_(config_.keymap) {
  wake_fd_ = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
  SB_CHECK(wake_fd_ >= 0) << "eventfd: " << strerror(errno);
  for (const std::string& s : config_.ignored) {
    LogLine("setting not understood, default kept: " + s);
  }
  SbAudioSinkImpl::Initialize();
  NetworkNotifier::GetOrCreateInstance();
}

ApplicationBitsosFb::~ApplicationBitsosFb() {
  SbAudioSinkImpl::TearDown();
  SetKdGraphics(false);
  if (wake_fd_ >= 0) {
    close(wake_fd_);
  }
}

void ApplicationBitsosFb::Initialize() {
  time_zone_monitor_ = starboard::shared::linux::TimeZoneMonitor::Create();
  if (config_.kd_graphics) {
    SetKdGraphics(true);
  }
  inputs_.reset(new bitsos_fb::EvdevDevices(config_, &LogLine));
  inputs_->Rescan();
  next_rescan_ = CurrentMonotonicTime() + kRescanUsec;
}

void ApplicationBitsosFb::Teardown() {
  time_zone_monitor_ = nullptr;
  std::shared_ptr<bitsos_fb::FbCompositor> comp;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    comp.swap(compositor_);
  }
  if (comp) {
    comp->Stop();
  }
  inputs_.reset();
  fb_.Close();
  SetKdGraphics(false);
}

void ApplicationBitsosFb::SetKdGraphics(bool on) {
  if (on) {
    tty_fd_ = open("/dev/tty", O_RDWR | O_CLOEXEC);
    if (tty_fd_ < 0) {
      LogLine(std::string("console stays in text mode: ") + strerror(errno));
      return;
    }
    // Armed first: a crash right after the switch still gives the console
    // back.
    bitsos_fb::ArmConsoleGuard(tty_fd_, &bitsos_fb::RestoreTextMode);
    if (ioctl(tty_fd_, KDSETMODE, KD_GRAPHICS) != 0) {
      LogLine(std::string("console stays in text mode: ") + strerror(errno));
      bitsos_fb::DisarmConsoleGuard();
      close(tty_fd_);
      tty_fd_ = -1;
    }
  } else if (tty_fd_ >= 0) {
    bitsos_fb::DisarmConsoleGuard();
    bitsos_fb::RestoreTextMode(tty_fd_);
    close(tty_fd_);
    tty_fd_ = -1;
  }
}

std::shared_ptr<bitsos_fb::FbCompositor> ApplicationBitsosFb::Compositor() {
  std::lock_guard<std::mutex> lock(mutex_);
  return compositor_;
}

SbWindow ApplicationBitsosFb::CreateWindow(const SbWindowOptions* options) {
  std::lock_guard<std::mutex> lock(mutex_);
  if (!fb_.is_open()) {
    if (fb_failed_) {
      return kSbWindowInvalid;
    }
    std::string error;
    if (!fb_.Open(config_, &error)) {
      fb_failed_ = true;
      SB_LOG(ERROR) << "bitsos-fb: no screen: " << error;
      return kSbWindowInvalid;
    }
    LogLine("screen " + fb_.Describe() + " path=" + config_.fb_path);
  }
  Window* w = new Window;
  w->handle = next_handle_++;
  w->width = config_.window_w;
  w->height = config_.window_h;
  if (options && options->size.width > 0 && options->size.height > 0 &&
      options->size.width <= bitsos_fb::kMaxWindowW &&
      options->size.height <= bitsos_fb::kMaxWindowH) {
    w->width = options->size.width;
    w->height = options->size.height;
  }
  windows_.push_back(w);
  if (windows_.size() == 1) {
    compositor_ = std::make_shared<bitsos_fb::FbCompositor>(
        &fb_, w->width, w->height, config_.cursor, &LogLine);
    compositor_->Start();
    const bitsos_fb::Placement& p = compositor_->scaler().placement();
    SetGeometryLocked(w->width, w->height, p);
    SB_LOG(INFO) << "bitsos-fb: window " << w->width << "x" << w->height
                 << " shown at " << p.x << "," << p.y << " size " << p.w
                 << "x" << p.h;
  }
  return reinterpret_cast<SbWindow>(w);
}

void ApplicationBitsosFb::SetGeometryLocked(int w, int h,
                                            const bitsos_fb::Placement& p) {
  geometry_changed_ = true;
  geometry_w_ = w;
  geometry_h_ = h;
  geometry_placement_ = p;
  geometry_screen_w_ = fb_.width();
  geometry_screen_h_ = fb_.height();
  WakeSystemEventWait();
}

bool ApplicationBitsosFb::DestroyWindow(SbWindow window) {
  std::shared_ptr<bitsos_fb::FbCompositor> old;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    Window* w = reinterpret_cast<Window*>(window);
    auto it = std::find(windows_.begin(), windows_.end(), w);
    if (!w || it == windows_.end()) {
      return false;
    }
    const bool was_front = it == windows_.begin();
    windows_.erase(it);
    delete w;
    if (was_front) {
      old.swap(compositor_);
      if (!windows_.empty()) {
        Window* next = windows_.front();
        compositor_ = std::make_shared<bitsos_fb::FbCompositor>(
            &fb_, next->width, next->height, config_.cursor, &LogLine);
        compositor_->Start();
        SetGeometryLocked(next->width, next->height,
                          compositor_->scaler().placement());
      }
    }
  }
  if (old) {
    old->Stop();
  }
  return true;
}

bool ApplicationBitsosFb::GetWindowSize(SbWindow window, SbWindowSize* size) {
  std::lock_guard<std::mutex> lock(mutex_);
  Window* w = reinterpret_cast<Window*>(window);
  if (!size ||
      std::find(windows_.begin(), windows_.end(), w) == windows_.end()) {
    return false;
  }
  size->width = w->width;
  size->height = w->height;
  size->video_pixel_ratio = 1.0f;
  return true;
}

void* ApplicationBitsosFb::GetPlatformHandle(SbWindow window) {
  std::lock_guard<std::mutex> lock(mutex_);
  Window* w = reinterpret_cast<Window*>(window);
  if (std::find(windows_.begin(), windows_.end(), w) == windows_.end()) {
    return nullptr;
  }
  // gfx::AcceleratedWidget is a uint32_t: a small number, not a pointer.
  return reinterpret_cast<void*>(static_cast<uintptr_t>(w->handle));
}

void ApplicationBitsosFb::Present(void* window_handle, const void* pixels,
                                  int32_t stride, int32_t width,
                                  int32_t height, int32_t damage_x,
                                  int32_t damage_y, int32_t damage_width,
                                  int32_t damage_height) {
  std::shared_ptr<bitsos_fb::FbCompositor> comp;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (windows_.empty() ||
        reinterpret_cast<uintptr_t>(window_handle) != windows_.front()->handle) {
      return;
    }
    comp = compositor_;
  }
  if (comp) {
    comp->Present(pixels, stride, width, height,
                  bitsos_fb::MakeRect(damage_x, damage_y, damage_width,
                                      damage_height));
  }
}

void ApplicationBitsosFb::PlayerSetBounds(SbPlayer player, int z_index,
                                          const Rect& rect) {
  if (auto comp = Compositor()) {
    comp->SetVideoBounds(reinterpret_cast<uintptr_t>(player), z_index,
                         ToRect(rect));
  }
}

void ApplicationBitsosFb::AcceptFrame(SbPlayer player,
                                      const scoped_refptr<VideoFrame>& frame,
                                      int z_index,
                                      const Rect& rect) {
  auto comp = Compositor();
  if (!comp) {
    return;
  }
  const uintptr_t key = reinterpret_cast<uintptr_t>(player);
  if (!frame || frame->is_end_of_stream()) {
    {
      std::lock_guard<std::mutex> lock(mutex_);
      last_frames_.erase(key);
    }
    comp->RemoveVideo(key);
    return;
  }
  {
    // The punch-out sink hands the same frame over again on every tick
    // (about 100 times a second); only a new frame is work for the screen.
    std::lock_guard<std::mutex> lock(mutex_);
    scoped_refptr<VideoFrame>& last = last_frames_[key];
    if (last.get() == frame.get()) {
      return;
    }
    last = frame;
  }
  comp->SetVideoFrame(key, std::make_shared<FrameImage>(frame));
}

bool ApplicationBitsosFb::MayHaveSystemEvents() {
  // Input devices are watched from the start, window or not; this also makes
  // every wake-up go through the one eventfd.
  return true;
}

void ApplicationBitsosFb::WakeSystemEventWait() {
  const uint64_t one = 1;
  // EAGAIN only when the counter is full, i.e. already signalled.
  (void)!write(wake_fd_, &one, sizeof(one));
}

Application::Event* ApplicationBitsosFb::WaitForSystemEventWithTimeout(
    int64_t time) {
  if (Event* e = TakePending()) {
    return e;
  }
  const int64_t now = CurrentMonotonicTime();
  if (inputs_ && now >= next_rescan_) {
    inputs_->Rescan();
    next_rescan_ = now + kRescanUsec;
  }
  std::vector<pollfd> fds;
  pollfd wake;
  wake.fd = wake_fd_;
  wake.events = POLLIN;
  wake.revents = 0;
  fds.push_back(wake);
  if (inputs_) {
    inputs_->AddPollFds(&fds);
  }
  int64_t wait = std::max<int64_t>(0, time);
  if (inputs_) {
    wait = std::min<int64_t>(wait, std::max<int64_t>(0, next_rescan_ - now));
  }
  timespec ts;
  ts.tv_sec = static_cast<time_t>(wait / 1'000'000);
  ts.tv_nsec = static_cast<long>((wait % 1'000'000) * 1000);
  const int n = ppoll(fds.data(), fds.size(), &ts, nullptr);
  if (n <= 0) {
    return nullptr;
  }
  if (fds[0].revents) {
    uint64_t count;
    (void)!read(wake_fd_, &count, sizeof(count));
  }
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (geometry_changed_) {
      geometry_changed_ = false;
      decoder_.SetGeometry(geometry_w_, geometry_h_, geometry_placement_,
                           geometry_screen_w_, geometry_screen_h_);
    }
  }
  if (inputs_) {
    std::vector<bitsos_fb::InEvent> events;
    inputs_->ReadReady(fds, &decoder_, &events);
    pending_.insert(pending_.end(), events.begin(), events.end());
    if (!events.empty()) {
      if (auto comp = Compositor()) {
        comp->SetPointer(decoder_.pointer_screen_x(),
                         decoder_.pointer_screen_y(),
                         decoder_.pointer_seen());
      }
    }
  }
  return TakePending();
}

Application::Event* ApplicationBitsosFb::TakePending() {
  SbWindow window = kSbWindowInvalid;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (!windows_.empty()) {
      window = reinterpret_cast<SbWindow>(windows_.front());
    }
  }
  while (!pending_.empty()) {
    const bitsos_fb::InEvent e = pending_.front();
    pending_.pop_front();
    if (e.type == bitsos_fb::InEvent::kPress && e.key == kSbKeyBackspace &&
        (e.modifiers & bitsos_fb::kModCtrl) &&
        (e.modifiers & bitsos_fb::kModAlt)) {
      // Way out: the keyboard is taken exclusively, so the console's own
      // keys (Ctrl+Alt+F1..) do not work while Cobalt runs.
      LogLine("Ctrl+Alt+Backspace: stopping");
      Stop(0);
      continue;
    }
    if (!SbWindowIsValid(window)) {
      continue;  // nobody to send it to yet
    }
    SbInputData* data = new SbInputData();
    memset(data, 0, sizeof(*data));
    data->window = window;
    switch (e.type) {
      case bitsos_fb::InEvent::kPress:
        data->type = kSbInputEventTypePress;
        break;
      case bitsos_fb::InEvent::kUnpress:
        data->type = kSbInputEventTypeUnpress;
        break;
      case bitsos_fb::InEvent::kMove:
        data->type = kSbInputEventTypeMove;
        break;
      case bitsos_fb::InEvent::kWheel:
        data->type = kSbInputEventTypeWheel;
        break;
    }
    switch (e.source) {
      case bitsos_fb::InEvent::kKeyboard:
        data->device_type = kSbInputDeviceTypeKeyboard;
        data->device_id = kKeyboardId;
        break;
      case bitsos_fb::InEvent::kMouse:
        data->device_type = kSbInputDeviceTypeMouse;
        data->device_id = kMouseId;
        data->pressure = NAN;
        data->size = {NAN, NAN};
        data->tilt = {NAN, NAN};
        break;
      case bitsos_fb::InEvent::kRemote:
        data->device_type = kSbInputDeviceTypeRemote;
        data->device_id = kRemoteId;
        break;
    }
    data->key = static_cast<SbKey>(e.key);
    data->character = static_cast<wchar_t>(e.character);
    data->key_location = Location(e);
    data->key_modifiers = static_cast<unsigned int>(e.modifiers);
    data->position.x = static_cast<float>(e.x);
    data->position.y = static_cast<float>(e.y);
    data->delta.x = static_cast<float>(e.dx);
    data->delta.y = static_cast<float>(e.dy);
    return new Event(kSbEventTypeInput, data, &DeleteDestructor<SbInputData>);
  }
  return nullptr;
}

}  // namespace bitsos
}  // namespace starboard
