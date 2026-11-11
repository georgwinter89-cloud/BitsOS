//! Frame counters for the software path's cost (WY-0 measures what the Mini-PC will pay on the CPU).

use std::time::Duration;

#[derive(Default, Clone)]
pub struct Stats {
    pub frames: u64,
    pub empty_frames: u64,
    pub commits: u64,
    pub render: Duration,
    /// Parts of `render`: the elements with their images, and the effects' work before the draw.
    pub build: Duration,
    pub prepare: Duration,
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
    /// WY-13: main loop turns, turns that slept until a time already past (`spin`), timerfd re-arms (`pacer`),
    /// and clients whose data still waited after two display dispatches in a row (its epoll did not report it),
    /// which bitscomp then read directly (`clients`).
    pub loop_turns: u64,
    pub past_wakes: u64,
    pub timer_sets: u64,
    pub unread_reads: u64,
    /// WY-16: dma-bufs the renderer took and refused, commits that waited for their acquire point, buffers held
    /// until after the frame and release points signalled then.
    pub dmabuf_imported: u64,
    pub dmabuf_failed: u64,
    pub acquire_waits: u64,
    pub held_buffers: u64,
    pub release_points: u64,
    /// Gauges: commits waiting for their acquire point now, buffers and release points held now.
    pub acquire_pending: u64,
    pub held_now: u64,
}

impl Stats {
    pub fn report(&self) -> String {
        let n = self.frames.max(1) as f64;
        format!(
            "frames={} empty={} commits={} render_avg_ms={:.3} build_avg_ms={:.3} prepare_avg_ms={:.3} present_avg_ms={:.3} damaged_mpx_avg={:.3} input_reports={} input_age_avg_ms={:.3} input_dropped={} key_presses={} keys_forwarded={} loop_turns={} past_wakes={} timer_sets={} unread_reads={} dmabuf_imported={} dmabuf_failed={} acquire_waits={} held_buffers={} release_points={} acquire_pending={} held_now={}",
            self.frames,
            self.empty_frames,
            self.commits,
            self.render.as_secs_f64() * 1e3 / n,
            self.build.as_secs_f64() * 1e3 / n,
            self.prepare.as_secs_f64() * 1e3 / n,
            self.present.as_secs_f64() * 1e3 / n,
            self.damaged_px as f64 / n / 1e6,
            self.input_events,
            self.input_age_us as f64 / self.input_events.max(1) as f64 / 1e3,
            self.input_dropped,
            self.key_presses,
            self.keys_forwarded,
            self.loop_turns,
            self.past_wakes,
            self.timer_sets,
            self.unread_reads,
            self.dmabuf_imported,
            self.dmabuf_failed,
            self.acquire_waits,
            self.held_buffers,
            self.release_points,
            self.acquire_pending,
            self.held_now,
        )
    }
}
