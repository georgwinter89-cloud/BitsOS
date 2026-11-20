//! Wave 42: the registers of NBIF, MMHUB and IH as BAR5 bytes, placed by the card's OWN discovery table like
//! `regs14`'s (the numbers are `rdna4_core::{nbreg, hub41, ih7}`'s), and the two addresses past BAR5 the index
//! pair reaches. Wave 43: the graphics block's two bases with them (GFXHUB is the second register set of the one
//! hub, and `gc12`'s registers lie on the same bases), so one look-up serves both hubs.

use super::regs14::Regs14;
use crate::gpucore::disc::Discovery;
use rdna4_core::ipfacts::hw;
use rdna4_core::regmap::{BAR5_BYTES, Reg};
use rdna4_core::{hub41, ih7, nbreg};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NbRegs {
    /// (hardware id, base index, the base as a dword) of every base a register of this wave lies on.
    bases: [(u16, u8, u32); 6],
    pub index: u32,
    pub data: u32,
    pub memsize: u32,
    pub fb_base: u32,
    /// The direct HDP flush register (before the remap).
    pub hdp_flush: u32,
    /// Byte addresses for the index pair.
    pub strap2: u32,
    pub df: u32,
}

impl NbRegs {
    /// `None` when the table lacks a base or a register would lie past BAR5.
    pub fn from(d: &Discovery, r: &Regs14) -> Option<NbRegs> {
        let base = |hw_id: u16, i: u8| d.reg(hw_id, 0, usize::from(i), 0).map(|dw| (hw_id, i, dw));
        let far = |reg: Reg| d.reg(reg.hw_id, 0, usize::from(reg.base_index), reg.offset).filter(|dw| *dw >= BAR5_BYTES / 4).and_then(|dw| dw.checked_mul(4));
        let n = NbRegs {
            bases: [base(hw::NBIF, 1)?, base(hw::NBIF, 2)?, base(hw::MMHUB, 0)?, base(hw::OSSSYS, 0)?, base(hw::GC, 0)?, base(hw::GC, 1)?],
            index: r.rsmu_index,
            data: r.rsmu_data,
            memsize: r.memsize,
            fb_base: r.fb_base,
            hdp_flush: r.hdp_direct,
            strap2: far(nbreg::STRAP2)?,
            df: far(nbreg::DF_NCS_CONFIG1)?,
        };
        // The highest register of each block this wave touches must lie inside BAR5.
        [nbreg::DOORBELL_ENTRY_1, nbreg::BIOS_SCRATCH_4, hub41::CONTEXT1_PAGE_TABLE_END_ADDR_LO32, ih7::RING1_CLIENT_CFG_DATA, hub41::GC.ctx1_end_lo, rdna4_core::gc12::CP_MEC_RS64_PRGRM_CNTR_START_HI, rdna4_core::gc12::RLC_RLCS_BOOTLOAD_STATUS].into_iter().all(|x| n.byte(x).is_some()).then_some(n)
    }

    /// The BAR5 byte of a register of this wave (a family member up to 64 dwords above its first included).
    pub fn byte(&self, r: Reg) -> Option<u32> {
        let (_, _, base) = self.bases.iter().find(|(h, i, _)| *h == r.hw_id && *i == r.base_index)?;
        let dw = base.checked_add(r.offset)?;
        (dw.checked_add(64)? < BAR5_BYTES / 4).then_some(dw * 4)
    }

    /// The addresses the index pair is ever aimed at on this card: the SMU's flags, STRAP2, DF's register. Which
    /// of them a stage may reach, and what it may write there, is `nb::index_rules`.
    pub fn index_targets(&self) -> [u32; 3] {
        [rdna4_core::regmap::SMN_MP1_FIRMWARE_FLAGS, self.strap2, self.df]
    }
}
