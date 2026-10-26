// BitsOS V2 Cobalt backend: A/V numbers (see av_stats.h).

#include "starboard/shared/bitsos/fb/av_stats.h"

#include <stdlib.h>
#include <time.h>

#include <algorithm>
#include <chrono>
#include <cinttypes>

namespace bitsos_fb {

namespace {

constexpr int64_t kFlushUs = 1'000'000;
constexpr int64_t kLogUs = 10'000'000;
constexpr int64_t kPlayerGoneUs = 10'000'000;

int64_t MonotonicUs() {
  timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return static_cast<int64_t>(ts.tv_sec) * 1'000'000 + ts.tv_nsec / 1000;
}

void StderrLog(const std::string& line) {
  fprintf(stderr, "bitsos-fb: %s\n", line.c_str());
}

std::string Ms(int64_t us) {
  char b[32];
  snprintf(b, sizeof(b), "%.1f", static_cast<double>(us) / 1000.0);
  return b;
}

std::string WindowText(const char* name, const AvStats::Window& w) {
  if (w.n == 0) {
    return std::string(name) + " n=0\n";
  }
  char b[256];
  snprintf(b, sizeof(b),
           "%s n=%" PRIu64 " avg_ms=%s min_ms=%s max_ms=%s within_20ms=%" PRIu64
           " within_40ms=%" PRIu64 " over_80ms=%" PRIu64 "\n",
           name, w.n, Ms(w.sum_us / static_cast<int64_t>(w.n)).c_str(),
           Ms(w.min_us).c_str(), Ms(w.max_us).c_str(), w.within_20ms,
           w.within_40ms, w.over_80ms);
  return b;
}

}  // namespace

void AvStats::Window::Add(int64_t off_us) {
  min_us = n == 0 ? off_us : std::min(min_us, off_us);
  max_us = n == 0 ? off_us : std::max(max_us, off_us);
  ++n;
  sum_us += off_us;
  const int64_t a = off_us < 0 ? -off_us : off_us;
  within_20ms += a <= 20'000;
  within_40ms += a <= 40'000;
  over_80ms += a > 80'000;
}

AvStats* AvStats::Get() {
  // Lives as long as the process: the player and compositor threads may
  // still report while static destructors run.
  static AvStats* stats =
      new AvStats(getenv("BITSOS_AV_STATS"), getenv("BITSOS_AV_TRACE"),
                  StderrLog, /*writer_thread=*/true);
  return stats;
}

AvStats::AvStats(const char* stats_path, const char* trace_path, LogFn log,
                 bool writer_thread)
    : stats_path_(stats_path ? stats_path : ""), log_(log) {
  if (trace_path && *trace_path) {
    trace_ = fopen(trace_path, "ae");
    if (!trace_ && log_) {
      log_(std::string("av: cannot open the trace ") + trace_path);
    }
  }
  if (writer_thread && (trace_ || !stats_path_.empty() || log_)) {
    writer_ = std::thread([this] { WriterLoop(); });
  }
}

AvStats::~AvStats() {
  {
    std::lock_guard<std::mutex> lock(out_mutex_);
    stop_ = true;
  }
  wake_.notify_all();
  if (writer_.joinable()) {
    writer_.join();
  }
  std::lock_guard<std::mutex> io(io_mutex_);
  FlushTrace();
  if (trace_) {
    fclose(trace_);
  }
}

void AvStats::WriterLoop() {
  std::unique_lock<std::mutex> lock(out_mutex_);
  while (!stop_) {
    wake_.wait_for(lock, std::chrono::microseconds(kWriterWakeUs));
    if (stop_) {
      break;
    }
    lock.unlock();
    Flush(MonotonicUs());
    lock.lock();
  }
}

void AvStats::NoteRender(int64_t now_us, const void* who, int64_t media_us,
                         bool playing, double rate, int64_t pts_us,
                         int64_t dropped) {
  {
    std::lock_guard<std::mutex> lock(mutex_);
    ++renders_;
    auto it = players_.find(who);
    if (it == players_.end()) {
      it = players_.emplace(who, Player()).first;
    }
    // A count below the last one is a new player at the same address.
    const int64_t base = dropped >= it->second.dropped ? it->second.dropped : 0;
    dropped_ += static_cast<uint64_t>(dropped - base);
    it->second.dropped = dropped;
    it->second.seen_us = now_us;
    for (auto p = players_.begin(); p != players_.end();) {
      p = now_us - p->second.seen_us > kPlayerGoneUs ? players_.erase(p)
                                                     : std::next(p);
    }
    media_us_ = media_us;
    media_at_us_ = now_us;
    playing_ = playing;
    rate_ = rate;
    (void)pts_us;
    dirty_ = true;
  }
}

void AvStats::NoteShown(int64_t now_us, int64_t pts_us, int w, int h) {
  bool have = false;
  int64_t off = 0;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    ++shown_;
    last_pts_us_ = pts_us;
    if (w > 0 && h > 0) {
      last_w_ = w;
      last_h_ = h;
      if (static_cast<int64_t>(w) * h >
          static_cast<int64_t>(max_w_) * max_h_) {
        max_w_ = w;
        max_h_ = h;
      }
    }
    if (playing_ && media_us_ >= 0 && rate_ > 0) {
      const int64_t clock =
          media_us_ + static_cast<int64_t>(
                          static_cast<double>(now_us - media_at_us_) * rate_);
      off = pts_us - clock;
      have = off <= kOffsetIgnoreUs && off >= -kOffsetIgnoreUs;
    }
    if (have) {
      last_offset_us_ = off;
      total_.Add(off);
      recent_.Add(off);
    } else {
      ++skipped_;
    }
    dirty_ = true;
  }
  if (!trace_) {
    return;
  }
  char b[96];
  const int n =
      have ? snprintf(b, sizeof(b), "shown %" PRId64 " %" PRId64 " %" PRId64
                      "\n", now_us, pts_us, off)
           : snprintf(b, sizeof(b), "shown %" PRId64 " %" PRId64 " -\n",
                      now_us, pts_us);
  if (n <= 0 || static_cast<size_t>(n) >= sizeof(b)) {
    return;
  }
  std::lock_guard<std::mutex> lock(out_mutex_);
  if (trace_pending_.size() + static_cast<size_t>(n) > kTracePendingMax) {
    ++trace_lost_;
  } else {
    trace_pending_.append(b, static_cast<size_t>(n));
  }
}

std::string AvStats::Text(int64_t now_us) {
  std::lock_guard<std::mutex> lock(mutex_);
  return TextLocked(now_us);
}

std::string AvStats::TextLocked(int64_t now_us) {
  char b[512];
  snprintf(b, sizeof(b),
           "bitsos-av 1\n"
           "time_us=%" PRId64 "\n"
           "playing=%d rate=%.2f\n"
           "media_time_us=%" PRId64 " read_us_ago=%" PRId64 "\n"
           "last_pts_us=%" PRId64 " last_offset_ms=%s\n"
           "frame_last=%dx%d frame_max=%dx%d\n"
           "renders=%" PRIu64 " frames_shown=%" PRIu64
           " frames_dropped=%" PRIu64 " offsets_skipped=%" PRIu64 "\n",
           now_us, playing_ ? 1 : 0, rate_, media_us_, now_us - media_at_us_,
           last_pts_us_, Ms(last_offset_us_).c_str(), last_w_, last_h_,
           max_w_, max_h_, renders_, shown_,
           dropped_, skipped_);
  return std::string(b) + WindowText("offset_total", total_) +
         WindowText("offset_recent", recent_);
}

void AvStats::FlushTrace() {
  if (!trace_) {
    return;
  }
  std::string text;
  uint64_t lost = 0;
  {
    std::lock_guard<std::mutex> lock(out_mutex_);
    text.swap(trace_pending_);
    lost = trace_lost_;
    trace_lost_ = 0;
  }
  if (lost) {
    fprintf(trace_, "lost %" PRIu64 "\n", lost);
    if (log_) {
      log_("av: the trace writer fell behind, " + std::to_string(lost) +
           " lines lost");
    }
  }
  if (!text.empty()) {
    fwrite(text.data(), 1, text.size(), trace_);
  }
  fflush(trace_);
}

void AvStats::Flush(int64_t now_us) {
  std::lock_guard<std::mutex> io(io_mutex_);
  FlushTrace();
  std::string text;
  std::string line;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (log_ && (logged_us_ == 0 || now_us - logged_us_ >= kLogUs)) {
      // The first note only starts the first 10 s.
      if (logged_us_ != 0 && shown_ != shown_at_log_) {
        const double secs = static_cast<double>(now_us - logged_us_) / 1e6;
        char b[160];
        snprintf(b, sizeof(b),
                 "av: %.1f frames/s shown, %" PRIu64 " dropped in %.0f s; ",
                 static_cast<double>(shown_ - shown_at_log_) / secs,
                 dropped_ - dropped_at_log_, secs);
        line = b + WindowText("offset", recent_);
        line.pop_back();
      }
      logged_us_ = now_us;
      shown_at_log_ = shown_;
      dropped_at_log_ = dropped_;
      recent_ = Window();
    }
    if (!stats_path_.empty() && dirty_ &&
        (flushed_us_ == 0 || now_us - flushed_us_ >= kFlushUs)) {
      text = TextLocked(now_us);
      flushed_us_ = now_us;
      dirty_ = false;
    }
  }
  if (!line.empty()) {
    log_(line);
  }
  if (text.empty()) {
    return;
  }
  // Readers see the old file or the new one, never half of one.
  const std::string tmp = stats_path_ + ".new";
  FILE* f = fopen(tmp.c_str(), "we");
  if (!f) {
    return;
  }
  const bool ok = fwrite(text.data(), 1, text.size(), f) == text.size();
  if (fclose(f) == 0 && ok) {
    rename(tmp.c_str(), stats_path_.c_str());
  }
}

}  // namespace bitsos_fb

// Weak in video_render_algorithm_impl.cc: called on every Render() that
// draws a frame.
extern "C" void BitsosAvNoteRender(const void* who, int64_t media_time_us,
                                   bool audio_playing, double playback_rate,
                                   int64_t frame_pts_us,
                                   int64_t dropped_frames) {
  bitsos_fb::AvStats::Get()->NoteRender(
      bitsos_fb::MonotonicUs(), who, media_time_us, audio_playing,
      playback_rate, frame_pts_us, dropped_frames);
}
