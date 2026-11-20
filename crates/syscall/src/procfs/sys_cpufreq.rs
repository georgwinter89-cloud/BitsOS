//! SYS-K: `/sys/devices/system/cpu/cpuN/cpufreq/{scaling_cur_freq,cpuinfo_min_freq,cpuinfo_max_freq}` (kHz, `%u\n`,
//! 0444, linux-7.3.1/drivers/cpufreq/cpufreq.c `show_scaling_cur_freq`/`show_cpuinfo_*`). BitsOS has no cpufreq
//! driver and does not set the clock; it measures it (APERF/MPERF, `hal::freq`, every 2 s by the metric sampler).
//! The limits are not read from the CPU (CPPC and the P-state MSRs are not on the normal read list and there is no
//! AML for `_PSS`), so min/max are the measured range since boot widened by the reference clock (the TSC rate,
//! the CPU's nominal clock): real numbers, and `scaling_cur_freq` always lies inside them. Until the first
//! measurement the current clock reads the reference clock. Without APERF/MPERF (most guests) the directory is
//! missing, as on Linux without a cpufreq driver.

use super::attr::{Live, PAGE};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use vfs::Errno;
use vfs::synth::{DirSource, SynthEntry};

fn reference_khz() -> u64 {
    time::tsc_hz() / 1000
}

/// (min, max) as shown: the measured range widened by the reference clock.
pub fn limits(range: Option<(u64, u64)>, reference: u64) -> (u64, u64) {
    let (lo, hi) = range.unwrap_or((reference, reference));
    (lo.min(reference), hi.max(reference))
}

const FILES: [(&str, fn(usize) -> u64); 3] = [
    ("cpuinfo_max_freq", |_| limits(hal::freq::range_khz(), reference_khz()).1),
    ("cpuinfo_min_freq", |_| limits(hal::freq::range_khz(), reference_khz()).0),
    ("scaling_cur_freq", |n| hal::freq::current_khz(n).unwrap_or_else(reference_khz)),
];

struct FreqDir(usize);

impl DirSource for FreqDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let n = self.0;
        Ok(FILES
            .iter()
            .enumerate()
            .map(|(i, (name, f))| {
                let f = *f;
                SynthEntry::file(name, 0o444, Arc::new(Live::new(PAGE, move || Ok::<String, Errno>(format!("{}\n", f(n)))))).cookie(i as u64 + 1)
            })
            .collect())
    }
}

/// `cpuN/cpufreq` for an online CPU when the clock can be measured.
pub(super) fn dir(n: usize) -> Option<SynthEntry> {
    hal::freq::supported().then(|| SynthEntry::dir("cpufreq", Arc::new(FreqDir(n))).with_perm(0o755))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_shown_range_holds_the_reference_clock() {
        assert_eq!(super::limits(None, 3_800_000), (3_800_000, 3_800_000));
        assert_eq!(super::limits(Some((4_400_000, 4_450_000)), 3_800_000), (3_800_000, 4_450_000));
        assert_eq!(super::limits(Some((1_600_000, 5_100_000)), 3_800_000), (1_600_000, 5_100_000));
    }
}
