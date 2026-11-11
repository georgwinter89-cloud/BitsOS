//! Frame pacing on one clock (CLOCK_MONOTONIC, the clock of Smithay's `Clock<Monotonic>`, of presentation_time
//! and of V2's FBIO_BITSOS_PACING): a timerfd armed at the next frame's time wakes the loop (ns-exact, where the
//! loop's own timeout only has milliseconds). Frames sit on the output's vblank grid when the back end knows it,
//! else on a 60 Hz grid of their own. Frame callbacks get the frame's time; presentation feedback says the frame
//! reached the screen one period later (V2's deferred io copies it at that vblank).

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::time::Duration;

pub const DEFAULT_PERIOD: Duration = Duration::from_micros(16_667);

pub struct Pacer {
    fd: OwnedFd,
    armed: Option<Duration>,
}

pub fn now() -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid timespec to write.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// The first grid point (`phase` + k * `period`) at or after `at`.
pub fn on_grid(at: Duration, phase: Duration, period: Duration) -> Duration {
    let p = period.as_nanos().max(1);
    let (a, ph) = (at.as_nanos(), phase.as_nanos() % p);
    let k = (a.saturating_sub(ph)).div_ceil(p);
    Duration::from_nanos((ph + k * p) as u64)
}

/// When the next frame may be drawn: one period after the last or now, whichever is later, on the grid point nearest
/// that, which may lie just behind it. A flip returns just past its vblank (V2's FBIOPAN_DISPLAY) and a frame is late
/// by that much: rounding up would make it wait a whole period for nothing and halve the rate (WY-20: 30 frames a
/// second at 3 ms of render). Half a period or more past a grid point, the next one is nearer.
pub fn next_frame(last: Duration, now: Duration, period: Duration, phase: Duration) -> Duration {
    on_grid((last + period).max(now).saturating_sub(period / 2), phase, period)
}

/// The time of a frame drawn at `now` that was due at `due`: its grid point, or after an idle stretch its true time.
pub fn frame_time(due: Duration, now: Duration, period: Duration) -> Duration {
    if now > due + period { now } else { due }
}

impl Pacer {
    pub fn new() -> Result<Pacer, String> {
        // SAFETY: plain timerfd_create; the fd is checked and owned below.
        let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC) };
        if fd < 0 {
            return Err(format!("timerfd_create: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: `fd` was just made and is owned by nobody else.
        Ok(Pacer { fd: unsafe { OwnedFd::from_raw_fd(fd) }, armed: None })
    }

    /// Wakes the loop at `at` (absolute CLOCK_MONOTONIC); `None` disarms. True when the kernel was told
    /// (`ctl stats` timer_sets; a time already past is set again every turn, `spin` names its source).
    pub fn arm(&mut self, at: Option<Duration>) -> bool {
        // Unchanged and still ahead: nothing to tell the kernel (the loop turns often).
        if self.armed == at && at.is_none_or(|t| t > now()) {
            return false;
        }
        self.armed = at;
        // A zero it_value disarms; an absolute time in the past fires at once.
        let t = at.map_or(Duration::ZERO, |d| d.max(Duration::from_nanos(1)));
        let spec = libc::itimerspec {
            it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
            it_value: libc::timespec { tv_sec: t.as_secs() as i64, tv_nsec: t.subsec_nanos() as i64 },
        };
        // SAFETY: `spec` lives across the call; the old value is not asked for.
        let r =
            unsafe { libc::timerfd_settime(self.fd.as_raw_fd(), libc::TFD_TIMER_ABSTIME, &spec, std::ptr::null_mut()) };
        if r < 0 {
            eprintln!("bitscomp: pacer: timerfd_settime: {}", std::io::Error::last_os_error());
        }
        true
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Reads the expirations of the timer behind `fd` so its level-triggered source quiets down.
pub fn drain(fd: BorrowedFd<'_>) {
    let mut n = 0u64;
    // SAFETY: reads 8 bytes into `n`; EAGAIN when it did not fire is fine.
    unsafe { libc::read(fd.as_raw_fd(), (&mut n as *mut u64).cast(), 8) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn grid() {
        assert_eq!(on_grid(ms(100), ms(5), ms(10)), ms(105));
        assert_eq!(on_grid(ms(105), ms(5), ms(10)), ms(105));
        assert_eq!(on_grid(ms(3), ms(1005), ms(10)), ms(5));
        // One period after the last frame, or now when the loop was late, then onto the grid.
        assert_eq!(next_frame(ms(105), ms(106), ms(10), ms(5)), ms(115));
        assert_eq!(next_frame(ms(105), ms(141), ms(10), ms(5)), ms(145));
        assert_eq!(next_frame(ms(105), ms(139), ms(10), ms(5)), ms(135), "just past a grid point: it counts");
        assert_eq!(next_frame(ms(108), ms(109), ms(10), ms(5)), ms(115), "an idle stretch's frame came off the grid");
    }

    /// Nearest-grid rounding may come sooner than one period after an off-grid frame (an idle stretch's), down to half
    /// of one, but it always lands in a later refresh: no frame is drawn that the screen could not show, with or
    /// without a flip that waits.
    #[test]
    fn no_two_frames_in_one_refresh() {
        let (period, phase) = (ms(10), ms(5));
        let slot = |t: Duration| (t.as_nanos() - phase.as_nanos()) / period.as_nanos();
        for last_us in 105_000..125_000u64 {
            let last = Duration::from_micros(last_us);
            for late_us in [0u64, 100, 4_000, 9_999, 10_000, 14_000, 25_000] {
                let next = next_frame(last, last + Duration::from_micros(late_us), period, phase);
                assert!(slot(next) > slot(last), "{last:?} late {late_us} us: {next:?}");
                assert!(next >= last + period / 2, "{last:?} late {late_us} us: {next:?}");
            }
        }
    }

    /// What the main loop does with a repeating effect, on a virtual clock (`fx::plan::top` asks for the next step
    /// from the frame's time). `wait`: the flip blocks in `present` until just past the next vblank.
    fn frames_in_a_second(render: Duration, wait: bool, start: Duration) -> u32 {
        let period = Duration::from_nanos(16_666_667);
        let after_vblank = Duration::from_micros(500);
        let mut fx = crate::fx::Fx::default();
        let (mut t, mut next, mut frames) = (start, start, 0);
        fx.shown(start, Duration::ZERO);
        loop {
            // The loop sleeps until the step is due, and the frame until its grid point.
            t = t.max(fx.due().expect("the effect asks for every frame")).max(next);
            if t >= start + Duration::from_secs(1) {
                return frames;
            }
            assert!(fx.tick(t));
            let frame = frame_time(next, t, period);
            fx.shown(frame, period);
            frames += 1;
            t += render;
            if wait {
                t = on_grid(t, Duration::ZERO, period) + after_vblank;
            }
            next = next_frame(frame, t, period, Duration::ZERO);
        }
    }

    #[test]
    fn a_repeating_effect_gets_every_refresh() {
        let fast = ms(3);
        let n = frames_in_a_second(fast, true, ms(1000));
        assert!((59..=61).contains(&n), "the flip waits, 3 ms of render: {n} frames, not 30");
        let n = frames_in_a_second(fast, false, ms(1000));
        assert!((59..=61).contains(&n), "nothing waits: {n}");
        let n = frames_in_a_second(ms(14), true, ms(1000));
        assert!((59..=61).contains(&n), "a frame that just fits, every time: {n}");
        let n = frames_in_a_second(fast, true, ms(1007));
        assert!((59..=61).contains(&n), "starting off the grid after an idle stretch: {n}");
        let n = frames_in_a_second(fast, false, ms(1007));
        assert!((59..=61).contains(&n), "the same with a flip that does not wait: {n}");
        let n = frames_in_a_second(ms(20), true, ms(1000));
        assert!((29..=31).contains(&n), "a frame too slow for one refresh gets every second one: {n}");
    }
}
