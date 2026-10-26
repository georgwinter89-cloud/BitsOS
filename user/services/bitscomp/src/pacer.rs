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

/// When the next frame may be drawn: one period after the last, on the grid, never in the past.
pub fn next_frame(last: Duration, now: Duration, period: Duration, phase: Duration) -> Duration {
    on_grid((last + period).max(now), phase, period)
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

    /// Wakes the loop at `at` (absolute CLOCK_MONOTONIC); `None` disarms.
    pub fn arm(&mut self, at: Option<Duration>) {
        // Unchanged and still ahead: nothing to tell the kernel (the loop turns often).
        if self.armed == at && at.is_none_or(|t| t > now()) {
            return;
        }
        self.armed = at;
        // A zero it_value disarms; an absolute time in the past fires at once.
        let t = at.map_or(Duration::ZERO, |d| d.max(Duration::from_nanos(1)));
        let spec = libc::itimerspec {
            it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
            it_value: libc::timespec { tv_sec: t.as_secs() as i64, tv_nsec: t.subsec_nanos() as i64 },
        };
        // SAFETY: `spec` lives across the call; the old value is not asked for.
        let r = unsafe { libc::timerfd_settime(self.fd.as_raw_fd(), libc::TFD_TIMER_ABSTIME, &spec, std::ptr::null_mut()) };
        if r < 0 {
            eprintln!("bitscomp: pacer: timerfd_settime: {}", std::io::Error::last_os_error());
        }
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
        assert_eq!(next_frame(ms(105), ms(140), ms(10), ms(5)), ms(145));
    }
}
