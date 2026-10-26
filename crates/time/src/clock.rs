//! The clocksource: the TSC behind `now_ns` (spec/core.md §7.1). A rate change (the first rough rate replaced by
//! the HPET calibration) rebases the clock so it never jumps back: `base_ns` is the old clock's reading at
//! `base_tsc`, and from there the new rate counts on. Readers see (base, rate) consistently through a sequence
//! counter (a seqlock of atomics: no lock, no unsafe; NMI- and IRQ-safe).
//!
//! The conversion multiplies instead of dividing: `ns = ticks * ns_mult >> 32` with `ns_mult = 10^9 * 2^32 / hz`
//! (Linux `clocksource` mult/shift), exact to ~0.5 ppb, in u128 so no delta overflows.

use core::sync::atomic::{AtomicU64, Ordering, fence};

/// Plausible TSC rates (V1 `time.asm`: 100 MHz to 10 GHz).
pub const MIN_HZ: u64 = 100_000_000;
pub const MAX_HZ: u64 = 10_000_000_000;

const NS_PER_S: u128 = 1_000_000_000;

/// ns per tick in 32.32 fixed point (pure).
pub const fn ns_mult(hz: u64) -> u64 {
    if hz == 0 {
        return 0;
    }
    (((NS_PER_S << 32) + hz as u128 / 2) / hz as u128) as u64
}

/// ticks per ns in 32.32 fixed point (pure).
pub const fn tick_mult(hz: u64) -> u64 {
    ((hz as u128) << 32).div_ceil(NS_PER_S) as u64
}

/// `ticks * mult >> 32`, saturating at u64::MAX (pure).
pub const fn scale(ticks: u64, mult: u64) -> u64 {
    let v = (ticks as u128 * mult as u128) >> 32;
    if v > u64::MAX as u128 { u64::MAX } else { v as u64 }
}

/// One consistent view of the clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub base_tsc: u64,
    pub base_ns: u64,
    pub hz: u64,
    pub ns_mult: u64,
    pub tick_mult: u64,
}

impl Snapshot {
    /// Monotonic ns at TSC value `tsc` (a TSC before the base, e.g. another CPU a few ticks behind the one that
    /// rebased, reads as the base: the clock never goes back).
    pub fn ns_at(&self, tsc: u64) -> u64 {
        if self.hz == 0 {
            return 0;
        }
        self.base_ns.saturating_add(scale(tsc.saturating_sub(self.base_tsc), self.ns_mult))
    }
    /// The TSC value at monotonic `ns` (the base for an earlier `ns`).
    pub fn tsc_at(&self, ns: u64) -> u64 {
        self.base_tsc.saturating_add(scale(ns.saturating_sub(self.base_ns), self.tick_mult))
    }
    /// TSC ticks in `ns` nanoseconds.
    pub fn ticks_in(&self, ns: u64) -> u64 {
        scale(ns, self.tick_mult)
    }
}

/// The clock (one static instance in `lib.rs`).
pub struct Clock {
    seq: AtomicU64,
    base_tsc: AtomicU64,
    base_ns: AtomicU64,
    hz: AtomicU64,
    ns_mult: AtomicU64,
    tick_mult: AtomicU64,
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock {
    pub const fn new() -> Self {
        Clock {
            seq: AtomicU64::new(0),
            base_tsc: AtomicU64::new(0),
            base_ns: AtomicU64::new(0),
            hz: AtomicU64::new(0),
            ns_mult: AtomicU64::new(0),
            tick_mult: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                core::hint::spin_loop();
                continue;
            }
            let snap = Snapshot {
                base_tsc: self.base_tsc.load(Ordering::Relaxed),
                base_ns: self.base_ns.load(Ordering::Relaxed),
                hz: self.hz.load(Ordering::Relaxed),
                ns_mult: self.ns_mult.load(Ordering::Relaxed),
                tick_mult: self.tick_mult.load(Ordering::Relaxed),
            };
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s1 {
                return snap;
            }
        }
    }

    /// Sets the rate from TSC value `tsc` on: the clock reads what it read there with the old rate (0 the first
    /// time) and counts on at `hz`. Writers are serialised by the sequence counter itself.
    pub fn set_rate(&self, tsc: u64, hz: u64) {
        let old = self.snapshot();
        let now_ns = old.ns_at(tsc);
        let mut s = self.seq.load(Ordering::Relaxed);
        loop {
            if s & 1 == 0 {
                match self.seq.compare_exchange_weak(s, s + 1, Ordering::Acquire, Ordering::Relaxed) {
                    Ok(_) => break,
                    Err(v) => s = v,
                }
            } else {
                core::hint::spin_loop();
                s = self.seq.load(Ordering::Relaxed);
            }
        }
        fence(Ordering::Release);
        self.base_tsc.store(tsc, Ordering::Relaxed);
        self.base_ns.store(now_ns, Ordering::Relaxed);
        self.hz.store(hz, Ordering::Relaxed);
        self.ns_mult.store(ns_mult(hz), Ordering::Relaxed);
        self.tick_mult.store(tick_mult(hz), Ordering::Relaxed);
        self.seq.store(s + 2, Ordering::Release);
    }

    pub fn hz(&self) -> u64 {
        self.snapshot().hz
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_is_exact_to_a_ppb() {
        for hz in [MIN_HZ, 1_000_000_000, 3_792_877_000, 3_999_999_999, MAX_HZ] {
            let m = ns_mult(hz);
            // One second of ticks is one second, within 2 ns (the mult's rounding: at most 1.2 ppb at 10 GHz).
            let ns = scale(hz, m);
            assert!(ns.abs_diff(1_000_000_000) <= 2, "{hz}: {ns}");
            // And back: one second is hz ticks within one tick.
            let t = scale(1_000_000_000, tick_mult(hz));
            assert!(t.abs_diff(hz) <= 1, "{hz}: {t}");
            // A day of ticks: within 150 us (the same 1.2 ppb).
            let day = hz as u128 * 86_400;
            assert!((scale(day as u64, m) as i128 - 86_400_000_000_000).abs() < 150_000);
        }
        assert_eq!(scale(u64::MAX, u64::MAX), u64::MAX, "saturates");
        assert_eq!(ns_mult(0), 0);
    }

    #[test]
    fn rebase_never_goes_back() {
        let c = Clock::new();
        assert_eq!(c.snapshot().ns_at(12345), 0, "no rate: 0");
        c.set_rate(1_000, 1_000_000_000);
        let s = c.snapshot();
        assert_eq!(s.ns_at(1_000), 0);
        assert_eq!(s.ns_at(2_000), 1_000);
        assert_eq!(s.ns_at(500), 0, "a TSC behind the base reads as the base");
        // At TSC 1_001_000 the clock says 1 ms; the new rate (2 GHz) counts on from there.
        c.set_rate(1_001_000, 2_000_000_000);
        let s = c.snapshot();
        assert_eq!(s.ns_at(1_001_000), 1_000_000);
        assert_eq!(s.ns_at(1_003_000), 1_001_000);
        assert_eq!(s.tsc_at(1_001_000), 1_003_000);
        assert_eq!(s.tsc_at(0), 1_001_000, "a past ns maps to the base");
        assert_eq!(s.ticks_in(1_000), 2_000);
        assert_eq!(c.hz(), 2_000_000_000);
    }
}
