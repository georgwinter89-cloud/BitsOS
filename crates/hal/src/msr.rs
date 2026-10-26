//! Model-specific registers (owner: CP). Reading an MSR the CPU does not implement raises #GP, and `read` has
//! no fault recovery, so the API is limited to named ones the kernel needs; MCP's `msr_read` goes through
//! `read_checked`, which allows only MSRs that CPUID (or a bank's STATUS) says exist.

pub const IA32_APIC_BASE: u32 = 0x1B;
/// TSC adjust (exists with CPUID 7.0 EBX bit 1, `tsc_adjust`); firmware may leave it non-zero per core.
pub const IA32_TSC_ADJUST: u32 = 0x3B;
pub const IA32_MTRRCAP: u32 = 0xFE;
pub const IA32_MCG_CAP: u32 = 0x179;
pub const IA32_MCG_STATUS: u32 = 0x17A;
pub const IA32_PAT: u32 = 0x277;
/// AMD RAPL energy unit and the core/package energy counters (CPUID `rapl`; CP-5's idle-power check reads them
/// through MCP `msr_read`).
pub const AMD_RAPL_POWER_UNIT: u32 = 0xC001_0299;
pub const AMD_CORE_ENERGY_STATUS: u32 = 0xC001_029A;
pub const AMD_PKG_ENERGY_STATUS: u32 = 0xC001_029B;
/// Intel RAPL energy unit, package and core (PP0) energy counters (linux-6.6/arch/x86/include/asm/msr-index.h:391,
/// 394, 404). Intel has no CPUID bit for them; `intel_rapl_model` decides (CP-6). PSys (0x64D) is left out: it
/// is missing on some desktop parts and `read` cannot recover from the #GP.
pub const INTEL_RAPL_POWER_UNIT: u32 = 0x606;
pub const INTEL_PKG_ENERGY_STATUS: u32 = 0x611;
pub const INTEL_PP0_ENERGY_STATUS: u32 = 0x639;
/// HWP enable (bit 0; exists with CPUID 6 EAX bit 7, `hwp`). SDM Vol. 3B 15.4.2: the other HWP MSRs may only be
/// accessed once this bit is set, else #GP; firmware may leave it clear and V2 never sets it (Linux's intel_pstate
/// does, linux-6.6/drivers/cpufreq/intel_pstate.c:1731, before it reads the capabilities at :920).
pub const IA32_PM_ENABLE: u32 = 0x770;
/// Highest/guaranteed/efficient/lowest performance in HWP units (CPUID `hwp` and IA32_PM_ENABLE bit 0; SDM Vol.
/// 3B 15.4.3). `smp` reads byte 0 (highest) on hybrid Intel CPUs for the capacity hint (CP-6).
pub const IA32_HWP_CAPABILITIES: u32 = 0x771;
/// TSC-deadline timer target (exists with CPUID 1 ECX bit 24, `tsc_deadline_timer`; CP-5's `apic::timer`).
pub const IA32_TSC_DEADLINE: u32 = 0x6E0;
/// CET enables for user and supervisor mode (exist with CPUID SHSTK or IBT).
pub const IA32_U_CET: u32 = 0x6A0;
pub const IA32_S_CET: u32 = 0x6A2;
pub const IA32_MTRR_DEF_TYPE: u32 = 0x2FF;
pub const IA32_MC0_CTL: u32 = 0x400;
pub const IA32_XSS: u32 = 0xDA0;
pub const IA32_EFER: u32 = 0xC000_0080;
pub const IA32_STAR: u32 = 0xC000_0081;
pub const IA32_LSTAR: u32 = 0xC000_0082;
pub const IA32_CSTAR: u32 = 0xC000_0083;
pub const IA32_FMASK: u32 = 0xC000_0084;
pub const IA32_FS_BASE: u32 = 0xC000_0100;
pub const IA32_GS_BASE: u32 = 0xC000_0101;
pub const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;
pub const IA32_TSC_AUX: u32 = 0xC000_0103;
pub const AMD_SYSCFG: u32 = 0xC001_0010;
/// AMD top of memory below 4 GiB and above 4 GiB (with SYSCFG.MtrrTom2En).
pub const AMD_TOM: u32 = 0xC001_001A;
pub const AMD_TOM2: u32 = 0xC001_001D;
/// AMD Scalable MCA: bank i's registers at 0xC000_2000 + 0x10 * i (IPID at +5).
pub const AMD_SMCA_BASE: u32 = 0xC000_2000;

pub const EFER_SCE: u64 = 1 << 0;
pub const EFER_LME: u64 = 1 << 8;
pub const EFER_LMA: u64 = 1 << 10;
pub const EFER_NXE: u64 = 1 << 11;

/// Machine-check bank registers. Legacy layout (Intel, and AMD without SMCA): bank i at 0x400 + 4 * i, but
/// only for i < 32: 0x480 starts the VMX capability MSRs, so banks from 32 on have no legacy address. AMD
/// Scalable MCA (CPUID 0x8000_0007 EBX bit 3): bank i at 0xC000_2000 + 0x10 * i for every bank MCG_CAP reports.
pub const LEGACY_MCA_BANKS: u32 = 32;
/// Offsets of a bank's registers in both layouts (SMCA adds IPID at 5).
pub const MC_CTL: u32 = 0;
pub const MC_STATUS: u32 = 1;
pub const MC_ADDR: u32 = 2;
pub const MC_MISC: u32 = 3;
pub const SMCA_IPID: u32 = 5;

/// MSR number of register `reg` (`MC_CTL`..) of bank `bank`, `None` where the layout has no address for it.
pub const fn mc_reg(smca: bool, bank: u32, reg: u32) -> Option<u32> {
    if smca {
        if bank > 0xFF || reg > SMCA_IPID {
            return None;
        }
        Some(AMD_SMCA_BASE + 0x10 * bank + reg)
    } else {
        if bank >= LEGACY_MCA_BANKS || reg > MC_MISC {
            return None;
        }
        Some(IA32_MC0_CTL + 4 * bank + reg)
    }
}
pub const fn mc_ctl(bank: u32) -> u32 {
    IA32_MC0_CTL + 4 * bank
}
pub const fn mc_status(bank: u32) -> u32 {
    IA32_MC0_CTL + 4 * bank + 1
}
pub const fn mc_addr(bank: u32) -> u32 {
    IA32_MC0_CTL + 4 * bank + 2
}
pub const fn mc_misc(bank: u32) -> u32 {
    IA32_MC0_CTL + 4 * bank + 3
}
/// AMD SMCA only (CPUID 0x8000_0007 EBX bit 3).
pub const fn smca_ipid(bank: u32) -> u32 {
    AMD_SMCA_BASE + 0x10 * bank + 5
}

/// MSRs that exist on every x86-64 CPU this kernel accepts (exposed to MCP `msr_read`). The rest of the read
/// list depends on CPUID (`ReadCaps`).
const READABLE: &[u32] = &[IA32_APIC_BASE, IA32_PAT, IA32_EFER, IA32_STAR, IA32_LSTAR, IA32_FMASK];

/// What decides which further MSRs exist on this CPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadCaps {
    /// CPUID MTRR: MTRRCAP and MTRR_DEF_TYPE.
    pub mtrr: bool,
    /// CPUID RDTSCP or RDPID: IA32_TSC_AUX.
    pub tsc_aux: bool,
    /// CPUID MCE and MCA: MCG_CAP, MCG_STATUS and the bank registers.
    pub mca: bool,
    /// MCG_CAP.Count.
    pub mca_banks: u32,
    /// AMD SMCA: bank registers at their SMCA addresses.
    pub smca: bool,
    /// AMD RAPL (CPUID 0x8000_0007 EDX bit 14): the energy MSRs.
    pub rapl: bool,
    /// TSC-deadline timer: IA32_TSC_DEADLINE.
    pub tsc_deadline: bool,
    /// Intel RAPL by model (`intel_rapl_model`): the Intel energy MSRs.
    pub intel_rapl: bool,
    /// CPUID `hwp` on Intel (`intel_hwp`): IA32_PM_ENABLE.
    pub hwp: bool,
    /// IA32_PM_ENABLE bit 0 (HWP switched on): IA32_HWP_CAPABILITIES.
    pub hwp_enabled: bool,
}

/// Intel client models whose RAPL MSRs (0x606, 0x611, 0x639) exist: the hybrid generations from Linux's model
/// list (linux-6.6/arch/x86/events/rapl.c:805-814: Alder, Raptor, Meteor Lake) plus Arrow Lake (0xC5/0xC6,
/// linux-6.6/arch/x86/include/asm/intel-family.h:128-129; the Gaming-PC's 265K is 0xC6 and its Linux shows
/// `intel-rapl:0`). Intel has no CPUID bit for RAPL and `read` has no fault recovery, so unknown models and every
/// guest (CPUID `hypervisor`: KVM passes the host model through but #GPs on these MSRs) get no.
const INTEL_RAPL_MODELS: &[u32] = &[0x97, 0x9A, 0xAA, 0xAC, 0xB7, 0xBA, 0xBF, 0xC5, 0xC6];

/// Whether this CPU is one of `INTEL_RAPL_MODELS` on bare metal (pure; host-tested).
pub fn intel_rapl_model(v: &crate::cpu::CpuidView) -> bool {
    let eax = v.get(1, 0).eax;
    let family = (eax >> 8) & 0xF;
    let model = ((eax >> 4) & 0xF) | ((eax >> 12) & 0xF0);
    is_intel(v) && family == 6 && !v.has("hypervisor") && INTEL_RAPL_MODELS.contains(&model)
}

/// Whether HWP is switched on (IA32_PM_ENABLE bit 0), read with `read` only when CPUID says `hwp` (pure;
/// host-tested). The HWP MSRs other than PM_ENABLE #GP while it is clear (SDM Vol. 3B 15.4.2).
pub fn hwp_enabled(v: &crate::cpu::CpuidView, read: &mut dyn FnMut(u32) -> u64) -> bool {
    intel_hwp(v) && read(IA32_PM_ENABLE) & 1 != 0
}

/// CPUID `hwp` on a GenuineIntel CPU: the HWP MSRs are Intel-only, and a set leaf-6 bit 7 on another vendor
/// (reserved there) must not open them.
pub fn intel_hwp(v: &crate::cpu::CpuidView) -> bool {
    is_intel(v) && v.has("hwp")
}

fn is_intel(v: &crate::cpu::CpuidView) -> bool {
    let vendor = v.get(0, 0);
    vendor.ebx == u32::from_le_bytes(*b"Genu") && vendor.ecx == u32::from_le_bytes(*b"ntel")
}

impl ReadCaps {
    pub fn live() -> Self {
        let v = crate::cpu::live();
        let mca = v.has("mca") && v.has("mce");
        ReadCaps {
            mtrr: v.has("mtrr"),
            tsc_aux: v.has("rdtscp") || v.has("rdpid"),
            mca,
            mca_banks: if mca { (read(IA32_MCG_CAP) & 0xFF) as u32 } else { 0 },
            smca: mca && v.has("smca"),
            rapl: v.has("rapl"),
            tsc_deadline: v.has("tsc_deadline_timer"),
            intel_rapl: intel_rapl_model(&v),
            hwp: intel_hwp(&v),
            hwp_enabled: hwp_enabled(&v, &mut |m| read(m)),
        }
    }
}

/// Whether reading `msr` is safe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    No,
    Yes,
    /// MCi_ADDR and MCi_MISC may be unimplemented (rdmsr then #GPs, SDM Vol. 3B 16.3.2.3/4): readable only
    /// while the bank's STATUS (`status`) has `valid_bit` (ADDRV or MISCV) set.
    IfStatus { status: u32, valid_bit: u64 },
}

/// Whether `msr` may be read on a CPU with `caps` (pure; host-tested).
pub fn access(msr: u32, caps: &ReadCaps) -> Access {
    let yes = |b: bool| if b { Access::Yes } else { Access::No };
    if READABLE.contains(&msr) {
        return Access::Yes;
    }
    match msr {
        IA32_MTRRCAP | IA32_MTRR_DEF_TYPE => return yes(caps.mtrr),
        IA32_TSC_AUX => return yes(caps.tsc_aux),
        IA32_MCG_CAP | IA32_MCG_STATUS => return yes(caps.mca),
        AMD_RAPL_POWER_UNIT | AMD_CORE_ENERGY_STATUS | AMD_PKG_ENERGY_STATUS => return yes(caps.rapl),
        IA32_TSC_DEADLINE => return yes(caps.tsc_deadline),
        INTEL_RAPL_POWER_UNIT | INTEL_PKG_ENERGY_STATUS | INTEL_PP0_ENERGY_STATUS => return yes(caps.intel_rapl),
        IA32_PM_ENABLE => return yes(caps.hwp),
        IA32_HWP_CAPABILITIES => return yes(caps.hwp && caps.hwp_enabled),
        _ => {}
    }
    if !caps.mca {
        return Access::No;
    }
    let (base, stride) = if caps.smca { (AMD_SMCA_BASE, 0x10) } else { (IA32_MC0_CTL, 4) };
    let Some(off) = msr.checked_sub(base) else { return Access::No };
    let (bank, reg) = (off / stride, off % stride);
    let Some(status) = mc_reg(caps.smca, bank, MC_STATUS) else { return Access::No };
    if bank >= caps.mca_banks {
        return Access::No;
    }
    match reg {
        MC_CTL | MC_STATUS => Access::Yes,
        MC_ADDR => Access::IfStatus { status, valid_bit: crate::cpu::MCI_STATUS_ADDRV },
        MC_MISC => Access::IfStatus { status, valid_bit: crate::cpu::MCI_STATUS_MISCV },
        SMCA_IPID if caps.smca => Access::Yes,
        _ => Access::No,
    }
}

#[cfg(target_os = "none")]
pub fn read(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: callers pass architectural MSRs that exist on every CPU this kernel accepts (CP checks CPUID
    // before using optional ones); rdmsr has no memory effects.
    unsafe { core::arch::asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) };
    (hi as u64) << 32 | lo as u64
}

#[cfg(target_os = "none")]
pub fn write(msr: u32, value: u64) {
    // SAFETY: only hal-internal code and the CP/SC/AB owners call this with architectural MSRs and values
    // they validated (the trusted base reviews every caller).
    unsafe {
        core::arch::asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32,
            options(nostack, preserves_flags))
    };
}

#[cfg(not(target_os = "none"))]
pub fn read(_msr: u32) -> u64 {
    0
}
#[cfg(not(target_os = "none"))]
pub fn write(_msr: u32, _value: u64) {}

/// For MCP `msr_read` (input from the network): refuses MSRs that might not exist instead of faulting.
pub fn read_checked(msr: u32) -> Option<u64> {
    match access(msr, &ReadCaps::live()) {
        Access::No => None,
        Access::Yes => Some(read(msr)),
        Access::IfStatus { status, valid_bit } => (read(status) & valid_bit != 0).then(|| read(msr)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bank_registers() {
        assert_eq!((mc_ctl(0), mc_status(0), mc_addr(0), mc_misc(0)), (0x400, 0x401, 0x402, 0x403));
        assert_eq!(mc_status(5), 0x415);
        assert_eq!(smca_ipid(2), 0xC000_2025);
    }

    #[test]
    fn readable_list() {
        let none = ReadCaps::default();
        let intel = ReadCaps { mtrr: true, tsc_aux: true, mca: true, mca_banks: 10, smca: false, ..Default::default() };
        assert_eq!(access(IA32_PAT, &none), Access::Yes);
        assert_eq!(access(IA32_MCG_CAP, &none), Access::No, "no MCA, no MCG_CAP");
        assert_eq!(access(IA32_TSC_AUX, &none), Access::No, "TSC_AUX needs RDTSCP or RDPID");
        assert_eq!(access(IA32_MTRRCAP, &none), Access::No, "MTRRCAP needs CPUID MTRR");
        assert_eq!(access(IA32_TSC_AUX, &intel), Access::Yes);
        assert_eq!(access(IA32_MCG_CAP, &intel), Access::Yes);
        assert_eq!(access(mc_ctl(9), &intel), Access::Yes);
        assert_eq!(access(mc_status(9), &intel), Access::Yes);
        assert_eq!(access(mc_addr(9), &intel), Access::IfStatus { status: mc_status(9), valid_bit: crate::cpu::MCI_STATUS_ADDRV });
        assert_eq!(access(mc_misc(9), &intel), Access::IfStatus { status: mc_status(9), valid_bit: crate::cpu::MCI_STATUS_MISCV });
        assert_eq!(access(mc_ctl(10), &intel), Access::No, "bank 10 does not exist with 10 banks");
        assert_eq!(access(0x10, &intel), Access::No, "TSC is not on the list");
        assert_eq!(access(AMD_SYSCFG, &intel), Access::No);
        assert_eq!(access(AMD_PKG_ENERGY_STATUS, &intel), Access::No, "RAPL energy needs CPUID rapl");
        assert_eq!(access(IA32_TSC_DEADLINE, &intel), Access::No);
        let zen4 = ReadCaps { rapl: true, ..Default::default() };
        for m in [AMD_RAPL_POWER_UNIT, AMD_CORE_ENERGY_STATUS, AMD_PKG_ENERGY_STATUS] {
            assert_eq!(access(m, &zen4), Access::Yes);
        }
        assert_eq!(access(IA32_TSC_DEADLINE, &ReadCaps { tsc_deadline: true, ..Default::default() }), Access::Yes);
        let hwp_msrs = [IA32_PM_ENABLE, IA32_HWP_CAPABILITIES];
        for m in [INTEL_RAPL_POWER_UNIT, INTEL_PKG_ENERGY_STATUS, INTEL_PP0_ENERGY_STATUS].into_iter().chain(hwp_msrs) {
            assert_eq!(access(m, &zen4), Access::No, "{m:#x}: Intel-only");
            assert_eq!(access(m, &intel), Access::No, "{m:#x}: needs the model or CPUID hwp");
        }
        // CPUID hwp but HWP left off by the firmware: PM_ENABLE yes, the capabilities #GP.
        let hwp_off = ReadCaps { hwp: true, ..intel };
        assert_eq!(access(IA32_PM_ENABLE, &hwp_off), Access::Yes);
        assert_eq!(access(IA32_HWP_CAPABILITIES, &hwp_off), Access::No, "HWP MSRs #GP until PM_ENABLE bit 0");
        assert_eq!(access(IA32_HWP_CAPABILITIES, &ReadCaps { hwp_enabled: true, ..intel }), Access::No);
        let arl = ReadCaps { intel_rapl: true, hwp: true, hwp_enabled: true, ..intel };
        for m in [INTEL_RAPL_POWER_UNIT, INTEL_PKG_ENERGY_STATUS, INTEL_PP0_ENERGY_STATUS].into_iter().chain(hwp_msrs) {
            assert_eq!(access(m, &arl), Access::Yes);
        }
        assert_eq!(access(0x64D, &arl), Access::No, "PSys stays off the list");
        assert_eq!(access(AMD_SYSCFG, &arl), Access::No);
    }

    #[test]
    fn many_banks_stop_at_the_legacy_range() {
        let big = ReadCaps { mtrr: true, tsc_aux: true, mca: true, mca_banks: 64, smca: false, ..Default::default() };
        assert_eq!(access(0x47D, &big), Access::Yes, "bank 31 status");
        assert_eq!(access(0x480, &big), Access::No, "IA32_VMX_BASIC is not bank 32");
        assert_eq!(access(0x48F, &big), Access::No);
        assert_eq!(mc_reg(false, 32, MC_STATUS), None);
        assert_eq!(mc_reg(false, 3, SMCA_IPID), None, "no IPID in the legacy layout");
    }

    #[test]
    fn smca_layout() {
        let amd = ReadCaps { mtrr: true, tsc_aux: true, mca: true, mca_banks: 40, smca: true, ..Default::default() };
        assert_eq!(mc_reg(true, 2, MC_STATUS), Some(0xC000_2021));
        assert_eq!(mc_reg(true, 2, SMCA_IPID), Some(smca_ipid(2)));
        assert_eq!(access(0xC000_2021 + 0x10 * 37, &amd), Access::Yes, "bank 38 of 40");
        assert_eq!(access(smca_ipid(39), &amd), Access::Yes);
        assert_eq!(access(AMD_SMCA_BASE + 0x10 * 40, &amd), Access::No);
        assert_eq!(access(AMD_SMCA_BASE + 0x10 * 3 + 2, &amd), Access::IfStatus { status: AMD_SMCA_BASE + 0x31, valid_bit: crate::cpu::MCI_STATUS_ADDRV });
        assert_eq!(access(AMD_SMCA_BASE + 4, &amd), Access::No, "MCA_CONFIG is not on the list");
        assert_eq!(access(mc_status(1), &amd), Access::No, "SMCA CPUs are read at their SMCA addresses");
    }
}
