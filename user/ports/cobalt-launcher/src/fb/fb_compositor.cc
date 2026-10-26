// BitsOS V2 Cobalt backend: puts UI, video and pointer on the framebuffer.

#include "starboard/shared/bitsos/fb/fb_compositor.h"

#include "starboard/shared/bitsos/fb/av_stats.h"

#include <pthread.h>
#include <string.h>

#include <algorithm>
#include <sstream>

namespace bitsos_fb {
namespace {

constexpr std::chrono::milliseconds kRecheckInterval(500);

}  // namespace

FbCompositor::FbCompositor(FbDevice* fb, int win_w, int win_h,
                           CursorMode cursor, LogFn log)
    : fb_(fb),
      win_w_(std::max(1, win_w)),
      win_h_(std::max(1, win_h)),
      cursor_mode_(cursor),
      cursor_scale_(CursorScaleFor(fb->height())),
      last_log_(std::chrono::steady_clock::now()),
      log_(log),
      last_recheck_(std::chrono::steady_clock::now()) {
  scaler_.Init(win_w_, win_h_,
               FitWindow(win_w_, win_h_, fb_->width(), fb_->height()));
  ui_.assign(static_cast<size_t>(win_w_) * win_h_, 0);
  composed_.assign(ui_.size(), 0);
}

FbCompositor::~FbCompositor() {
  Stop();
}

void FbCompositor::Start() {
  std::lock_guard<std::mutex> lock(mutex_);
  if (started_) {
    return;
  }
  started_ = true;
  stop_ = false;
  // Black everywhere first (the bars around a window of another shape).
  ClearFb(fb_->pixels(), fb_->pitch(), fb_->format(),
          MakeRect(0, 0, fb_->width(), fb_->height()));
  damage_ = MakeRect(0, 0, win_w_, win_h_);
  thread_ = std::thread(&FbCompositor::Loop, this);
}

void FbCompositor::Stop() {
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (!started_) {
      return;
    }
    stop_ = true;
  }
  wake_.notify_all();
  if (thread_.joinable()) {
    thread_.join();
  }
  std::lock_guard<std::mutex> lock(mutex_);
  started_ = false;
  idle_.notify_all();
}

void FbCompositor::DamageLocked(const Rect& src) {
  const Rect r = Intersect(src, MakeRect(0, 0, win_w_, win_h_));
  if (r.Empty()) {
    return;
  }
  damage_ = Union(damage_, r);
  wake_.notify_one();
}

bool FbCompositor::CursorVisibleLocked() const {
  switch (cursor_mode_) {
    case CursorMode::kOn:
      return true;
    case CursorMode::kOff:
      return false;
    case CursorMode::kAuto:
      return cursor_seen_;
  }
  return false;
}

void FbCompositor::Present(const void* pixels, int stride, int width,
                           int height, const Rect& damage) {
  if (!pixels || width <= 0 || height <= 0 ||
      static_cast<long long>(stride) < static_cast<long long>(width) * 4) {
    return;
  }
  const Rect r = Intersect(
      damage, MakeRect(0, 0, std::min(width, win_w_), std::min(height, win_h_)));
  std::lock_guard<std::mutex> lock(mutex_);
  ++stats_.presents;
  if (r.Empty()) {
    return;
  }
  const uint8_t* src = static_cast<const uint8_t*>(pixels);
  for (int y = r.y; y < r.y + r.h; ++y) {
    memcpy(&ui_[static_cast<size_t>(y) * win_w_ + r.x],
           src + static_cast<size_t>(y) * stride + static_cast<size_t>(r.x) * 4,
           static_cast<size_t>(r.w) * 4);
  }
  DamageLocked(r);
}

void FbCompositor::SetVideoBounds(uintptr_t player, int z_index,
                                  const Rect& bounds) {
  std::lock_guard<std::mutex> lock(mutex_);
  Video& v = videos_[player];
  DamageLocked(v.bounds);
  v.z = z_index;
  v.bounds = bounds;
  DamageLocked(v.bounds);
}

void FbCompositor::SetVideoFrame(uintptr_t player,
                                 std::shared_ptr<VideoImage> image) {
  std::lock_guard<std::mutex> lock(mutex_);
  Video& v = videos_[player];
  v.image = std::move(image);
  v.fresh = true;
  ++stats_.video_frames;
  DamageLocked(v.bounds);
}

void FbCompositor::RemoveVideo(uintptr_t player) {
  std::lock_guard<std::mutex> lock(mutex_);
  auto it = videos_.find(player);
  if (it == videos_.end()) {
    return;
  }
  DamageLocked(it->second.bounds);
  videos_.erase(it);
}

void FbCompositor::SetPointer(int screen_x, int screen_y, bool seen) {
  std::lock_guard<std::mutex> lock(mutex_);
  if (screen_x == cursor_x_ && screen_y == cursor_y_ && seen == cursor_seen_) {
    return;
  }
  cursor_x_ = screen_x;
  cursor_y_ = screen_y;
  cursor_seen_ = seen;
  cursor_dirty_ = true;
  wake_.notify_one();
}

void FbCompositor::WaitIdle() {
  std::unique_lock<std::mutex> lock(mutex_);
  idle_.wait(lock, [this] {
    return !started_ || stop_ ||
           (!busy_ && damage_.Empty() && !cursor_dirty_);
  });
}

FbCompositor::Stats FbCompositor::stats() {
  std::lock_guard<std::mutex> lock(mutex_);
  return stats_;
}

void FbCompositor::Loop() {
  pthread_setname_np(pthread_self(), "bitsos_fb");
  std::unique_lock<std::mutex> lock(mutex_);
  while (true) {
    // The timeout only serves the screen recheck.
    wake_.wait_for(lock, kRecheckInterval, [this] {
      return stop_ || !damage_.Empty() || cursor_dirty_;
    });
    if (stop_) {
      break;
    }
    RecheckScreenLocked();
    if (damage_.Empty() && !cursor_dirty_) {
      continue;
    }
    const Rect dmg = damage_;
    damage_ = Rect();
    const bool cursor_moved = cursor_dirty_;
    cursor_dirty_ = false;
    busy_ = true;
    std::vector<Video> videos;
    for (auto& kv : videos_) {
      if (kv.second.image && !Intersect(kv.second.bounds, dmg).Empty()) {
        videos.push_back(kv.second);
        kv.second.fresh = false;
      }
    }
    std::stable_sort(videos.begin(), videos.end(),
                     [](const Video& a, const Video& b) { return a.z < b.z; });
    const bool want_cursor = CursorVisibleLocked();
    const int cx = cursor_x_;
    const int cy = cursor_y_;

    // Video frames are converted and drawn without the lock: Present and the
    // player are never held up by a conversion.
    lock.unlock();
    const auto t0 = std::chrono::steady_clock::now();
    if (!dmg.Empty()) {
      // Black first, unless a video layer covers all of it anyway.
      bool covered = false;
      for (const Video& v : videos) {
        covered = covered || Intersect(v.bounds, dmg) == dmg;
      }
      if (!covered) {
        Fill(composed_.data(), win_w_, dmg, 0);
      }
      bool drawn_all = true;
      for (const Video& v : videos) {
        drawn_all = DrawVideo(v, dmg) && drawn_all;
      }
      if (covered && !drawn_all) {
        Fill(composed_.data(), win_w_, dmg, 0);
        for (const Video& v : videos) {
          DrawVideo(v, dmg);
        }
      }
    }
    lock.lock();
    if (!dmg.Empty()) {
      BlendUiOver(ui_.data(), win_w_, composed_.data(), win_w_, dmg);
    }
    ++stats_.passes;
    lock.unlock();
    const auto t1 = std::chrono::steady_clock::now();

    const Rect old_cursor =
        cursor_on_ ? CursorRect(cursor_on_x_, cursor_on_y_, cursor_scale_) : Rect();
    cursor_on_ = want_cursor;
    cursor_on_x_ = cx;
    cursor_on_y_ = cy;
    if (!dmg.Empty()) {
      RenderDst(scaler_.SrcToDst(dmg));
    }
    if (cursor_moved) {
      // Restore what was under the old pointer, then draw the new one.
      if (!old_cursor.Empty()) {
        RenderDst(old_cursor);
      }
      if (cursor_on_) {
        RenderDst(CursorRect(cx, cy, cursor_scale_));
      }
    }

    const auto t2 = std::chrono::steady_clock::now();
    // steady_clock is CLOCK_MONOTONIC, the clock Starboard's media time
    // notes use.
    const int64_t shown_us = static_cast<int64_t>(
        std::chrono::duration_cast<std::chrono::microseconds>(
            t2.time_since_epoch())
            .count());
    for (const Video& v : videos) {
      const int64_t pts = v.fresh ? v.image->timestamp() : -1;
      if (pts >= 0) {
        int w, h;
        v.image->FrameSize(&w, &h);
        AvStats::Get()->NoteShown(shown_us, pts, w, h);
      }
    }
    lock.lock();
    const auto us = [](std::chrono::steady_clock::duration d) {
      return static_cast<uint64_t>(
          std::chrono::duration_cast<std::chrono::microseconds>(d).count());
    };
    stats_.compose_us += us(t1 - t0);
    stats_.scale_us += us(t2 - t1);
    stats_.max_pass_us = std::max(stats_.max_pass_us, us(t2 - t0));
    MaybeLogLocked();
    busy_ = false;
    if (damage_.Empty() && !cursor_dirty_) {
      idle_.notify_all();
    }
  }
  busy_ = false;
  idle_.notify_all();
}

void FbCompositor::RecheckScreenLocked() {
  const auto now = std::chrono::steady_clock::now();
  if (screen_lost_ || now - last_recheck_ < kRecheckInterval) {
    return;
  }
  last_recheck_ = now;
  switch (fb_->Recheck()) {
    case FbDevice::Change::kSame:
      return;
    case FbDevice::Change::kMoved:
      // Another page is visible now: everything again, bars included.
      ++stats_.pans;
      ClearFb(fb_->pixels(), fb_->pitch(), fb_->format(),
              MakeRect(0, 0, fb_->width(), fb_->height()));
      damage_ = MakeRect(0, 0, win_w_, win_h_);
      return;
    case FbDevice::Change::kLost:
      screen_lost_ = true;
      stats_.lost = true;
      if (log_) {
        log_("screen mode changed while running; nothing is drawn any more");
      }
      return;
  }
}

bool FbCompositor::DrawVideo(const Video& v, const Rect& dmg) {
  const Rect clip =
      Intersect(Intersect(v.bounds, dmg), MakeRect(0, 0, win_w_, win_h_));
  if (clip.Empty()) {
    return true;
  }
  YuvPlanes yuv;
  if (v.image->GetYuv(&yuv) && yuv.Valid()) {
    if (yuv.width == v.bounds.w && yuv.height == v.bounds.h) {
      // The usual case: the frame fills its rectangle 1:1.
      ConvertYuvRect(yuv,
                     MakeRect(clip.x - v.bounds.x, clip.y - v.bounds.y, clip.w,
                              clip.h),
                     composed_.data(), win_w_, clip.x, clip.y);
      return true;
    }
    if (scratch_image_ != v.image) {
      scratch_.resize(static_cast<size_t>(yuv.width) * yuv.height);
      ConvertYuvRect(yuv, MakeRect(0, 0, yuv.width, yuv.height),
                     scratch_.data(), yuv.width, 0, 0);
      scratch_image_ = v.image;
    }
    DrawStretched(reinterpret_cast<const uint8_t*>(scratch_.data()),
                  yuv.width, yuv.height, yuv.width * 4, v.bounds, clip,
                  composed_.data(), win_w_);
    return true;
  }
  const uint8_t* data = nullptr;
  int w = 0, h = 0, pitch = 0;
  if (!v.image->Get(&data, &w, &h, &pitch)) {
    return false;
  }
  DrawStretched(data, w, h, pitch, v.bounds, clip, composed_.data(), win_w_);
  return true;
}

void FbCompositor::MaybeLogLocked() {
  const auto now = std::chrono::steady_clock::now();
  if (!log_ || now - last_log_ < std::chrono::seconds(10)) {
    return;
  }
  const double secs =
      std::chrono::duration<double>(now - last_log_).count();
  const Stats& a = logged_;
  const Stats& b = stats_;
  const uint64_t passes = b.passes - a.passes;
  std::ostringstream s;
  s.setf(std::ios::fixed);
  s.precision(1);
  s << "screen stats: presents/s=" << (b.presents - a.presents) / secs
    << " video_frames/s=" << (b.video_frames - a.video_frames) / secs
    << " passes/s=" << passes / secs
    << " screen_mpix/s=" << (b.screen_pixels - a.screen_pixels) / secs / 1e6;
  s.precision(2);
  s << " compose_ms=" << (passes ? (b.compose_us - a.compose_us) / 1e3 / passes : 0)
    << " scale_ms=" << (passes ? (b.scale_us - a.scale_us) / 1e3 / passes : 0)
    << " max_pass_ms=" << b.max_pass_us / 1e3;
  if (b.pans) {
    s << " pans=" << b.pans;
  }
  stats_.max_pass_us = 0;
  logged_ = stats_;
  last_log_ = now;
  log_(s.str());
}

void FbCompositor::RenderDst(const Rect& dst) {
  if (screen_lost_) {
    return;
  }
  const Rect screen = MakeRect(0, 0, fb_->width(), fb_->height());
  const Rect d = Intersect(dst, screen);
  if (d.Empty()) {
    return;
  }
  const Placement& p = scaler_.placement();
  const Rect area = MakeRect(p.x, p.y, p.w, p.h);
  if (!(Intersect(d, area) == d)) {
    ClearFb(fb_->pixels(), fb_->pitch(), fb_->format(), d);
  }
  scaler_.Render(composed_.data(), win_w_, d, fb_->pixels(), fb_->pitch(),
                 fb_->format());
  if (cursor_on_) {
    DrawCursor(fb_->pixels(), fb_->pitch(), fb_->format(), cursor_on_x_,
               cursor_on_y_, cursor_scale_, d);
  }
  std::lock_guard<std::mutex> lock(mutex_);
  stats_.screen_pixels += static_cast<uint64_t>(d.w) * d.h;
}

}  // namespace bitsos_fb
