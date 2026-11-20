//! CP-5's boot glue between the tables and `time` (spec/core.md §7): the HPET and the PM timer the tables name
//! pass the register guard, then `time::calibrate` starts the HPET and measures the TSC; the RTC is read with the
//! FADT's century register and "no CMOS RTC" flag. Runs once, from `live::init`.

use crate::Acpi;
use crate::live;
use time::sources::{HPET_SIZE, PmTimer};

/// The lowest PM timer port read (V1 `pm_usable`: below 0x400 sit ISA devices whose bytes a read may take).
pub const PM_PORT_MIN: u16 = 0x400;
/// CMOS indices a century register may have (Linux reads `acpi_gbl_FADT.century` when non-zero; bank 0 only,
/// above the clock and status registers 0x00-0x0D).
pub const CENTURY_MIN: u8 = 0x0E;
pub const CENTURY_MAX: u8 = 0x7F;

/// GP B5: whether this CPU can take the package to PC10, where the HPET's counter may stop. The CPUID part is
/// Intel-only and false under a hypervisor that hides MWAIT (KVM does); only when it says yes is the package
/// C-state limit read, fault-safe (Linux reads it unguarded, linux-7.3.1/arch/x86/kernel/hpet.c:974). On AMD no
/// MSR is touched and the answer is `Trusted`.
pub fn hpet_trust() -> gp_platform::clock::HpetTrust {
    use gp_platform::clock::{hpet_trust, needs_pkg_limit, MwaitFacts, PkgLimit, MSR_PKG_CST_CONFIG_CONTROL};
    let v = hal::cpu::live();
    let f = hal::cpu::detect();
    let l5 = if hal::cpu::cpuid(0, 0).eax >= 5 { hal::cpu::cpuid(5, 0) } else { Default::default() };
    let facts = MwaitFacts { intel: f.intel, mwait: v.has("monitor"), cpuid5_ecx: l5.ecx, cpuid5_edx: l5.edx };
    let limit = if needs_pkg_limit(&facts) { hal::msr::try_read(MSR_PKG_CST_CONFIG_CONTROL).map_or(PkgLimit::Faulted, PkgLimit::Read) } else { PkgLimit::NotAsked };
    hpet_trust(&facts, limit)
}

/// What the tables offer for calibration, after the guard (pure apart from the guard calls).
pub fn inputs(a: &Acpi, bi: &bootinfo::BootInfo<'_>) -> time::CalibInputs {
    let tsc = bi.tsc().unwrap_or_default();
    let trust = hpet_trust();
    // The guard judges one register access at a time (at most 8 bytes): every register slot of the block.
    // An untrusted HPET is not handed on at all: `time` then neither maps nor starts nor reads it.
    let hpet_phys = a
        .hpet
        .filter(|_| !trust.untrusted())
        .and_then(|h| h.mmio_base())
        .filter(|&p| p.is_multiple_of(HPET_SIZE as u64) && (0..HPET_SIZE as u64).step_by(8).all(|o| live::check_mem(p + o, 8).is_ok()));
    let pm_timer = a
        .fadt
        .as_ref()
        .and_then(|f| f.pm_timer_port())
        .filter(|&(port, _)| port >= PM_PORT_MIN && live::check_io(port as u64, 4, false).is_ok())
        .map(|(port, bits)| PmTimer { port, bits });
    time::CalibInputs {
        hpet_phys,
        pm_timer,
        hint_hz: tsc.hint_hz,
        reload: (tsc.calibrated_hz != 0).then_some((tsc.calibrated_hz, tsc.source)),
        hpet_trust: trust,
    }
}

/// The FADT's century index if usable (0: none).
pub fn century_reg(a: &Acpi) -> u8 {
    a.fadt.as_ref().map(|f| f.century).filter(|c| (CENTURY_MIN..=CENTURY_MAX).contains(c)).unwrap_or(0)
}

pub(crate) fn boot(a: &Acpi, bi: &bootinfo::BootInfo<'_>) {
    let i = inputs(a, bi);
    let _ = time::calibrate(&i);
    let no_rtc = a.fadt.as_ref().is_some_and(|f| f.no_cmos_rtc());
    if let Err(e) = time::wall::read_rtc(century_reg(a), no_rtc) {
        klog::event!(Warn, "time", "rtc.unreadable", error = e.name(); "no wall clock until NTP or clock_settime");
    }
}
