//! The last hand-back of the screen to the console (DP-2f): what fbdev's part of it cost, for the
//! `fbdev screen.handback` event, the tests' failure text and the Mini-PC check list.
//!
//! Atomics, not a lock: written only by the `fbdev` thread, read for diagnostics; a reader racing a hand-back may
//! pair fields of two hand-backs (`at_ns` is written last, so a complete record has a newer `at_ns`).

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

/// One hand-back: the holder's pid, why it ended, when it was done (kernel clock, ns), the time under
/// `fbdev.screen` (every page write-protected again), the time to show the console (the scanout guard's drop and
/// the VT's redraw in `display::sync_vt`), the pixels of the copy that showed the console frame
/// (`display::returned_pixels`: one whole frame) and all pixels presented meanwhile (a console write counts too).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HandBack {
    pub pid: u32,
    pub reason: &'static str,
    pub at_ns: u64,
    pub release_ns: u64,
    pub show_ns: u64,
    pub pixels: u64,
    pub presented: u64,
}

const REASONS: [&str; 3] = ["none", "ended", "requested"];

static PID: AtomicU32 = AtomicU32::new(0);
static REASON: AtomicU8 = AtomicU8::new(0);
static AT: AtomicU64 = AtomicU64::new(0);
static RELEASE: AtomicU64 = AtomicU64::new(0);
static SHOW: AtomicU64 = AtomicU64::new(0);
static PIXELS: AtomicU64 = AtomicU64::new(0);
static PRESENTED: AtomicU64 = AtomicU64::new(0);

pub(super) fn record(h: &HandBack) {
    PID.store(h.pid, Ordering::Relaxed);
    REASON.store(REASONS.iter().position(|r| *r == h.reason).unwrap_or(0) as u8, Ordering::Relaxed);
    RELEASE.store(h.release_ns, Ordering::Relaxed);
    SHOW.store(h.show_ns, Ordering::Relaxed);
    PIXELS.store(h.pixels, Ordering::Relaxed);
    PRESENTED.store(h.presented, Ordering::Relaxed);
    AT.store(h.at_ns, Ordering::Release);
}

/// The last hand-back (all zero before the first).
pub fn last() -> HandBack {
    let at_ns = AT.load(Ordering::Acquire);
    HandBack {
        pid: PID.load(Ordering::Relaxed),
        reason: REASONS.get(REASON.load(Ordering::Relaxed) as usize).copied().unwrap_or("none"),
        at_ns,
        release_ns: RELEASE.load(Ordering::Relaxed),
        show_ns: SHOW.load(Ordering::Relaxed),
        pixels: PIXELS.load(Ordering::Relaxed),
        presented: PRESENTED.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_last() {
        assert_eq!(last(), HandBack { reason: "none", ..HandBack::default() }, "nothing before the first");
        let h = HandBack { pid: 7, reason: "requested", at_ns: 5, release_ns: 1, show_ns: 2, pixels: 3, presented: 4 };
        record(&h);
        assert_eq!(last(), h);
        record(&HandBack { reason: "ended", ..h });
        assert_eq!(last().reason, "ended");
        // A reason outside the list is kept as "none", never a panic.
        record(&HandBack { reason: "other", at_ns: 6, ..h });
        assert_eq!((last().reason, last().at_ns), ("none", 6));
    }
}
