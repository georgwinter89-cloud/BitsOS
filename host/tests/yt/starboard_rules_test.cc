// Host tests for the two Starboard rules patch 0007 takes from src/fb (YT-5):
// the largest video offered (media_caps.h) and when ALSA frames count as
// played (alsa_played.h). Built and run by host/tests/yt/run-host-tests.sh;
// a grep in launcher.sh checks that the patch still uses both.

#include <stdio.h>
#include <stdlib.h>

#include "starboard/shared/bitsos/fb/alsa_played.h"
#include "starboard/shared/bitsos/fb/media_caps.h"

using bitsos_fb::AlsaPlayed;
using bitsos_fb::VideoSizeSupported;

#define CHECK(c)                                                   \
  do {                                                             \
    if (!(c)) {                                                    \
      fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #c); \
      exit(1);                                                     \
    }                                                              \
  } while (0)

namespace {

void TestVideoCaps() {
  CHECK(VideoSizeSupported(1920, 1080));
  CHECK(VideoSizeSupported(1080, 1920));  // vertical
  CHECK(VideoSizeSupported(1920, 1088));  // coded in 16-pixel macroblocks
  CHECK(VideoSizeSupported(1088, 1920));
  CHECK(VideoSizeSupported(1280, 720));
  CHECK(VideoSizeSupported(0, 0));        // not known yet: as upstream
  CHECK(!VideoSizeSupported(1921, 1080));
  CHECK(!VideoSizeSupported(1080, 1921));
  CHECK(!VideoSizeSupported(1920, 1089));
  CHECK(!VideoSizeSupported(2560, 1440));
  CHECK(!VideoSizeSupported(1440, 2560));
  CHECK(!VideoSizeSupported(3840, 2160));
  CHECK(!VideoSizeSupported(7680, 4320));
  CHECK(!VideoSizeSupported(1440, 1440));  // square, short side over
  printf("ok rules.video_caps\n");
}

// Written minus delay, never going back, never past what was written.
void TestPlayedFollowsDelay() {
  AlsaPlayed p;
  bool fb = true;
  CHECK(p.Played(0, 0, &fb) == 0 && !fb);
  p.Wrote(2048);
  CHECK(p.InAlsa() == 2048);
  CHECK(p.Played(2048, 1, &fb) == 0);
  CHECK(p.Played(1500, 2, &fb) == 548);
  CHECK(p.InAlsa() == 1500);
  CHECK(p.Played(1600, 3, &fb) == 0);   // silence counted in the delay
  CHECK(p.InAlsa() == 1500);            // never back
  p.Wrote(512);
  CHECK(p.Played(3000, 4, &fb) == 0);   // delay over written: nothing
  CHECK(p.Played(0, 5, &fb) == 2012);   // all played
  CHECK(p.InAlsa() == 0);
  CHECK(p.All() == 0);
  // Monotonic sum over a long run: what was reported equals what was written
  // once the delay reaches 0.
  long sum = 0;
  long written = 0;
  for (int i = 0; i < 10000; ++i) {
    p.Wrote(512);
    written += 512;
    sum += p.Played(static_cast<int>((i * 7919) % 4096), 10 + i, &fb);
    CHECK(!fb);
    CHECK(sum <= written);
  }
  sum += p.Played(0, 20000, &fb);
  CHECK(sum == written);
  printf("ok rules.played_follows_delay\n");
}

// An unknown delay is waited out for a short while, then written counts as
// played (upstream's rule) until the delay is known again.
void TestUnknownDelayFallsBack() {
  AlsaPlayed p;
  bool fb = false;
  p.Wrote(4096);
  CHECK(p.Played(-1, 1'000'000, &fb) == 0 && !fb);
  CHECK(p.Played(-1, 1'050'000, &fb) == 0 && !fb);
  CHECK(p.Played(-1, 1'100'000, &fb) == 4096 && fb);  // falls back, once
  p.Wrote(512);
  CHECK(p.Played(-1, 1'110'000, &fb) == 512 && !fb);  // still falling back
  p.Wrote(512);
  CHECK(p.Played(1024, 1'120'000, &fb) == 0 && !fb);  // known again
  CHECK(p.Played(512, 1'130'000, &fb) == 0);
  CHECK(p.Played(-1, 1'140'000, &fb) == 0 && !fb);    // a new wait
  CHECK(p.Played(-1, 1'239'999, &fb) == 0 && !fb);
  CHECK(p.Played(-1, 1'240'000, &fb) == 512 && fb);
  printf("ok rules.unknown_delay_falls_back\n");
}

// The source holding fewer frames than counted in ALSA re-syncs instead of
// waiting for frames that never come.
void TestResync() {
  AlsaPlayed p;
  bool fb = false;
  p.Wrote(4096);
  CHECK(!p.Resync(4096));
  CHECK(!p.Resync(5000));
  CHECK(p.InAlsa() == 4096);
  CHECK(p.Resync(1000));
  CHECK(p.InAlsa() == 1000);
  CHECK(p.Played(3000, 1, &fb) == 0);  // the real delay is larger: wait
  CHECK(p.Played(500, 2, &fb) == 500);
  CHECK(p.Resync(-3));                 // nonsense from the source: 0
  CHECK(p.InAlsa() == 0);
  printf("ok rules.resync\n");
}

}  // namespace

int main() {
  TestVideoCaps();
  TestPlayedFollowsDelay();
  TestUnknownDelayFallsBack();
  TestResync();
  printf("ALL PASSED\n");
  return 0;
}
