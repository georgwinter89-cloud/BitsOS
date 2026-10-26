//! Re-checking a TSC rate a Reload handed over (INT-A1e; pure, host-tested). A Reload keeps the previous
//! kernel's rate instead of calibrating again (spec §7.1: no 100 ms at every Reload), so a rate that was wrong
//! once stayed wrong in every later kernel: the Mini-PC ran 139 ppm slow against its HPET, generation after
//! generation, on a rate measured by a kernel from before CP-5's calibration. So a kernel started by a Reload
//! now checks the rate it got: one (TSC, HPET) sample at calibration, a second one `SPAN_NS` later (a thread
//! takes it, the boot does not wait), the rate between the two, and `judge` decides. A correction rebases the
//! clock (it never goes back) and is what the next Reload hands on.

use crate::calib::{self, CalibError, Counter};

/// The handed-on rate stays when the HPET agrees within this (the clock's own check allows 100 ppm).
pub const LIMIT_PPM: i64 = 50;
/// A measurement further than this from the handed-on rate is not believed (the boot calibration's 2 % rule):
/// a broken counter, or a hypervisor's HPET and TSC that disagree.
pub const PLAUSIBLE_PPM: i64 = 20_000;
/// The span between the two samples at boot: a microsecond of read jitter is 1 ppm over it.
pub const SPAN_NS: u64 = 1_000_000_000;

/// The TSC (at the middle of the tightest read) and the HPET's low 32 bits read together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub tsc: u64,
    pub count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Within `LIMIT_PPM`: the handed-on rate stays.
    Kept,
    /// Replaced by the measured rate.
    Corrected,
    /// Would be replaced, but under a hypervisor the HPET is emulated: logged only.
    Hypervisor,
    /// The measurement is too far off (`PLAUSIBLE_PPM`, or outside the loader's hint) to believe.
    Implausible,
    /// No rate could be measured (counter stood still, TSC went back, span too long for the 32-bit counter).
    Failed(CalibError),
}

impl Outcome {
    pub fn name(self) -> &'static str {
        match self {
            Outcome::Kept => "kept",
            Outcome::Corrected => "corrected",
            Outcome::Hypervisor => "hypervisor",
            Outcome::Implausible => "implausible",
            Outcome::Failed(_) => "failed",
        }
    }
}

/// One re-check's result (the `tsc.recheck` event, `Calibration::recheck`, ktests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recheck {
    pub handed_hz: u64,
    /// 0 when the measurement failed.
    pub measured_hz: u64,
    /// The handed-on rate against the measured one (positive: the handed-on rate is too high, the clock ran
    /// slow; the Mini-PC's +139).
    pub ppm: i64,
    pub span_ns: u64,
    pub outcome: Outcome,
}

/// The rate between two samples. The HPET counter has 32 bits: a span the counter may have wrapped in (judged
/// by the TSC at the handed-on rate, with a margin of 2) is refused rather than misread.
pub fn rate_between(a: Sample, b: Sample, ctr: Counter, handed_hz: u64) -> Result<u64, CalibError> {
    let ticks = b.tsc.checked_sub(a.tsc).filter(|&t| t > 0).ok_or(CalibError::TscBroken)?;
    let wrap_fs = (ctr.mask as u128 + 1) * ctr.fs_per_count as u128;
    let span_fs = ticks as u128 * 1_000_000_000_000_000 / handed_hz.max(1) as u128;
    if span_fs.saturating_mul(2) >= wrap_fs {
        return Err(CalibError::Stalled);
    }
    let hz = calib::rate((a.tsc, a.count), (b.tsc, b.count), ctr)?;
    if !(crate::clock::MIN_HZ..=crate::clock::MAX_HZ).contains(&hz) {
        return Err(CalibError::Implausible(hz));
    }
    Ok(hz)
}

/// The decision (pure): keep within `LIMIT_PPM`; never believe a measurement more than `PLAUSIBLE_PPM` off or
/// outside the loader's hint (2 %); under a hypervisor only log; otherwise correct.
pub fn judge(handed_hz: u64, measured: Result<u64, CalibError>, hint_hz: u64, hypervisor: bool) -> (Outcome, i64) {
    let m = match measured {
        Ok(m) => m,
        Err(e) => return (Outcome::Failed(e), 0),
    };
    let ppm = calib::ppm(handed_hz, m);
    let outcome = if ppm.abs() <= LIMIT_PPM {
        Outcome::Kept
    } else if ppm.abs() > PLAUSIBLE_PPM || !calib::agrees_with_hint(m, hint_hz) {
        Outcome::Implausible
    } else if hypervisor {
        Outcome::Hypervisor
    } else {
        Outcome::Corrected
    };
    (outcome, ppm)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HPET: Counter = Counter { mask: 0xFFFF_FFFF, fs_per_count: 69_841_279 }; // 14.318 MHz
    const TRUE_HZ: u64 = 3_792_876_000;
    const HANDED: u64 = 3_793_403_200; // the Mini-PC's handed-on rate, +139 ppm

    /// Two samples `secs` apart on a machine whose TSC really runs at `hz`, the HPET starting at `c0`.
    fn pair(hz: u64, secs_x1000: u64, c0: u64) -> (Sample, Sample) {
        let fs = secs_x1000 as u128 * 1_000_000_000_000;
        let a = Sample { tsc: 1_000_000, count: c0 };
        let b = Sample {
            tsc: a.tsc + (fs * hz as u128 / 1_000_000_000_000_000) as u64,
            count: (c0 + (fs / HPET.fs_per_count as u128) as u64) & HPET.mask,
        };
        (a, b)
    }

    #[test]
    fn the_mini_pc_case_is_corrected_to_the_hpet_rate() {
        let (a, b) = pair(TRUE_HZ, 1000, 123);
        let m = rate_between(a, b, HPET, HANDED).unwrap();
        assert!(calib::ppm(m, TRUE_HZ).abs() <= 1, "{m}");
        let (o, ppm) = judge(HANDED, Ok(m), 3_800_000_000, false);
        assert_eq!(o, Outcome::Corrected);
        assert!((138..=140).contains(&ppm), "{ppm}");
        // Under a hypervisor the same numbers are only logged.
        assert_eq!(judge(HANDED, Ok(m), 3_800_000_000, true).0, Outcome::Hypervisor);
    }

    #[test]
    fn a_right_rate_is_kept() {
        let (a, b) = pair(TRUE_HZ, 1000, 0xFFFF_0000); // the counter wraps inside the span
        let m = rate_between(a, b, HPET, TRUE_HZ).unwrap();
        assert_eq!(judge(TRUE_HZ, Ok(m), 0, false).0, Outcome::Kept);
        // 50 ppm is the edge: kept; just above it is corrected (ppm truncates: 52 reads as 51).
        let at = |ppm: i64| (TRUE_HZ as i128 * (1_000_000 + ppm as i128) / 1_000_000) as u64;
        assert_eq!(judge(at(50), Ok(TRUE_HZ), 0, false).0, Outcome::Kept);
        assert_eq!(judge(at(52), Ok(TRUE_HZ), 0, false).0, Outcome::Corrected);
    }

    #[test]
    fn wild_or_failed_measurements_change_nothing() {
        // 3 % off: not believed, whatever the hint.
        assert_eq!(judge(TRUE_HZ, Ok(TRUE_HZ / 100 * 97), 0, false).0, Outcome::Implausible);
        // Within 2 % of the handed-on rate but outside the loader's hint.
        assert_eq!(judge(TRUE_HZ, Ok(TRUE_HZ / 1000 * 1015), 3_000_000_000, false).0, Outcome::Implausible);
        assert_eq!(judge(TRUE_HZ, Err(CalibError::Stalled), 0, false), (Outcome::Failed(CalibError::Stalled), 0));
        // A span the 32-bit counter may have wrapped in (300 s at 14.318 MHz) is refused, not misread.
        let (a, b) = pair(TRUE_HZ, 200_000, 0);
        assert_eq!(rate_between(a, b, HPET, TRUE_HZ), Err(CalibError::Stalled));
        let (a, _) = pair(TRUE_HZ, 1000, 0);
        assert_eq!(rate_between(a, a, HPET, TRUE_HZ), Err(CalibError::TscBroken));
        assert_eq!(Outcome::Failed(CalibError::Stalled).name(), "failed");
    }
}
