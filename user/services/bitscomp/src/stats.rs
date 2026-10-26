//! Frame counters for the software path's cost (WY-0 measures what the Mini-PC will pay on the CPU).

use std::time::Duration;

#[derive(Default, Clone)]
pub struct Stats {
    pub frames: u64,
    pub empty_frames: u64,
    pub commits: u64,
    pub render: Duration,
    pub present: Duration,
    pub damaged_px: u64,
    /// SYN_REPORTs read from evdev and their summed age (event time to bitscomp reading it, CLOCK_MONOTONIC).
    pub input_events: u64,
    pub input_age_us: u64,
    /// SYN_DROPPED read from evdev (the kernel's ring for bitscomp overflowed) and key presses read.
    pub input_dropped: u64,
    pub key_presses: u64,
    /// Key presses handed to the focused client (wl_keyboard, or nobody when no client has the focus).
    pub keys_forwarded: u64,
}

impl Stats {
    pub fn report(&self) -> String {
        let n = self.frames.max(1) as f64;
        format!(
            "frames={} empty={} commits={} render_avg_ms={:.3} present_avg_ms={:.3} damaged_mpx_avg={:.3} input_reports={} input_age_avg_ms={:.3} input_dropped={} key_presses={} keys_forwarded={}",
            self.frames,
            self.empty_frames,
            self.commits,
            self.render.as_secs_f64() * 1e3 / n,
            self.present.as_secs_f64() * 1e3 / n,
            self.damaged_px as f64 / n / 1e6,
            self.input_events,
            self.input_age_us as f64 / self.input_events.max(1) as f64 / 1e3,
            self.input_dropped,
            self.key_presses,
            self.keys_forwarded,
        )
    }
}
