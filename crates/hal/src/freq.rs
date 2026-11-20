//! SYS-K: the CPUs' current clock from APERF/MPERF (linux-7.3.1/arch/x86/kernel/cpu/aperfmperf.c). Each CPU reads
//! its own pair (they are per-CPU MSRs) in `sample_irq`, called on it through `smp::call_on`; the metric sampler
//! turns two samples into kHz (`khz`) and keeps the result here for `/sys/devices/system/cpu/cpuN/cpufreq`.
//! Only reads: CPUID leaf 6 ECX bit 0 says both MSRs exist, and the reads are fault-safe on top (`msr::try_read`).

use core::sync::atomic::{AtomicU64, Ordering};

pub const IA32_MPERF: u32 = 0xE7;
pub const IA32_APERF: u32 = 0xE8;
/// CPUs beyond this have no clock shown (the Mini-PC has 16).
pub const MAX_CPUS: usize = 256;

static APERF: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static MPERF: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// The clock last measured per CPU (kHz, 0: none yet), and the lowest and highest measured since boot (all CPUs).
static CUR_KHZ: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static MIN_KHZ: AtomicU64 = AtomicU64::new(u64::MAX);
static MAX_KHZ: AtomicU64 = AtomicU64::new(0);

/// CPUID.6:ECX bit 0 (Linux X86_FEATURE_APERFMPERF).
pub fn supported() -> bool {
    crate::cpu::cpuid(0, 0).eax >= 6 && crate::cpu::cpuid(6, 0).ecx & 1 != 0
}

/// Runs on CPU `cpu` (hard-IRQ context of `smp::call_on`: no heap, no locks): stores its APERF and MPERF.
/// Callers check `supported` first.
pub fn sample_irq(cpu: u64) -> u64 {
    let Some(i) = usize::try_from(cpu).ok().filter(|&i| i < MAX_CPUS) else { return 0 };
    // MPERF first as Linux's `aperfmperf_get_khz` reads them back to back; the order only shifts the ratio by a
    // few cycles.
    // Fault-safe (GP-CPU): on the Mini-PC both registers are there and read as before; on a CPU nobody has booted
    // a missing one answers "no sample" (0) once every two seconds instead of ending the kernel from an interrupt.
    let (Some(m), Some(a)) = (crate::msr::try_read(IA32_MPERF), crate::msr::try_read(IA32_APERF)) else { return 0 };
    MPERF[i].store(m, Ordering::Relaxed);
    APERF[i].store(a, Ordering::Relaxed);
    1
}

/// The pair `sample_irq` last stored for `cpu` (APERF, MPERF).
pub fn sample(cpu: usize) -> Option<(u64, u64)> {
    (cpu < MAX_CPUS).then(|| (APERF[cpu].load(Ordering::Relaxed), MPERF[cpu].load(Ordering::Relaxed)))
}

/// The clock between two samples in kHz: the reference rate (MPERF ticks at the TSC rate, `tsc_khz`) times the
/// share of actual to reference cycles. None when MPERF did not move (the CPU slept the whole time) or a counter
/// went back (reset).
pub fn khz(before: (u64, u64), after: (u64, u64), tsc_khz: u64) -> Option<u64> {
    let da = after.0.checked_sub(before.0)?;
    let dm = after.1.checked_sub(before.1).filter(|&d| d > 0)?;
    Some((da as u128 * tsc_khz as u128 / dm as u128).min(u64::MAX as u128) as u64)
}

/// Keeps `khz` as `cpu`'s current clock and widens the range seen since boot.
pub fn note(cpu: usize, khz: u64) {
    if cpu >= MAX_CPUS || khz == 0 {
        return;
    }
    CUR_KHZ[cpu].store(khz, Ordering::Relaxed);
    MIN_KHZ.fetch_min(khz, Ordering::Relaxed);
    MAX_KHZ.fetch_max(khz, Ordering::Relaxed);
}

/// `cpu`'s clock last measured (kHz).
pub fn current_khz(cpu: usize) -> Option<u64> {
    (cpu < MAX_CPUS).then(|| CUR_KHZ[cpu].load(Ordering::Relaxed)).filter(|&k| k != 0)
}

/// Lowest and highest clock measured on any CPU since boot (kHz).
pub fn range_khz() -> Option<(u64, u64)> {
    let (lo, hi) = (MIN_KHZ.load(Ordering::Relaxed), MAX_KHZ.load(Ordering::Relaxed));
    (hi != 0 && lo <= hi).then_some((lo, hi))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_is_the_reference_rate_times_the_cycle_share() {
        // 3.8 GHz reference, 1.17 actual cycles per reference cycle: 4.446 GHz.
        assert_eq!(khz((1_000, 1_000), (1_000 + 117_000, 1_000 + 100_000), 3_800_000), Some(4_446_000));
        assert_eq!(khz((5, 9), (7, 9), 3_800_000), None, "MPERF did not move: the CPU slept");
        assert_eq!(khz((9, 9), (5, 20), 3_800_000), None, "APERF went back");
    }

    #[test]
    fn the_range_widens_and_zero_is_no_measurement() {
        note(3, 0);
        assert_eq!(current_khz(3), None);
        note(3, 4_400_000);
        note(4, 1_600_000);
        assert_eq!(current_khz(3), Some(4_400_000));
        let (lo, hi) = range_khz().unwrap();
        assert!(lo <= 1_600_000 && hi >= 4_400_000);
        assert_eq!(current_khz(MAX_CPUS), None);
    }
}
