// BitsOS V2 Cobalt backend: when frames written to ALSA count as played
// (YT-5).
//
// Used by starboard/shared/alsa/alsa_audio_sink_type.cc (patch 0007). Upstream
// reports frames to the source as consumed when they are written, so the audio
// clock (and every video frame with it) ran ahead of the speaker by all that
// ALSA holds. Here a frame counts as played when written minus snd_pcm_delay(),
// as the PulseAudio sink does with pa_stream_get_time(). The sink logs; this
// class only counts. Header-only, so the patched file needs no new GN
// dependency; host tests: host/tests/yt/alsa_played_test.cc.

#ifndef STARBOARD_SHARED_BITSOS_FB_ALSA_PLAYED_H_
#define STARBOARD_SHARED_BITSOS_FB_ALSA_PLAYED_H_

#include <stdint.h>

namespace bitsos_fb {

class AlsaPlayed {
 public:
  // How long the delay may stay unknown before written counts as played
  // again (upstream's rule), so that a failing snd_pcm_delay() cannot stop
  // the clock: the source would then hold only frames already in ALSA and
  // nothing more would be written or reported.
  static constexpr int64_t kUnknownDelayFallbackUs = 100'000;

  // |frames| more were written at playback rate 1.
  void Wrote(int frames) {
    if (frames > 0) {
      written_ += frames;
    }
  }

  // Written but not yet reported as played: what the source still holds and
  // ALSA has.
  int64_t InAlsa() const { return written_ - reported_; }

  // Frames to report as played now, given snd_pcm_delay() (< 0: unknown) at
  // |now_us|. The delay may also count silence the idle loop wrote: never
  // below what was reported, never more than was written. |*fell_back| is set
  // on the call that gives up on an unknown delay.
  int Played(int delayed_frames, int64_t now_us, bool* fell_back) {
    *fell_back = false;
    if (delayed_frames < 0) {
      if (unknown_since_us_ < 0) {
        unknown_since_us_ = now_us;
      }
      if (now_us - unknown_since_us_ < kUnknownDelayFallbackUs) {
        return 0;  // report later
      }
      *fell_back = !falling_back_;
      falling_back_ = true;
      return All();
    }
    unknown_since_us_ = -1;
    falling_back_ = false;
    const int64_t played = written_ - delayed_frames;
    if (played <= reported_) {
      return 0;
    }
    const int64_t n = played - reported_;
    reported_ = played;
    return static_cast<int>(n);
  }

  // Everything written counts as played (drained, or the resampling path).
  int All() {
    const int64_t n = written_ - reported_;
    reported_ = written_;
    return static_cast<int>(n);
  }

  // The source holds |frames_in_buffer| frames it has not seen reported. If
  // that is less than InAlsa(), the source was reset under the sink (Seek
  // destroys the sink, so this should not happen) or the count is wrong:
  // from now on only what the source holds counts as being in ALSA. Without
  // this the sink would wait for frames that never come. True if it
  // re-synced.
  bool Resync(int frames_in_buffer) {
    const int64_t holds = frames_in_buffer > 0 ? frames_in_buffer : 0;
    if (holds >= InAlsa()) {
      return false;
    }
    written_ = reported_ + holds;
    return true;
  }

 private:
  int64_t written_ = 0;
  int64_t reported_ = 0;
  int64_t unknown_since_us_ = -1;
  bool falling_back_ = false;
};

}  // namespace bitsos_fb

#endif  // STARBOARD_SHARED_BITSOS_FB_ALSA_PLAYED_H_
