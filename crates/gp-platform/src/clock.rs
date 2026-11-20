//! Owner: wave GP-CPU (plan/spec/gaming-pc.md §4). Which counter the TSC is measured against on a machine whose
//! HPET may stop, and what CPUID 0x15 says about the result. Pure; `time` and `acpi` call in here.
//!
//! The Gaming-PC's Linux prints "HPET dysfunctional in PC10. Force disabled." and offers `tsc acpi_pm` only. The
//! rule below is Linux's (linux-7.3.1/arch/x86/kernel/hpet.c:914-986). It can only say yes on a GenuineIntel CPU
//! with MWAIT, so an AMD machine keeps today's order (HPET, PM timer, PIT) whatever the other inputs are.

/// CPUID 5 ECX bit 0: MWAIT extensions enumerated (linux-7.3.1/arch/x86/include/asm/mwait.h:18).
pub const CPUID5_ECX_EXTENSIONS_SUPPORTED: u32 = 0x1;
/// CPUID 5 ECX bit 1: interrupts break MWAIT even when masked (mwait.h:19).
pub const CPUID5_ECX_INTERRUPT_BREAK: u32 = 0x2;
/// CPUID 5 EDX bits 31:28: number of PC10 sub-states (hpet.c:929, `mwait_substates & (0xF << 28)`).
pub const CPUID5_EDX_PC10_SUBSTATES: u32 = 0xF << 28;
/// MSR_PKG_CST_CONFIG_CONTROL (linux-7.3.1/arch/x86/include/asm/msr-index.h:118).
pub const MSR_PKG_CST_CONFIG_CONTROL: u32 = 0xE2;
/// Its package C-state limit field and the lowest value that allows PC10 (hpet.c:974-975).
pub const PKG_CST_LIMIT_MASK: u64 = 0xF;
pub const PKG_CST_LIMIT_PC10: u64 = 8;

/// What decides whether MWAIT can take the package to PC10.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MwaitFacts {
    /// CPUID 0 says GenuineIntel.
    pub intel: bool,
    /// CPUID 1 ECX bit 3 (MONITOR/MWAIT).
    pub mwait: bool,
    pub cpuid5_ecx: u32,
    pub cpuid5_edx: u32,
}

/// Linux `mwait_pc10_supported` (hpet.c:914-930).
pub fn mwait_pc10_supported(f: &MwaitFacts) -> bool {
    f.intel
        && f.mwait
        && f.cpuid5_ecx & CPUID5_ECX_EXTENSIONS_SUPPORTED != 0
        && f.cpuid5_ecx & CPUID5_ECX_INTERRUPT_BREAK != 0
        && f.cpuid5_edx & CPUID5_EDX_PC10_SUBSTATES != 0
}

/// The package C-state limit MSR as the kernel could read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PkgLimit {
    /// Not read: the CPUID part already said no (the MSR is never touched then).
    NotAsked,
    Read(u64),
    /// The read faulted. Linux reads it unguarded (hpet.c:974); BitsOS reads fault-safe and decides here.
    Faulted,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HpetTrust {
    #[default]
    Trusted,
    /// PC10 is enumerated and the package limit allows it: the counter can stop (hpet.c:965-986).
    Pc10,
    /// PC10 is enumerated and the limit could not be read. Stricter than Linux on purpose: with a PM timer at
    /// hand, distrusting a good HPET costs nothing, trusting a bad one costs the clock.
    Pc10LimitUnreadable,
}

impl HpetTrust {
    pub fn untrusted(self) -> bool {
        self != HpetTrust::Trusted
    }
    pub fn name(self) -> &'static str {
        match self {
            HpetTrust::Trusted => "trusted",
            HpetTrust::Pc10 => "pc10",
            HpetTrust::Pc10LimitUnreadable => "pc10_limit_unreadable",
        }
    }
}

/// Whether the kernel has to read `MSR_PKG_CST_CONFIG_CONTROL` at all.
pub fn needs_pkg_limit(f: &MwaitFacts) -> bool {
    mwait_pc10_supported(f)
}

/// Linux `hpet_is_pc10_damaged` (hpet.c:965-986) without its command-line override.
pub fn hpet_trust(f: &MwaitFacts, limit: PkgLimit) -> HpetTrust {
    if !mwait_pc10_supported(f) {
        return HpetTrust::Trusted;
    }
    match limit {
        PkgLimit::Read(v) if v & PKG_CST_LIMIT_MASK < PKG_CST_LIMIT_PC10 => HpetTrust::Trusted,
        PkgLimit::Read(_) => HpetTrust::Pc10,
        PkgLimit::NotAsked | PkgLimit::Faulted => HpetTrust::Pc10LimitUnreadable,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counter {
    Hpet,
    PmTimer,
    Pit,
}

/// The counters to measure against, in order. An untrusted HPET is not in the list at all: it is neither started
/// nor read (Linux returns before `hpet_set_mapping`, hpet.c:1001-1002).
pub fn counter_order(trust: HpetTrust) -> &'static [Counter] {
    if trust.untrusted() { &[Counter::PmTimer, Counter::Pit] } else { &[Counter::Hpet, Counter::PmTimer, Counter::Pit] }
}

/// The TSC rate CPUID 0x15 states: crystal Hz x numerator / denominator, only where the crystal is enumerated
/// (linux-7.3.1/arch/x86/kernel/tsc.c:668-669, :687, :723). Linux's leaf 0x16 guess for a missing crystal
/// (tsc.c:695-701) is left out: a rate derived from a rounded MHz figure is no cross-check.
pub fn cpuid15_hz(eax_denominator: u32, ebx_numerator: u32, ecx_crystal_hz: u32) -> Option<u64> {
    if eax_denominator == 0 || ebx_numerator == 0 || ecx_crystal_hz == 0 {
        return None;
    }
    Some(ecx_crystal_hz as u64 * ebx_numerator as u64 / eax_denominator as u64)
}

/// How far a measurement may be from CPUID 0x15 and still count as the same clock. A 100 ms window against the PM
/// timer is good for a few tens of ppm; a wrong crystal table is off by percent (24 against 25 MHz is 4 %).
pub const AGREE_PPM: i64 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateFrom {
    /// The measurement stands (no CPUID 0x15 rate, or the HPET is trusted: today's behaviour).
    Measured,
    /// CPUID 0x15, confirmed by the measurement (Linux takes it unchecked as a known frequency, tsc.c:683-688).
    Cpuid15Checked,
    /// CPUID 0x15 and the measurement disagree: the measurement wins and the line says so.
    MeasuredAgainstCpuid15,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settled {
    pub hz: u64,
    pub from: RateFrom,
    /// Measurement against CPUID 0x15 in ppm, where both exist.
    pub ppm: Option<i64>,
}

pub fn ppm(a: u64, b: u64) -> i64 {
    if b == 0 {
        return 0;
    }
    ((a as i128 - b as i128) * 1_000_000 / b as i128) as i64
}

/// The rate to use after a measurement of `measured_hz`. Only an untrusted HPET changes anything: there the exact
/// CPUID 0x15 rate is taken once the measurement confirmed it.
pub fn settle(trust: HpetTrust, measured_hz: u64, cpuid15: Option<u64>) -> Settled {
    let ppm = cpuid15.map(|c| ppm(measured_hz, c));
    match (trust.untrusted(), cpuid15, ppm) {
        (true, Some(c), Some(p)) if p.abs() <= AGREE_PPM => Settled { hz: c, from: RateFrom::Cpuid15Checked, ppm },
        (true, Some(_), _) => Settled { hz: measured_hz, from: RateFrom::MeasuredAgainstCpuid15, ppm },
        _ => Settled { hz: measured_hz, from: RateFrom::Measured, ppm },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Gaming-PC's 265K (plan/hw/gaming-pc-265k-2026-11-05/cpuid-cpu0.txt): leaf 5 = 40 40 3 10102020.
    const GP: MwaitFacts = MwaitFacts { intel: true, mwait: true, cpuid5_ecx: 0x3, cpuid5_edx: 0x1010_2020 };

    #[test]
    fn the_gaming_pc_distrusts_its_hpet_as_linux_does() {
        assert!(mwait_pc10_supported(&GP));
        assert!(needs_pkg_limit(&GP));
        // Linux printed "HPET dysfunctional in PC10" there, so the limit field read 8 or more.
        for limit in 8..=15u64 {
            assert_eq!(hpet_trust(&GP, PkgLimit::Read(0x1E00_8000 | limit)), HpetTrust::Pc10);
        }
        assert_eq!(counter_order(HpetTrust::Pc10), [Counter::PmTimer, Counter::Pit]);
    }

    #[test]
    fn truth_table() {
        let all = [false, true];
        for intel in all {
            for mwait in all {
                for ext in all {
                    for brk in all {
                        for pc10 in [0u32, 1 << 28, 0xF << 28] {
                            let f = MwaitFacts { intel, mwait, cpuid5_ecx: ext as u32 | (brk as u32) << 1, cpuid5_edx: pc10 | 0x0010_2020 };
                            let cpuid_yes = intel && mwait && ext && brk && pc10 != 0;
                            assert_eq!(mwait_pc10_supported(&f), cpuid_yes, "{f:?}");
                            for limit in 0..16u64 {
                                let want = if !cpuid_yes {
                                    HpetTrust::Trusted
                                } else if limit >= 8 {
                                    HpetTrust::Pc10
                                } else {
                                    HpetTrust::Trusted
                                };
                                // Bits above the limit field never matter.
                                assert_eq!(hpet_trust(&f, PkgLimit::Read(limit)), want);
                                assert_eq!(hpet_trust(&f, PkgLimit::Read(limit | !0xF)), want);
                            }
                            let unreadable = if cpuid_yes { HpetTrust::Pc10LimitUnreadable } else { HpetTrust::Trusted };
                            assert_eq!(hpet_trust(&f, PkgLimit::Faulted), unreadable);
                            assert_eq!(hpet_trust(&f, PkgLimit::NotAsked), unreadable);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn an_amd_cpu_keeps_todays_order_whatever_it_reports() {
        // The Mini-PC's shape: not Intel. Every other input is tried, the MSR included.
        for ecx in 0..4u32 {
            for edx in [0u32, 0x1010_2020, u32::MAX] {
                for mwait in [false, true] {
                    let f = MwaitFacts { intel: false, mwait, cpuid5_ecx: ecx, cpuid5_edx: edx };
                    assert!(!needs_pkg_limit(&f), "the MSR is never read on AMD");
                    for limit in [PkgLimit::NotAsked, PkgLimit::Faulted, PkgLimit::Read(0), PkgLimit::Read(u64::MAX)] {
                        let t = hpet_trust(&f, limit);
                        assert_eq!(t, HpetTrust::Trusted);
                        assert_eq!(counter_order(t), [Counter::Hpet, Counter::PmTimer, Counter::Pit]);
                        // And the measurement is never replaced.
                        assert_eq!(settle(t, 3_792_877_000, Some(3_800_000_000)).from, RateFrom::Measured);
                        assert_eq!(settle(t, 3_792_877_000, Some(3_800_000_000)).hz, 3_792_877_000);
                    }
                }
            }
        }
    }

    #[test]
    fn cpuid15_of_the_265k() {
        // Leaf 0x15 = 2, 202, 38 400 000: 38.4 MHz x 202 / 2 (Linux: "tsc: Detected 3878.400 MHz TSC").
        assert_eq!(cpuid15_hz(2, 0xCA, 0x0249_F000), Some(3_878_400_000));
        assert_eq!(cpuid15_hz(0, 0xCA, 0x0249_F000), None);
        assert_eq!(cpuid15_hz(2, 0, 0x0249_F000), None);
        assert_eq!(cpuid15_hz(2, 0xCA, 0), None, "no crystal, no rate (no leaf 0x16 guess)");
        assert_eq!(cpuid15_hz(u32::MAX, u32::MAX, u32::MAX), Some(u32::MAX as u64), "no overflow");
    }

    #[test]
    fn settle_takes_cpuid15_only_when_the_measurement_confirms_it() {
        let c = Some(3_878_400_000);
        let s = settle(HpetTrust::Pc10, 3_878_431_000, c);
        assert_eq!((s.hz, s.from, s.ppm), (3_878_400_000, RateFrom::Cpuid15Checked, Some(7)));
        let s = settle(HpetTrust::Pc10, 3_878_400_000 - 3_878_400, c);
        assert_eq!((s.from, s.ppm), (RateFrom::Cpuid15Checked, Some(-1000)), "the bound itself agrees");
        // A firmware that states the wrong crystal (24 for 25 MHz): the measurement wins.
        let s = settle(HpetTrust::Pc10, 4_040_000_000, c);
        assert_eq!((s.hz, s.from), (4_040_000_000, RateFrom::MeasuredAgainstCpuid15));
        assert!(s.ppm.unwrap() > 40_000);
        let s = settle(HpetTrust::Pc10LimitUnreadable, 3_878_431_000, None);
        assert_eq!((s.hz, s.from, s.ppm), (3_878_431_000, RateFrom::Measured, None));
        assert_eq!(ppm(1, 0), 0);
    }
}
