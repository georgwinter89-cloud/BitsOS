//! Event rate limits (RB-7a, B6): a storm of one event cannot flood the ring, the serial line or the sinks.
//!
//! Per key (call site of `event!`, `keys`): at most `burst` records at once, then `rate` per second (GCRA, the
//! token bucket as one atomic "theoretical arrival time"; Linux `lib/ratelimit.c` does interval/burst under a
//! try-lock). The check runs in `record` before anything is rendered or locked: one hash, a few atomics, no
//! allocation, no wait, so it is safe in IRQ context. A suppressed record gets no sequence number (no gap) and
//! is counted per key; klogd reports the counts once a second as `klog ratelimit.suppressed` (`sweep`).
//!
//! Not limited: the panic/NMI path (`try_event!`, the crash report must be whole), klog's own events (the
//! summaries), records before the clock runs, and everything while a test holds `suspend()`.

use crate::keys;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Defaults: 1000 records at once, then 100 per second, per key. A storm at 100 000/s keeps the 4 MB ring
/// (~30 000 records) for minutes instead of a third of a second; normal bursts (boot, a ktest) stay whole.
pub const DEFAULT_BURST: u64 = 1000;
pub const DEFAULT_RATE: u64 = 100;

/// Nanoseconds between two records at the steady rate (0 = no limit).
static PERIOD_NS: AtomicU64 = AtomicU64::new(1_000_000_000 / DEFAULT_RATE);
/// How far ahead of now the arrival time may run: (burst - 1) periods.
static TOLERANCE_NS: AtomicU64 = AtomicU64::new((DEFAULT_BURST - 1) * (1_000_000_000 / DEFAULT_RATE));
static SUSPENDED: AtomicU32 = AtomicU32::new(0);
static SETTING: AtomicU64 = AtomicU64::new(DEFAULT_BURST << 32 | DEFAULT_RATE);
counter!(SUPPRESSED, "klog.suppressed");

/// `burst` records at once, then `rate` per second, per key; `rate == 0` turns the limit off.
pub fn set(burst: u64, rate: u64) {
    let burst = burst.clamp(1, u32::MAX as u64);
    let rate = rate.min(1_000_000_000);
    let period = if rate == 0 { 0 } else { 1_000_000_000 / rate };
    PERIOD_NS.store(period, Ordering::Relaxed);
    TOLERANCE_NS.store((burst - 1).saturating_mul(period), Ordering::Relaxed);
    SETTING.store(burst << 32 | rate, Ordering::Relaxed);
}

/// (burst, rate per second); rate 0 = off.
pub fn get() -> (u64, u64) {
    let s = SETTING.load(Ordering::Relaxed);
    (s >> 32, s & 0xFFFF_FFFF)
}

/// CONFIG `log_ratelimit=`: `off`, or `<burst>/<rate>` (`1000/100`).
pub fn parse(v: &str) -> Option<(u64, u64)> {
    let v = v.trim();
    if v.eq_ignore_ascii_case("off") || v == "0" {
        return Some((DEFAULT_BURST, 0));
    }
    let (b, r) = v.split_once('/')?;
    let (b, r) = (b.trim().parse::<u64>().ok()?, r.trim().trim_end_matches("/s").parse::<u64>().ok()?);
    (b >= 1 && b <= u32::MAX as u64).then_some((b, r))
}

/// While the guard lives, nothing is limited (tests that log on purpose: `ktests::with_trace`, host tests).
pub struct Suspend(());

pub fn suspend() -> Suspend {
    SUSPENDED.fetch_add(1, Ordering::AcqRel);
    Suspend(())
}

impl Drop for Suspend {
    fn drop(&mut self) {
        SUSPENDED.fetch_sub(1, Ordering::AcqRel);
    }
}

/// GCRA step: may a record arriving at `now` pass, given the arrival time in `tat`? Lock-free (one CAS loop).
pub(crate) fn admit_at(tat: &AtomicU64, now: u64, period: u64, tolerance: u64) -> bool {
    let mut cur = tat.load(Ordering::Relaxed);
    loop {
        let t = cur.max(now);
        if t - now > tolerance {
            return false;
        }
        match tat.compare_exchange_weak(cur, t + period, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(c) => cur = c,
        }
    }
}

/// The emit path's check (`record`).
pub(crate) fn admit(subsystem: &'static str, name: &'static str, now: u64) -> bool {
    let period = PERIOD_NS.load(Ordering::Relaxed);
    if period == 0 || now == 0 || subsystem == "klog" || SUSPENDED.load(Ordering::Relaxed) != 0 {
        return true;
    }
    let (_, key) = keys::lookup(subsystem, name);
    if admit_at(&key.tat, now, period, TOLERANCE_NS.load(Ordering::Relaxed)) {
        return true;
    }
    key.suppressed.fetch_add(1, Ordering::Relaxed);
    SUPPRESSED.inc();
    false
}

/// Records suppressed since boot.
pub fn suppressed() -> u64 {
    SUPPRESSED.get()
}

/// klogd, once a second: reports and clears the per-key counts (one Warn record per key that lost records).
/// Returns the records reported.
pub fn sweep() -> u64 {
    let mut total = 0;
    keys::for_each(|k| total += report(k));
    total
}

/// Reports and clears one key's count (`sweep`); the records reported.
pub(crate) fn report(k: &keys::Key) -> u64 {
    if k.suppressed.load(Ordering::Relaxed) == 0 {
        return 0;
    }
    let Some((sub, name)) = k.names() else { return 0 };
    let n = k.suppressed.swap(0, Ordering::Relaxed);
    event!(Warn, "klog", "ratelimit.suppressed", subsystem = sub, event = name, count = n);
    n
}
