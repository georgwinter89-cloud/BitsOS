// Host tests for the backend's A/V numbers (YT-5, av_stats.cc).
// Built and run by host/tests/yt/run-host-tests.sh.

#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#include <fstream>
#include <sstream>
#include <string>
#include <vector>

#include "starboard/shared/bitsos/fb/av_stats.h"

using bitsos_fb::AvStats;

#define CHECK(c)                                                   \
  do {                                                             \
    if (!(c)) {                                                    \
      fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #c); \
      exit(1);                                                     \
    }                                                              \
  } while (0)

namespace {

std::vector<std::string> g_log;
void Log(const std::string& line) { g_log.push_back(line); }

std::string Slurp(const std::string& path) {
  std::ifstream f(path);
  std::stringstream s;
  s << f.rdbuf();
  return s.str();
}

bool Has(const std::string& text, const std::string& part) {
  return text.find(part) != std::string::npos;
}

// 60 frames/s, audio clock exact, picture shown 5 ms after its time: offset
// -5 ms on every frame; the file follows at most once a second.
void TestSteady(const std::string& dir) {
  const std::string stats = dir + "/av.txt";
  const std::string trace = dir + "/trace.txt";
  g_log.clear();
  {
    AvStats av(stats.c_str(), trace.c_str(), Log);
    int dummy;
    const int64_t t0 = 5'000'000;
    for (int i = 0; i < 600; ++i) {  // 10 s
      const int64_t pts = i * 16'667;
      const int64_t now = t0 + pts;
      av.NoteRender(now, &dummy, pts, true, 1.0, pts, i / 100);
      av.NoteShown(now + 5'000 + 5'000, pts + 5'000,  // shown 10 ms on, 5 ms
                   i < 300 ? 1280 : 1920, i < 300 ? 720 : 1080);
      av.Flush(now + 10'000);  // what the writer thread does
    }
    const std::string t = av.Text(t0 + 10'000'000);
    CHECK(Has(t, "frames_shown=600 frames_dropped=5 offsets_skipped=0"));
    CHECK(Has(t, "offset_total n=600 avg_ms=-5.0 min_ms=-5.0 max_ms=-5.0 "
                 "within_20ms=600 within_40ms=600 over_80ms=0"));
    CHECK(Has(t, "playing=1 rate=1.00"));
    CHECK(Has(t, "frame_last=1920x1080 frame_max=1920x1080\n"));
    // File written: last flush at a whole second, content is a prefix-valid
    // snapshot (key lines present).
    const std::string f = Slurp(stats);
    CHECK(Has(f, "bitsos-av 1\n"));
    CHECK(Has(f, "offset_total n="));
    CHECK(access((stats + ".new").c_str(), F_OK) != 0);
  }
  const std::string tr = Slurp(trace);
  CHECK(Has(tr, "shown 5010000 5000 -5000\n"));
  int lines = 0;
  for (char c : tr) lines += c == '\n';
  CHECK(lines == 600);
  // One 10 s log line (the first note only starts the window).
  CHECK(g_log.empty() || Has(g_log[0], "av: "));
  printf("ok av_stats.steady\n");
}

// Paused, rate 0, seeks (> 2 s off) are not counted as offsets; two players
// sum their own drop counters; a player reset (count goes down) is new.
void TestSkipsAndPlayers() {
  g_log.clear();
  AvStats av(nullptr, nullptr, Log);
  int a, b;
  av.NoteRender(1'000'000, &a, 0, false, 1.0, 0, 3);
  av.NoteShown(1'000'000, 0);                 // paused
  av.NoteRender(1'100'000, &a, 100'000, true, 0.0, 100'000, 4);
  av.NoteShown(1'100'000, 100'000);           // rate 0
  av.NoteRender(1'200'000, &a, 200'000, true, 1.0, 200'000, 4);
  av.NoteShown(1'200'000, 5'200'000);         // a seek target, 5 s ahead
  av.NoteShown(1'210'000, 230'000);           // +20 ms early
  av.NoteRender(1'300'000, &b, 0, true, 1.0, 0, 2);
  av.NoteRender(1'400'000, &a, 0, true, 1.0, 0, 1);  // new player at &a
  const std::string t = av.Text(1'500'000);
  CHECK(Has(t, "frames_shown=4 frames_dropped=7 offsets_skipped=3"));
  CHECK(Has(t, "frame_last=0x0 frame_max=0x0\n"));  // sizes unknown
  CHECK(Has(t, "offset_total n=1 avg_ms=20.0 min_ms=20.0 max_ms=20.0 "
               "within_20ms=1 within_40ms=1 over_80ms=0"));
  // Playback rate 2: the clock runs on twice as fast between notes.
  av.NoteRender(2'000'000, &b, 1'000'000, true, 2.0, 1'000'000, 2);
  av.NoteShown(2'010'000, 1'020'000);
  CHECK(Has(av.Text(2'100'000), "last_offset_ms=0.0"));
  printf("ok av_stats.skips_players\n");
}

// The 10 s log line: rate and drops of the window, then a new window.
void TestLog() {
  g_log.clear();
  AvStats av(nullptr, nullptr, Log);
  int p;
  for (int i = 0; i <= 301; ++i) {  // 30 frames/s for 10 s, then one more
    const int64_t now = 1'000'000 + i * 33'333;
    av.NoteRender(now, &p, i * 33'333, true, 1.0, i * 33'333, i >= 150 ? 2 : 0);
    av.NoteShown(now, i * 33'333 - 30'000);  // 30 ms late
    av.Flush(now);
  }
  CHECK(g_log.size() == 1);
  CHECK(Has(g_log[0], "av: 30.0 frames/s shown, 2 dropped in 10 s; offset n="));
  CHECK(Has(g_log[0], "avg_ms=-30.0"));
  CHECK(Has(g_log[0], "within_20ms=0 within_40ms="));
  printf("ok av_stats.log\n");
}

// The notes write nothing themselves: until Flush() the file and the trace
// stay empty, and the log gets nothing.
void TestNotesDoNoOutput(const std::string& dir) {
  const std::string stats = dir + "/quiet.txt";
  const std::string trace = dir + "/quiet-trace.txt";
  g_log.clear();
  AvStats av(stats.c_str(), trace.c_str(), Log);
  int p;
  for (int i = 0; i < 1000; ++i) {  // 20 s at 50 frames/s
    av.NoteRender(1'000'000 + i * 20'000, &p, i * 20'000, true, 1.0,
                  i * 20'000, 0);
    av.NoteShown(1'000'000 + i * 20'000, i * 20'000);
  }
  CHECK(access(stats.c_str(), F_OK) != 0);
  CHECK(Slurp(trace).empty());
  CHECK(g_log.empty());
  av.Flush(21'000'000);
  CHECK(Has(Slurp(stats), "frames_shown=1000 "));
  CHECK(Has(Slurp(trace), "shown 1000000 0 0\n"));
  printf("ok av_stats.notes_do_no_output\n");
}

// With the writer thread: the file shows up within a few wake-ups, and the
// destructor writes the trace lines still pending.
void TestWriterThread(const std::string& dir) {
  const std::string stats = dir + "/thread.txt";
  const std::string trace = dir + "/thread-trace.txt";
  {
    AvStats av(stats.c_str(), trace.c_str(), nullptr, true);
    int p;
    for (int i = 0; i < 100; ++i) {
      av.NoteRender(1'000'000 + i * 16'667, &p, i * 16'667, true, 1.0,
                    i * 16'667, 0);
      av.NoteShown(1'000'000 + i * 16'667, i * 16'667);
    }
    bool seen = false;
    for (int i = 0; i < 100 && !seen; ++i) {  // up to 5 s
      usleep(50'000);
      seen = Has(Slurp(stats), "frames_shown=100 ");
    }
    CHECK(seen);
    av.NoteShown(5'000'000, 4'000'000);  // pending when destroyed
  }
  const std::string tr = Slurp(trace);
  int lines = 0;
  for (char c : tr) lines += c == '\n';
  CHECK(lines == 101);
  printf("ok av_stats.writer_thread\n");
}

}  // namespace

int main() {
  const char* base = getenv("YT_TEST_TMP");
  std::string t = std::string(base ? base : "/tmp") + "/avtest-XXXXXX";
  std::vector<char> buf(t.begin(), t.end());
  buf.push_back(0);
  CHECK(mkdtemp(buf.data()));
  TestSteady(buf.data());
  TestSkipsAndPlayers();
  TestLog();
  TestNotesDoNoOutput(buf.data());
  TestWriterThread(buf.data());
  printf("ALL PASSED\n");
  return 0;
}
