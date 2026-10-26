//! What the MTRRs say about physical memory (pure, host-tested; the registers come from `hal::pat`).
//!
//! The one question the page tables ask: is this aligned power-of-two block (a 4 KiB, 2 MiB or 1 GiB page)
//! of one memory type? A large page across two MTRR types is undefined (SDM 11.11.9), so the direct map and
//! vmap split to smaller pages wherever `block_type` says `None`. Rules as the SDM and Linux
//! (`arch/x86/kernel/cpu/mtrr/generic.c`): MTRRs off -> UC everywhere; below 1 MiB the fixed ranges win when
//! enabled; overlapping variable ranges combine as UC > WT-over-WB > anything else conflicting = UC; no
//! match -> the default type; on AMD with TOM2 enabled, [4 GiB, TOM2) counts as one more WB range.

use alloc::vec::Vec;
use hal::pat::{MtrrRegs, MT_UC, MT_WB, MT_WT};

const MIB: u64 = 1 << 20;
const GIB4: u64 = 1 << 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mtrrs {
    enabled: bool,
    fixed_enabled: bool,
    default: u8,
    /// (start, end, type) of the 88 fixed sub-ranges below 1 MiB.
    fixed: Vec<(u64, u64, u8)>,
    /// (base, mask, type) of the valid variable ranges; mask cut to bits 12..phys_bits.
    var: Vec<(u64, u64, u8)>,
    tom2: Option<u64>,
}

/// Linux's `get_effective_type`: how two overlapping ranges' types combine.
pub fn combine(a: u8, b: u8) -> u8 {
    if a == b {
        a
    } else if a == MT_UC || b == MT_UC {
        MT_UC
    } else if (a == MT_WT && b == MT_WB) || (a == MT_WB && b == MT_WT) {
        MT_WT
    } else {
        MT_UC
    }
}

impl Mtrrs {
    /// A CPU without MTRRs: one type everywhere (WB, so PAT alone decides).
    pub fn none() -> Mtrrs {
        Mtrrs { enabled: true, fixed_enabled: false, default: MT_WB, fixed: Vec::new(), var: Vec::new(), tom2: None }
    }

    pub fn from_regs(r: &MtrrRegs) -> Mtrrs {
        let enabled = r.def_type & (1 << 11) != 0;
        let fixed_enabled = enabled && r.def_type & (1 << 10) != 0 && r.cap & (1 << 8) != 0;
        let bits = r.phys_bits.clamp(36, 52) as u32;
        let addr_mask = ((1u64 << bits) - 1) & !0xFFF;
        let mut fixed = Vec::new();
        // 0x250: 8 x 64 KiB from 0; 0x258, 0x259: 8 x 16 KiB from 0x80000; 0x268..: 8 x 4 KiB from 0xC0000.
        let mut at = 0u64;
        for (i, reg) in r.fixed.iter().enumerate() {
            let step = match i {
                0 => 64 << 10,
                1 | 2 => 16 << 10,
                _ => 4 << 10,
            };
            for k in 0..8 {
                fixed.push((at, at + step, ((reg >> (8 * k)) & 0xFF) as u8));
                at += step;
            }
        }
        let var = r
            .var
            .iter()
            .filter(|(_, mask)| mask & (1 << 11) != 0)
            .map(|&(base, mask)| (base & addr_mask, mask & addr_mask, (base & 0xFF) as u8))
            .collect();
        let tom2 = r.tom2.filter(|&t| t > GIB4);
        Mtrrs { enabled, fixed_enabled, default: (r.def_type & 0xFF) as u8, fixed, var, tom2 }
    }

    /// The memory type of the aligned block [start, start + size) (size a power of two, at least 4 KiB), or
    /// `None` when the block holds more than one type.
    pub fn block_type(&self, start: u64, size: u64) -> Option<u8> {
        debug_assert!(size.is_power_of_two() && size >= 4096 && start.is_multiple_of(size));
        if !self.enabled {
            return Some(MT_UC);
        }
        let end = start.checked_add(size)?;
        if self.fixed_enabled && start < MIB {
            if end > MIB {
                return None;
            }
            let mut ty = None;
            for &(_, _, t) in self.fixed.iter().filter(|f| f.0 < end && start < f.1) {
                match ty {
                    Some(x) if x != t => return None,
                    _ => ty = Some(t),
                }
            }
            return ty;
        }
        let low = size - 1;
        let mut ty: Option<u8> = None;
        for &(base, mask, t) in &self.var {
            if (start & mask & !low) != (base & mask & !low) {
                continue; // no address of the block matches
            }
            if mask & low != 0 {
                return None; // some addresses of the block match, others do not
            }
            ty = Some(ty.map_or(t, |x| combine(x, t)));
        }
        if let Some(top) = self.tom2
            && start < top
            && end > GIB4
        {
            if start < GIB4 || end > top {
                return None;
            }
            ty = Some(ty.map_or(MT_WB, |x| combine(x, MT_WB)));
        }
        Some(ty.unwrap_or(self.default))
    }

    /// The type of the 4 KiB page holding `addr`.
    pub fn type_at(&self, addr: u64) -> u8 {
        self.block_type(addr & !0xFFF, 4096).unwrap_or(MT_UC)
    }

    pub fn variable_ranges(&self) -> usize {
        self.var.len()
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn default_type(&self) -> u8 {
        self.default
    }
}

/// Short name of a memory type for events.
pub fn type_name(t: u8) -> &'static str {
    match t {
        0 => "UC",
        1 => "WC",
        4 => "WT",
        5 => "WP",
        6 => "WB",
        7 => "UC-",
        _ => "?",
    }
}

#[cfg(test)]
#[path = "mtrr_tests.rs"]
mod tests;
