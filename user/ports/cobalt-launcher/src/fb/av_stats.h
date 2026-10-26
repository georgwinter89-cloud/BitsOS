// BitsOS V2 Cobalt backend: picture against the audio clock, frame and drop
// counts (YT-5).
//
// Starboard's render algorithm reports on every Render() that draws: the
// audio clock (media time), the frame it chose and its drop counter
// (BitsosAvNoteRender, a weak hook in video_render_algorithm_impl.cc). The
// compositor reports when a frame reached the screen (NoteShown). The offset
// of a shown frame = its timestamp minus the audio clock at that moment (the
// last media time read, run on at the playback rate): > 0 the picture is
// early, < 0 it is late. The audio clock is Cobalt's: frames written to ALSA
// minus snd_pcm_delay, so this is the whole chain as far as the delay is
// honest (SN-7).
//
// Out: a small key=value file (BITSOS_AV_STATS), replaced by rename at most
// once a second while notes come in, so that MCP can read it at any time; a
// log line every 10 s while frames are shown; with BITSOS_AV_TRACE one line
// per shown frame ("shown <monotonic us> <pts us> <offset us|->", and
// "lost <n>" if the writer fell more than kTracePendingMax behind).
//
// The notes only count under a lock; all file and log output is done by
// Flush(), on a writer thread of its own (or by the caller in tests), so a
// slow stick or profile file system cannot hold up frame choice or
// presentation, which would show up as the very drops these numbers count.

#ifndef STARBOARD_SHARED_BITSOS_FB_AV_STATS_H_
#define STARBOARD_SHARED_BITSOS_FB_AV_STATS_H_

#include <stdint.h>
#include <stdio.h>

#include <condition_variable>
#include <map>
#include <mutex>
#include <string>
#include <thread>

namespace bitsos_fb {

class AvStats {
 public:
  // Offsets over this are seeks or a second player, not sync.
  static constexpr int64_t kOffsetIgnoreUs = 2'000'000;
  // How often the writer thread flushes.
  static constexpr int64_t kWriterWakeUs = 250'000;
  // Trace text waiting for the writer; more is counted as lost, not kept, so
  // a stalled file system cannot make Cobalt's memory grow.
  static constexpr size_t kTracePendingMax = 1 << 20;

  struct Window {
    uint64_t n = 0;        // offsets counted
    int64_t sum_us = 0;
    int64_t min_us = 0;
    int64_t max_us = 0;
    uint64_t within_20ms = 0;
    uint64_t within_40ms = 0;
    uint64_t over_80ms = 0;
    void Add(int64_t off_us);
  };

  typedef void (*LogFn)(const std::string& line);

  // The process-wide instance, paths from the environment (may be unset).
  static AvStats* Get();

  // |writer_thread|: start the thread that calls Flush(); without it the
  // caller does (tests, with their own clock).
  AvStats(const char* stats_path, const char* trace_path, LogFn log,
          bool writer_thread = false);
  AvStats(const AvStats&) = delete;
  AvStats& operator=(const AvStats&) = delete;
  ~AvStats();

  // From the render algorithm (player thread). |who| tells the players
  // apart; |dropped| is that player's own running count.
  void NoteRender(int64_t now_us, const void* who, int64_t media_us,
                  bool playing, double rate, int64_t pts_us, int64_t dropped);
  // From the compositor thread, after a new frame went to the screen;
  // |w| x |h| is its decoded size (0 x 0 if unknown).
  void NoteShown(int64_t now_us, int64_t pts_us, int w = 0, int h = 0);

  // Writes what is due as of |now_us|: pending trace lines, the 10 s log
  // line, the file (at most once a second, only if something changed).
  void Flush(int64_t now_us);

  // The file's text as of |now_us| (also for tests).
  std::string Text(int64_t now_us);

 private:
  struct Player {
    int64_t dropped = 0;
    int64_t seen_us = 0;
  };

  void WriterLoop();
  void FlushTrace();
  std::string TextLocked(int64_t now_us);

  const std::string stats_path_;
  const LogFn log_;
  FILE* trace_ = nullptr;

  std::mutex io_mutex_;  // one Flush() at a time

  std::mutex out_mutex_;  // the pending trace and the writer's stop flag
  std::condition_variable wake_;
  std::string trace_pending_;
  uint64_t trace_lost_ = 0;
  bool stop_ = false;
  std::thread writer_;

  std::mutex mutex_;  // the numbers
  std::map<const void*, Player> players_;
  uint64_t renders_ = 0;
  uint64_t shown_ = 0;
  uint64_t dropped_ = 0;
  uint64_t skipped_ = 0;  // shown while paused, or offset ignored
  int64_t media_us_ = -1;
  int64_t media_at_us_ = 0;
  bool playing_ = false;
  double rate_ = 0;
  int64_t last_pts_us_ = -1;
  int64_t last_offset_us_ = 0;
  int last_w_ = 0;
  int last_h_ = 0;
  int max_w_ = 0;  // the largest frame shown (by area)
  int max_h_ = 0;
  Window total_;
  Window recent_;  // since the last log line
  uint64_t shown_at_log_ = 0;
  uint64_t dropped_at_log_ = 0;
  int64_t logged_us_ = 0;
  int64_t flushed_us_ = 0;
  bool dirty_ = false;
};

}  // namespace bitsos_fb

#endif  // STARBOARD_SHARED_BITSOS_FB_AV_STATS_H_
