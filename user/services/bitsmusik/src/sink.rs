//! Where decoded samples go: the kernel's ALSA PCM ([`crate::pcm::Alsa`], the only sink of the real service) or
//! [`Null`], a sink paced by the clock that keeps nothing (host tests: they never open /dev/snd). Samples are
//! f32, interleaved, at the rate and channel count `open` answered.

use std::time::{Duration, Instant};

pub trait Sink: Send {
    /// Opens for `rate` Hz and `channels` (1 or 2): the rate the sink plays at (the player resamples to it when
    /// it differs).
    fn open(&mut self, rate: u32, channels: u32) -> Result<u32, String>;
    /// Writes whole frames; returns once they are queued (or the sink's deadline passed: an error).
    fn write(&mut self, samples: &[f32]) -> Result<(), String>;
    /// Frames played since `open` (written minus what still waits in the device).
    fn played(&mut self) -> u64;
    /// Throws away what is queued (stop, next, prev).
    fn drop_queued(&mut self);
    /// Waits until what is queued has played (end of the queue), at most `max`.
    fn drain(&mut self, max: Duration);
    fn close(&mut self);
    /// The output in words for `status` (`hdmi`, `jack`, `usb`, `none` = nothing connected, `null` = test sink).
    fn output(&self) -> String;
    /// The output the kernel is set to when nothing plays (`hdmi`, `jack`, `usb`, else `auto`).
    fn target(&self) -> String {
        "auto".into()
    }
}

/// A clock-paced sink without a device: `write` returns when the wall clock reaches what was written minus a
/// queue of [`Null::QUEUE`].
pub struct Null {
    rate: u32,
    channels: u32,
    start: Option<Instant>,
    written: u64,
}

impl Null {
    const QUEUE: Duration = Duration::from_millis(100);

    pub fn new() -> Null {
        Null {
            rate: 48_000,
            channels: 2,
            start: None,
            written: 0,
        }
    }

    fn elapsed_frames(&self) -> u64 {
        self.start
            .map_or(0, |s| (s.elapsed().as_secs_f64() * self.rate as f64) as u64)
    }
}

impl Default for Null {
    fn default() -> Self {
        Null::new()
    }
}

impl Sink for Null {
    fn open(&mut self, rate: u32, channels: u32) -> Result<u32, String> {
        *self = Null {
            rate: rate.max(1),
            channels: channels.max(1),
            start: None,
            written: 0,
        };
        Ok(self.rate)
    }
    fn write(&mut self, samples: &[f32]) -> Result<(), String> {
        let now = self.elapsed_frames();
        if self.start.is_none() || now > self.written {
            // Started or ran dry (paused): the clock restarts at what was written.
            self.start = Some(
                Instant::now() - Duration::from_secs_f64(self.written as f64 / self.rate as f64),
            );
        }
        self.written += (samples.len() / self.channels as usize) as u64;
        let ahead = self.written as f64 / self.rate as f64 - Null::QUEUE.as_secs_f64();
        let at = self
            .start
            .map(|s| s + Duration::from_secs_f64(ahead.max(0.0)));
        if let Some(at) = at
            && let Some(wait) = at.checked_duration_since(Instant::now())
        {
            std::thread::sleep(wait);
        }
        Ok(())
    }
    fn played(&mut self) -> u64 {
        self.elapsed_frames().min(self.written)
    }
    fn drop_queued(&mut self) {
        self.written = self.played();
    }
    fn drain(&mut self, max: Duration) {
        let left = self.written.saturating_sub(self.played());
        std::thread::sleep(Duration::from_secs_f64(left as f64 / self.rate as f64).min(max));
    }
    fn close(&mut self) {
        self.start = None;
        self.written = 0;
    }
    fn output(&self) -> String {
        "null".into()
    }
}

/// Linear resampling of interleaved f32 from one rate to another, kept across calls (no clicks between packets).
pub struct Resampler {
    from: u32,
    to: u32,
    channels: usize,
    /// Position of the next output frame in input frames, relative to `last`.
    pos: f64,
    last: Vec<f32>,
}

impl Resampler {
    pub fn new(from: u32, to: u32, channels: usize) -> Resampler {
        Resampler {
            from: from.max(1),
            to: to.max(1),
            channels: channels.max(1),
            pos: 0.0,
            last: Vec::new(),
        }
    }

    pub fn to_rate(&self) -> u32 {
        self.to
    }

    pub fn run(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        let step = self.from as f64 / self.to as f64;
        // Frames: the last one of the previous call (index 0), then this call's.
        let have_last = !self.last.is_empty();
        let n = input.len() / ch + have_last as usize;
        let frame = |i: usize, c: usize| -> f32 {
            if have_last {
                if i == 0 {
                    self.last[c]
                } else {
                    input[(i - 1) * ch + c]
                }
            } else {
                input[i * ch + c]
            }
        };
        while n >= 2 && self.pos + 1.0 < n as f64 {
            let i = self.pos as usize;
            let f = (self.pos - i as f64) as f32;
            for c in 0..ch {
                out.push(frame(i, c) * (1.0 - f) + frame(i + 1, c) * f);
            }
            self.pos += step;
        }
        if n >= 1 {
            let lastf: Vec<f32> = (0..ch).map(|c| frame(n - 1, c)).collect();
            self.pos -= (n - 1) as f64;
            self.last = lastf;
        }
        if self.pos < 0.0 {
            self.pos = 0.0;
        }
    }
}
