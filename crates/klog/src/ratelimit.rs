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
pub(crate) fn admit(subsystem: &'static str, name: &'static str, level: crate::Level, now: u64) -> bool {
    let period = PERIOD_NS.load(Ordering::Relaxed);
    if period == 0 || now == 0 || subsystem == "klog" || SUSPENDED.load(Ordering::Relaxed) != 0 {
        return true;
    }
    let (_, key) = keys::lookup(subsystem, name);
    if admit_at(&key.tat, now, period, TOLERANCE_NS.load(Ordering::Relaxed)) {
        return true;
    }
    // Count and level in one word (`count_one`), so `report` takes both at once: a record is never counted in
    // one round and summarised at another round's level.
    let _ = key.suppressed.try_update(Ordering::Relaxed, Ordering::Relaxed, |w| Some(count_one(w, level)));
    SUPPRESSED.inc();
    false
}

/// Records suppressed since boot.
pub fn suppressed() -> u64 {
    SUPPRESSED.get()
}

/// klogd, once a second: reports and clears the per-key counts (one record per key that lost records, at the
/// level of the records it lost).
/// Returns the records reported.
pub fn sweep() -> u64 {
    let mut total = 0;
    keys::for_each(|k| total += report(k));
    total
}

/// Reports and clears one key's count (`sweep`); the records reported.
pub(crate) fn report(k: &keys::Key) -> u64 {
    if k.suppressed.load(Ordering::Relaxed) & COUNT_MASK == 0 {
        return 0;
    }
    let Some((sub, name)) = k.names() else { return 0 };
    let (n, kept) = unpack(k.suppressed.swap(0, Ordering::Relaxed));
    if n == 0 {
        return 0;
    }
    // The summary carries the level of what it stands for: a storm of Debug records is not a warning (Georg
    // 26.10.: a warning only when something is wrong; Linux's lib/ratelimit.c prints its "callbacks suppressed"
    // line at KERN_WARNING whatever was limited, BitsOS does not). A count without a level (written directly,
    // not by `admit`) is a handled state: Info.
    let level = kept.unwrap_or(crate::Level::Info);
    // Shown when the records it stands for would be (their subsystem's setting), whatever "klog"'s is.
    if level <= crate::levels::effective(sub, false) {
        // event! takes a literal level; this summary's level is a runtime value (the suppressed records' level),
        // so the Event is built by hand, mirroring the macro's field construction.
        let fields = [
            crate::Field { key: "subsystem", value: crate::IntoValue::into_value(sub) },
            crate::Field { key: "event", value: crate::IntoValue::into_value(name) },
            crate::Field { key: "count", value: crate::IntoValue::into_value(&n) },
        ];
        crate::record_admitted(&crate::Event { level, subsystem: "klog", name: "ratelimit.suppressed", fields: &fields, message: None }, false);
    }
    n
}

/// `Key::suppressed` holds the count in its low 56 bits and the most severe suppressed level in the top byte
/// (`Level as u8 + 1`; 0: none kept).
pub(crate) const COUNT_MASK: u64 = (1 << 56) - 1;

/// The word after one more suppressed record at `level` (saturating count, most severe level kept).
pub(crate) fn count_one(w: u64, level: crate::Level) -> u64 {
    let count = ((w & COUNT_MASK) + 1).min(COUNT_MASK);
    let code = level as u64 + 1;
    let old = w >> 56;
    let code = if old == 0 { code } else { old.min(code) };
    (code << 56) | count
}

/// (count, most severe level) of a `Key::suppressed` word.
pub(crate) fn unpack(w: u64) -> (u64, Option<crate::Level>) {
    use crate::Level::*;
    let code = (w >> 56) as u8;
    (w & COUNT_MASK, [Error, Warn, Info, Debug, Trace].into_iter().find(|l| *l as u8 + 1 == code))
}
