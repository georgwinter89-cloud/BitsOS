//! Page attribute table and MTRRs (owner: MM). Linux's PAT layout on every CPU before WC mappings are used
//! (V2-PLAN §2.3): entry 0 WB, 1 WC, 2 UC-, 3 UC, 4 WB, 5 WP, 6 UC-, 7 WT. bitsboot maps the GOP framebuffer
//! with PWT=1 (entry 1), which becomes write-combining here.
//!
//! `read_mtrrs` takes a raw snapshot of the memory-type range registers; `mm::mtrr` interprets it (pure,
//! host-tested), so the direct map never puts a large page across two memory types (SDM 11.11.9).

use alloc::vec::Vec;

pub const LINUX_PAT: u64 = 0x0407_0506_0007_0106;

/// Memory type encodings (PAT entries and MTRRs share them).
pub const MT_UC: u8 = 0;
pub const MT_WC: u8 = 1;
pub const MT_WT: u8 = 4;
pub const MT_WP: u8 = 5;
pub const MT_WB: u8 = 6;
pub const MT_UC_MINUS: u8 = 7;

/// The memory type PAT entry `i` (0-7) holds in the PAT value `pat`.
pub const fn pat_entry(pat: u64, i: usize) -> u8 {
    ((pat >> (8 * i)) & 0x7) as u8
}

/// The PAT MSR of this CPU.
pub fn read_pat() -> u64 {
    crate::msr::read(crate::msr::IA32_PAT)
}

/// Programs the PAT on this CPU with the SDM procedure (SDM 11.12.4 with 11.11.8: caches off and flushed,
/// the whole TLB flushed including global entries, before and after the write).
pub fn init_this_cpu() {
    #[cfg(target_os = "none")]
    {
        let _irq = crate::cpu::IrqGuard::new();
        let cr0 = crate::cpu::read_cr0();
        // SAFETY: the SDM's PAT change sequence, step 1: no-fill cache mode, write back and invalidate the
        // caches. Interrupts are off throughout; only caching changes.
        unsafe {
            core::arch::asm!(
                "mov cr0, {cd}",
                "wbinvd",
                cd = in(reg) (cr0 | crate::cpu::CR0_CD) & !crate::cpu::CR0_NW,
                options(nostack)
            );
        }
        // A CR3 reload keeps global entries (CR4.PGE, set by CP-1): a global mapping cached with the old PAT
        // would keep its old type (PWT=1 was WT before the write, WC after).
        crate::paging::flush_all();
        crate::msr::write(crate::msr::IA32_PAT, LINUX_PAT);
        // SAFETY: as above, the closing steps: write back and invalidate again (the flush follows below).
        unsafe { core::arch::asm!("wbinvd", options(nostack)) };
        crate::paging::flush_all();
        // SAFETY: caches back on with the CR0 value read above.
        unsafe { core::arch::asm!("mov cr0, {}", in(reg) cr0, options(nostack)) };
    }
}

pub const IA32_MTRR_PHYSBASE0: u32 = 0x200;
pub const IA32_MTRR_FIX64K_00000: u32 = 0x250;
pub const IA32_MTRR_FIX16K_80000: u32 = 0x258;
pub const IA32_MTRR_FIX16K_A0000: u32 = 0x259;
pub const IA32_MTRR_FIX4K_C0000: u32 = 0x268;
/// AMD: TOM2, the top of DRAM above 4 GiB (SYSCFG.MtrrTom2En makes [4 GiB, TOM2) write-back).
pub const AMD_TOP_MEM2: u32 = 0xC001_001D;
pub const SYSCFG_MTRR_VAR_DRAM_EN: u64 = 1 << 20;
pub const SYSCFG_MTRR_TOM2_EN: u64 = 1 << 21;

/// The fixed-range MSRs in address order: 0x250 (8 x 64 KiB from 0), 0x258/0x259 (8 x 16 KiB each from
/// 0x80000), 0x268..=0x26F (8 x 4 KiB each from 0xC0000).
pub const FIXED_MSRS: [u32; 11] = [
    IA32_MTRR_FIX64K_00000,
    IA32_MTRR_FIX16K_80000,
    IA32_MTRR_FIX16K_A0000,
    IA32_MTRR_FIX4K_C0000,
    IA32_MTRR_FIX4K_C0000 + 1,
    IA32_MTRR_FIX4K_C0000 + 2,
    IA32_MTRR_FIX4K_C0000 + 3,
    IA32_MTRR_FIX4K_C0000 + 4,
    IA32_MTRR_FIX4K_C0000 + 5,
    IA32_MTRR_FIX4K_C0000 + 6,
    IA32_MTRR_FIX4K_C0000 + 7,
];

/// A raw MTRR snapshot of this CPU.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MtrrRegs {
    /// IA32_MTRRCAP: VCNT in bits 0-7, FIX bit 8, WC bit 10.
    pub cap: u64,
    /// IA32_MTRR_DEF_TYPE: type in bits 0-7, FE bit 10, E bit 11.
    pub def_type: u64,
    /// The fixed-range registers in `FIXED_MSRS` order (zero when the CPU has none).
    pub fixed: [u64; 11],
    /// (PHYSBASEn, PHYSMASKn) for every variable range the CPU has (VCNT pairs).
    pub var: Vec<(u64, u64)>,
    /// Physical address width (CPUID 0x80000008 EAX[7:0]; 36 when the leaf is missing, as Linux).
    pub phys_bits: u8,
    /// AMD: TOM2 when SYSCFG has MtrrVarDramEn and MtrrTom2En (Linux `mtrr_tom2`).
    pub tom2: Option<u64>,
}

/// Reads this CPU's MTRRs; `None` when CPUID says it has none.
pub fn read_mtrrs() -> Option<MtrrRegs> {
    let v = crate::cpu::live();
    if !v.has("mtrr") {
        return None;
    }
    use crate::msr::read;
    let cap = read(crate::msr::IA32_MTRRCAP);
    let mut r = MtrrRegs { cap, def_type: read(crate::msr::IA32_MTRR_DEF_TYPE), ..Default::default() };
    if cap & (1 << 8) != 0 {
        for (i, m) in FIXED_MSRS.iter().enumerate() {
            r.fixed[i] = read(*m);
        }
    }
    for i in 0..(cap & 0xFF) as u32 {
        r.var.push((read(IA32_MTRR_PHYSBASE0 + 2 * i), read(IA32_MTRR_PHYSBASE0 + 2 * i + 1)));
    }
    r.phys_bits = if v.max_ext() >= 0x8000_0008 { (v.get(0x8000_0008, 0).eax & 0xFF) as u8 } else { 36 };
    if crate::cpu::detect().amd {
        let syscfg = read(crate::msr::AMD_SYSCFG);
        let both = SYSCFG_MTRR_VAR_DRAM_EN | SYSCFG_MTRR_TOM2_EN;
        if syscfg & both == both {
            r.tom2 = Some(read(AMD_TOP_MEM2) & 0x000F_FFFF_FF80_0000);
        }
    }
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_layout() {
        let want = [MT_WB, MT_WC, MT_UC_MINUS, MT_UC, MT_WB, MT_WP, MT_UC_MINUS, MT_WT];
        for (i, t) in want.iter().enumerate() {
            assert_eq!(pat_entry(LINUX_PAT, i), *t, "entry {i}");
        }
    }
}
