//! SYS-K: each round, every online CPU reads its own APERF/MPERF (`hal::freq::sample_irq` through a cross-CPU
//! call; the call wakes a sleeping CPU, a few µs every 2 s) and the clock since the previous round is kept for
//! cpufreq's `scaling_cur_freq` and the series `cpu.<n>.khz`. Without APERF/MPERF (most guests) nothing is read.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// A CPU that does not take the call in this time is skipped this round.
const CALL_NS: u64 = 20_000_000;

#[derive(Default)]
pub struct Clocks {
    on: Option<bool>,
    before: BTreeMap<usize, (u64, u64)>,
}

impl Clocks {
    /// (CPU, kHz) of the CPUs measured this round; the first round only sets the baselines.
    pub fn round(&mut self, cpus: &[usize]) -> Vec<(usize, u64)> {
        if !*self.on.get_or_insert_with(hal::freq::supported) {
            return Vec::new();
        }
        let tsc_khz = time::tsc_hz() / 1000;
        let mut out = Vec::new();
        for &c in cpus {
            if hal::smp::call_on(c, hal::freq::sample_irq, c as u64, time::now_ns, CALL_NS) != Ok(1) {
                continue;
            }
            let Some(now) = hal::freq::sample(c) else { continue };
            if let Some(k) = self.before.insert(c, now).and_then(|b| hal::freq::khz(b, now, tsc_khz)) {
                hal::freq::note(c, k);
                out.push((c, k));
            }
        }
        out
    }
}
