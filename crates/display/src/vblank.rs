//! Timer-based vblank estimate (DP-1) for devices that cannot tell when the scanout starts a frame (GOP): a
//! frame clock computed from the one kernel clock, so no timer has to tick and nothing drifts by accumulation.
//! Default 60 Hz; a known refresh (a mode with `refresh_mhz`, later EDID) replaces it. The phase is arbitrary:
//! GOP gives no vblank, so this paces presents (DP-2 WAITFORVSYNC, page-flip pacing later) but cannot prevent
//! tearing. A refresh change re-anchors the clock so the sequence never goes backwards.
//!
//! DP-3: the clock knows where its rate came from (`RefreshSource`) and may run at an exact period (EDID's
//! `htotal * vtotal / clock`, to the ns) instead of one derived from the millihertz rate.

use devmodel::display::VblankEvent;
use ksync::SpinLock;

/// 60 Hz in millihertz.
pub const DEFAULT_REFRESH_MHZ: u32 = 60_000;

/// Where the clock's rate came from (fbdev's pacing ioctl reports it; the numbers are ABI there).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum RefreshSource {
    /// Nothing known: the 60 Hz default.
    Default = 0,
    /// CONFIG `display_refresh=` (or a test).
    Config = 1,
    /// The monitor's EDID timing for the scanout mode.
    Edid = 2,
    /// A display engine's own vblank (GX later).
    Hardware = 3,
}

impl RefreshSource {
    pub fn name(self) -> &'static str {
        match self {
            RefreshSource::Default => "default",
            RefreshSource::Config => "config",
            RefreshSource::Edid => "edid",
            RefreshSource::Hardware => "hardware",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Anchor {
    epoch_ns: u64,
    base_seq: u64,
    period_ns: u64,
    refresh_mhz: u32,
    source: RefreshSource,
}

/// Period of `mhz` millihertz in ns (60 Hz: 16,666,666 ns; 59.95 Hz: 16,680,567 ns).
pub fn period_ns(mhz: u32) -> u64 {
    let mhz = if mhz == 0 { DEFAULT_REFRESH_MHZ } else { mhz };
    1_000_000_000_000 / mhz as u64
}

impl Anchor {
    fn at(&self, now_ns: u64) -> VblankEvent {
        let n = now_ns.saturating_sub(self.epoch_ns) / self.period_ns;
        VblankEvent { sequence: self.base_seq + n, time_ns: self.epoch_ns + n * self.period_ns }
    }
}

/// One consistent reading of the clock (`VblankClock::snapshot`).
#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    pub refresh_mhz: u32,
    pub period_ns: u64,
    pub source: RefreshSource,
    pub last: VblankEvent,
    pub next: VblankEvent,
}

pub struct VblankClock {
    a: SpinLock<Anchor>,
}

impl VblankClock {
    /// A clock whose frame 0 starts at `epoch_ns`; `refresh_mhz` 0 = unknown = 60 Hz.
    pub fn new(epoch_ns: u64, refresh_mhz: u32) -> Self {
        let source = if refresh_mhz == 0 { RefreshSource::Default } else { RefreshSource::Config };
        let refresh_mhz = if refresh_mhz == 0 { DEFAULT_REFRESH_MHZ } else { refresh_mhz };
        let a = Anchor { epoch_ns, base_seq: 0, period_ns: period_ns(refresh_mhz), refresh_mhz, source };
        VblankClock { a: SpinLock::named("display.vblank", a) }
    }

    pub fn refresh_mhz(&self) -> u32 {
        self.a.lock().refresh_mhz
    }
    pub fn period_ns(&self) -> u64 {
        self.a.lock().period_ns
    }
    pub fn source(&self) -> RefreshSource {
        self.a.lock().source
    }
    /// Rate, period and source read together (one lock).
    pub fn rate(&self) -> (u32, u64, RefreshSource) {
        let a = *self.a.lock();
        (a.refresh_mhz, a.period_ns, a.source)
    }

    /// Rate, period, source and the vblanks around `now_ns` from one anchor (one lock): a concurrent rate
    /// change cannot pair a period with vblanks of another anchor (`next.time_ns == last.time_ns + period_ns`).
    pub fn snapshot(&self, now_ns: u64) -> Snapshot {
        let a = *self.a.lock();
        let last = a.at(now_ns);
        let next = VblankEvent { sequence: last.sequence + 1, time_ns: last.time_ns + a.period_ns };
        Snapshot { refresh_mhz: a.refresh_mhz, period_ns: a.period_ns, source: a.source, last, next }
    }

    /// The last estimated vblank at or before `now_ns` (its sequence number and time).
    pub fn last(&self, now_ns: u64) -> VblankEvent {
        self.a.lock().at(now_ns)
    }

    /// The first estimated vblank after `now_ns`.
    pub fn next(&self, now_ns: u64) -> VblankEvent {
        self.snapshot(now_ns).next
    }

    /// Switches to another refresh rate at `now_ns`; the next vblank keeps counting from the last one.
    /// 0 = back to the 60 Hz default (source Default); otherwise the source is Config.
    pub fn set_refresh(&self, now_ns: u64, refresh_mhz: u32) {
        let source = if refresh_mhz == 0 { RefreshSource::Default } else { RefreshSource::Config };
        let refresh_mhz = if refresh_mhz == 0 { DEFAULT_REFRESH_MHZ } else { refresh_mhz };
        self.set_period(now_ns, refresh_mhz, period_ns(refresh_mhz), source);
    }

    /// Like `set_refresh` with an exact period (EDID: `htotal * vtotal / clock`); a period of 0 or a rate of 0
    /// falls back to the millihertz rate or the default.
    pub fn set_period(&self, now_ns: u64, refresh_mhz: u32, period: u64, source: RefreshSource) {
        let refresh_mhz = if refresh_mhz == 0 { DEFAULT_REFRESH_MHZ } else { refresh_mhz };
        let period = if period == 0 { period_ns(refresh_mhz) } else { period };
        let mut a = self.a.lock();
        let l = a.at(now_ns);
        *a = Anchor { epoch_ns: l.time_ns, base_seq: l.sequence, period_ns: period, refresh_mhz, source };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sixty_hertz_by_default() {
        let c = VblankClock::new(1_000, 0);
        assert_eq!(c.refresh_mhz(), 60_000);
        assert_eq!(c.period_ns(), 16_666_666);
        assert_eq!(c.last(1_000).sequence, 0);
        let e = c.last(1_000 + 3 * 16_666_666 + 5);
        assert_eq!((e.sequence, e.time_ns), (3, 1_000 + 3 * 16_666_666));
        assert_eq!(c.next(1_000).time_ns, 1_000 + 16_666_666);
        assert_eq!(c.last(0).sequence, 0, "before the epoch: frame 0");
        assert_eq!(period_ns(59_950), 16_680_567);
    }

    #[test]
    fn refresh_change_keeps_counting() {
        let c = VblankClock::new(0, 60_000);
        let before = c.last(100_000_000);
        c.set_refresh(100_000_000, 59_950);
        let after = c.last(100_000_000);
        assert_eq!((after.sequence, after.time_ns), (before.sequence, before.time_ns), "same vblank right at the switch");
        let later = c.last(100_000_000 + 2 * 16_680_567);
        assert_eq!(later.sequence, before.sequence + 2);
        assert!(later.time_ns > before.time_ns);
        assert_eq!(c.source(), RefreshSource::Config);
    }

    #[test]
    fn exact_period_and_source() {
        let c = VblankClock::new(0, 0);
        assert_eq!(c.rate(), (60_000, 16_666_666, RefreshSource::Default));
        c.set_period(50_000_000, 59_951, 16_680_414, RefreshSource::Edid);
        assert_eq!(c.rate(), (59_951, 16_680_414, RefreshSource::Edid));
        let a = c.last(50_000_000);
        let b = c.last(50_000_000 + 1000 * 16_680_414);
        assert_eq!((b.sequence - a.sequence, b.time_ns - a.time_ns), (1000, 1000 * 16_680_414), "no drift over 1000 frames");
        c.set_period(60_000_000, 0, 0, RefreshSource::Default);
        assert_eq!(c.rate(), (60_000, 16_666_666, RefreshSource::Default));
        c.set_refresh(70_000_000, 0);
        assert_eq!(c.source(), RefreshSource::Default);
    }

    #[test]
    fn snapshot_is_one_anchor() {
        let c = VblankClock::new(0, 0);
        c.set_period(10_000_000, 59_951, 16_680_414, RefreshSource::Edid);
        let now = 10_000_000 + 5 * 16_680_414 + 7;
        let s = c.snapshot(now);
        assert_eq!((s.refresh_mhz, s.period_ns, s.source), c.rate());
        let (l, n) = (c.last(now), c.next(now));
        assert_eq!((s.last.sequence, s.last.time_ns, s.next.sequence, s.next.time_ns), (l.sequence, l.time_ns, n.sequence, n.time_ns));
        assert_eq!((s.next.sequence, s.next.time_ns), (s.last.sequence + 1, s.last.time_ns + s.period_ns));
        assert!(s.last.time_ns <= now && now < s.next.time_ns);
    }
}
