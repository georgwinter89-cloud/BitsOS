//! The wall clock (spec/core.md §7.3): UTC for CLOCK_REALTIME = the RTC's local time at boot, converted with
//! CONFIG `tz=` (default Europe/Berlin), plus the monotonic time since that read. Later NTP or
//! `clock_settime` replaces the base with `set_realtime`. The RTC itself is never written (Windows owns it).

use crate::rtc::{self, RtcError};
use crate::tz::{self, DateTime, Tz};
use ksync::IrqSpinLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base {
    /// No wall time yet (no RTC, or not read).
    None,
    /// The RTC's local time read at monotonic `mono_ns`.
    Rtc { local: DateTime, mono_ns: u64 },
    /// UTC ns set at monotonic `mono_ns` (NTP, `clock_settime`).
    Set { utc_ns: u64, mono_ns: u64 },
}

struct Wall {
    base: Base,
    tz: Tz,
    tz_named: bool,
    rtc_error: Option<RtcError>,
}

static WALL: IrqSpinLock<Wall> = IrqSpinLock::named("time.wall", Wall { base: Base::None, tz: tz::EUROPE_BERLIN, tz_named: false, rtc_error: None });

/// Reads the RTC (FADT CENTURY index, FADT "no CMOS RTC") and makes it the wall clock's base.
pub fn read_rtc(century_reg: u8, declared_absent: bool) -> Result<DateTime, RtcError> {
    let r = rtc::read(century_reg, declared_absent);
    let mono = crate::now_ns();
    let mut w = WALL.lock();
    match r {
        Ok(local) => {
            w.base = Base::Rtc { local, mono_ns: mono };
            w.rtc_error = None;
        }
        Err(e) => w.rtc_error = Some(e),
    }
    r
}

/// Sets the zone (CONFIG `tz=`).
pub fn set_tz(tz: Tz) {
    let mut w = WALL.lock();
    w.tz = tz;
    w.tz_named = true;
}

pub fn tz() -> Tz {
    WALL.lock().tz
}

/// UTC now, in ns since 1970 (`None`: no wall time known).
pub fn realtime_ns() -> Option<u64> {
    let (base, tz) = {
        let w = WALL.lock();
        (w.base, w.tz)
    };
    let now = crate::now_ns();
    match base {
        Base::None => None,
        Base::Rtc { local, mono_ns } => {
            let utc = tz.to_utc(&local);
            u64::try_from(utc).ok().map(|s| s.saturating_mul(1_000_000_000).saturating_add(now.saturating_sub(mono_ns)))
        }
        Base::Set { utc_ns, mono_ns } => Some(utc_ns.saturating_add(now.saturating_sub(mono_ns))),
    }
}

/// Sets UTC now (NTP, `clock_settime` later). The RTC stays untouched.
pub fn set_realtime(utc_ns: u64) {
    let mono = crate::now_ns();
    WALL.lock().base = Base::Set { utc_ns, mono_ns: mono };
}

/// Minutes east of UTC now ("the kernel answers minutes east of UTC", YK-3); the standard offset without a
/// wall time.
pub fn utc_offset_minutes() -> i32 {
    let tz = tz();
    match realtime_ns() {
        Some(ns) => tz.offset_at((ns / 1_000_000_000) as i64),
        None => tz.offset_at(0),
    }
}

/// UTC now as a date (`None` without a wall time).
pub fn utc_now() -> Option<DateTime> {
    realtime_ns().map(|ns| DateTime::from_unix((ns / 1_000_000_000) as i64))
}

/// Local now as a date.
pub fn local_now() -> Option<DateTime> {
    let ns = realtime_ns()?;
    let s = (ns / 1_000_000_000) as i64;
    Some(DateTime::from_unix(s + utc_offset_minutes() as i64 * 60))
}

/// What the wall clock rests on, the zone, whether `tz=` named it, and the last RTC error.
pub fn state() -> (Base, Tz, bool, Option<RtcError>) {
    let w = WALL.lock();
    (w.base, w.tz, w.tz_named, w.rtc_error)
}
