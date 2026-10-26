//! TSC calibration against a counter of known rate (pure; host-tested with simulated counters). V1
//! `time/window.asm`: one window of about 100 ms; each end is the read that two TSC reads around it held closest
//! of `SAMPLES` (a read disturbed by an SMI or a slow bus drops out), the TSC taken right before the read. The
//! counter counts up and wraps at `mask` (the Mini-PC's HPET has 32 bits: read only the low 32 and handle the
//! wrap, spec §7.1, §11.6).

/// Femtoseconds per count of the ACPI PM timer (3.579545 MHz).
pub const PM_FS: u64 = 279_365_115;
/// The PIT's input clock.
pub const PIT_HZ: u64 = 1_193_182;
/// The calibration window (spec §7.1: ~100 ms).
pub const WINDOW_FS: u64 = 100_000_000_000_000;
/// Reads per window end.
pub const SAMPLES: usize = 8;
/// A window that takes more TSC ticks than this failed (the counter stands still or runs far too slowly).
pub const MAX_WINDOW_TICKS: u64 = 1 << 33;

const FS_PER_S: u128 = 1_000_000_000_000_000;

/// A counter that counts up at `fs_per_count` and wraps at `mask + 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counter {
    pub mask: u64,
    pub fs_per_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalibError {
    /// The counter did not cover the window within `MAX_WINDOW_TICKS` TSC ticks.
    Stalled,
    /// The TSC went back or did not move.
    TscBroken,
    /// The rate came out outside 100 MHz..10 GHz.
    Implausible(u64),
}

/// (TSC right before the read, count) of the closest of `SAMPLES` reads.
fn sample(read: &mut dyn FnMut() -> u64, tsc: &mut dyn FnMut() -> u64) -> (u64, u64) {
    let mut best = (u64::MAX, 0, 0);
    for _ in 0..SAMPLES {
        let t0 = tsc();
        let c = read();
        let t1 = tsc();
        let d = t1.wrapping_sub(t0);
        if d < best.0 {
            best = (d, t0, c);
        }
    }
    (best.1, best.2)
}

/// TSC ticks per second from two (tsc, count) points (pure).
pub fn rate(start: (u64, u64), end: (u64, u64), ctr: Counter) -> Result<u64, CalibError> {
    let ticks = end.0.checked_sub(start.0).filter(|&t| t > 0).ok_or(CalibError::TscBroken)?;
    let counts = end.1.wrapping_sub(start.1) & ctr.mask;
    let fs = counts as u128 * ctr.fs_per_count as u128;
    if fs == 0 {
        return Err(CalibError::Stalled);
    }
    let hz = (ticks as u128 * FS_PER_S + fs / 2) / fs;
    Ok(hz.min(u64::MAX as u128) as u64)
}

/// Measures the TSC rate against `ctr` over `window_fs`. `pause` runs between two polls (spin hints; fewer VM
/// exits). The caller keeps interrupts as it likes: each end's reads are judged by their own TSC distance.
pub fn measure(read: &mut dyn FnMut() -> u64, tsc: &mut dyn FnMut() -> u64, pause: &mut dyn FnMut(), ctr: Counter, window_fs: u64) -> Result<u64, CalibError> {
    let need = (window_fs / ctr.fs_per_count.max(1)).clamp(1, ctr.mask);
    read(); // the first read can be slow (a VM exit maps the page)
    let start = sample(read, tsc);
    loop {
        pause();
        let c = read();
        if c.wrapping_sub(start.1) & ctr.mask >= need {
            break;
        }
        let now = tsc();
        if now < start.0 {
            return Err(CalibError::TscBroken);
        }
        if now - start.0 > MAX_WINDOW_TICKS {
            return Err(CalibError::Stalled);
        }
    }
    let end = sample(read, tsc);
    let hz = rate(start, end, ctr)?;
    if !(crate::clock::MIN_HZ..=crate::clock::MAX_HZ).contains(&hz) {
        return Err(CalibError::Implausible(hz));
    }
    Ok(hz)
}

/// Whether a measured rate fits the loader's value (V1 rule): within 2 % of `hint` when the hint itself is
/// plausible, else only the 100 MHz..10 GHz range.
pub fn agrees_with_hint(hz: u64, hint: u64) -> bool {
    if !(crate::clock::MIN_HZ..=crate::clock::MAX_HZ).contains(&hz) {
        return false;
    }
    if !(crate::clock::MIN_HZ..=crate::clock::MAX_HZ).contains(&hint) {
        return true;
    }
    hz.abs_diff(hint) as u128 * 50 <= hint as u128
}

/// Parts per million of `a` against `b` (signed; pure).
pub fn ppm(a: u64, b: u64) -> i64 {
    if b == 0 {
        return 0;
    }
    ((a as i128 - b as i128) * 1_000_000 / b as i128) as i64
}

/// The TSC rate CPUID leaf 0x15 states: crystal Hz * numerator / denominator (`None` where the crystal is not
/// enumerated, as on AMD and many Intel parts; Linux `native_calibrate_tsc`).
pub fn cpuid15_hz(eax_denominator: u32, ebx_numerator: u32, ecx_crystal_hz: u32) -> Option<u64> {
    if eax_denominator == 0 || ebx_numerator == 0 || ecx_crystal_hz == 0 {
        return None;
    }
    Some(ecx_crystal_hz as u64 * ebx_numerator as u64 / eax_denominator as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    /// A simulated machine: a time base in fs, a TSC at `tsc_hz`, a counter at `ctr`, a fixed cost per read.
    struct Sim {
        fs: Cell<u128>,
        tsc_hz: u64,
        read_cost_fs: u128,
        offset: u64,
    }

    impl Sim {
        fn tsc(&self) -> u64 {
            self.fs.set(self.fs.get() + 10_000_000); // 10 ns per rdtsc
            (self.fs.get() * self.tsc_hz as u128 / FS_PER_S) as u64
        }
        fn read(&self, ctr: Counter) -> u64 {
            self.fs.set(self.fs.get() + self.read_cost_fs);
            ((self.fs.get() / ctr.fs_per_count as u128) as u64).wrapping_add(self.offset) & ctr.mask
        }
    }

    fn run(tsc_hz: u64, ctr: Counter, offset: u64, read_cost_fs: u128) -> Result<u64, CalibError> {
        let s = Sim { fs: Cell::new(0), tsc_hz, read_cost_fs, offset };
        measure(&mut || s.read(ctr), &mut || s.tsc(), &mut || s.fs.set(s.fs.get() + 50_000_000), ctr, WINDOW_FS)
    }

    #[test]
    fn hpet_32bit_wraps_and_still_measures() {
        let hpet = Counter { mask: 0xFFFF_FFFF, fs_per_count: 69_841_279 }; // 14.318 MHz
        // Starts 1000 counts before the 32-bit wrap: the window crosses it.
        let hz = run(3_792_877_000, hpet, 0xFFFF_FFFF - 1000, 500_000_000).unwrap();
        assert!(ppm(hz, 3_792_877_000).abs() < 5, "{hz}");
    }

    #[test]
    fn pm_timer_24_bit() {
        let pm = Counter { mask: 0xFF_FFFF, fs_per_count: PM_FS };
        let hz = run(2_400_000_000, pm, 0xFF_FF00, 1_000_000_000).unwrap();
        assert!(ppm(hz, 2_400_000_000).abs() < 5, "{hz}");
    }

    #[test]
    fn a_counter_that_stands_still_fails() {
        let dead = Counter { mask: 0xFFFF_FFFF, fs_per_count: 69_841_279 };
        let s = Sim { fs: Cell::new(0), tsc_hz: 3_000_000_000, read_cost_fs: 0, offset: 0 };
        let r = measure(&mut || 7, &mut || s.tsc(), &mut || s.fs.set(s.fs.get() + 50_000_000), dead, WINDOW_FS);
        assert_eq!(r, Err(CalibError::Stalled));
    }

    #[test]
    fn implausible_rates_are_refused() {
        let hpet = Counter { mask: 0xFFFF_FFFF, fs_per_count: 69_841_279 };
        assert!(matches!(run(50_000_000, hpet, 0, 0), Err(CalibError::Implausible(_))));
        assert_eq!(rate((10, 0), (5, 100), hpet), Err(CalibError::TscBroken));
        assert_eq!(rate((10, 0), (20, 0), hpet), Err(CalibError::Stalled));
    }

    #[test]
    fn hint_rule() {
        assert!(agrees_with_hint(3_800_000_000, 3_790_000_000));
        assert!(!agrees_with_hint(3_800_000_000, 3_000_000_000), "more than 2 % off");
        assert!(agrees_with_hint(3_800_000_000, 0), "no hint: range only");
        assert!(!agrees_with_hint(50_000_000, 0));
        assert_eq!(ppm(1_000_003, 1_000_000), 3);
        assert_eq!(ppm(999_997, 1_000_000), -3);
    }

    #[test]
    fn cpuid_leaf_15() {
        // An Intel part with a 38.4 MHz crystal and ratio 188/2: 3.6096 GHz.
        assert_eq!(cpuid15_hz(2, 188, 38_400_000), Some(3_609_600_000));
        assert_eq!(cpuid15_hz(2, 188, 0), None, "crystal not enumerated");
        assert_eq!(cpuid15_hz(0, 0, 0), None);
    }
}
