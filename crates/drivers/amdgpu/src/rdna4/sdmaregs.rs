//! Wave 43 (piece 12): SDMA 7.0.1's registers as BAR5 bytes, placed by the card's OWN discovery table (the numbers
//! are `rdna4_core::sdma7`'s). Linux reaches them on GC's bases (`sdma_v7_0_get_reg_offset`, sdma_v7_0.c:125-141),
//! so the table's GC entry places them, not its SDMA entries; NBIF's doorbell entry for the engines and the two
//! registers of the in-ring HDP flush lie on NBIF's base 2.

use crate::gpucore::disc::Discovery;
use crate::gpucore::psp::Hw;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::regmap::{BAR5_BYTES, Reg};
use rdna4_core::sdma7::{self, INSTANCES};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SdmaRegs {
    /// GC's bases 0 and 1 and NBIF's base 2, as dwords.
    gc: [u32; 2],
    nbif2: u32,
}

impl SdmaRegs {
    /// `None` when the table lacks a base or a register of either engine would lie past BAR5.
    pub fn from(d: &Discovery) -> Option<SdmaRegs> {
        let base = |r: Reg| d.reg(r.hw_id, 0, usize::from(r.base_index), 0);
        let s = SdmaRegs { gc: [base(sdma7::RB_CNTL)?, base(sdma7::MCU_CNTL)?], nbif2: base(sdma7::DOORBELL_ENTRY_2)? };
        let all = (0..INSTANCES).all(|i| sdma7::ALL.iter().all(|r| s.byte(*r, i).is_some()));
        (all && s.nbif(sdma7::DOORBELL_ENTRY_2).is_some() && s.hdp_flush_dwords().is_some()).then_some(s)
    }

    fn at(base: u32, offset: u32) -> Option<u32> {
        let dw = base.checked_add(offset)?;
        (dw < BAR5_BYTES / 4).then_some(dw * 4)
    }

    /// The BAR5 byte of a register of engine `i`.
    pub fn byte(&self, r: Reg, i: u32) -> Option<u32> {
        let r = sdma7::instance(r, i);
        Self::at(*self.gc.get(usize::from(r.base_index))?, r.offset)
    }

    /// The BAR5 byte of one of NBIF's registers this block uses.
    pub fn nbif(&self, r: Reg) -> Option<u32> {
        Self::at(self.nbif2, r.offset)
    }

    /// The in-ring HDP flush's (done, request) registers as dwords (`get_hdp_flush_done_offset`,
    /// `get_hdp_flush_req_offset`, nbif_v6_3_1.c:433-447): what the packet names, shifted left by 2.
    pub fn hdp_flush_dwords(&self) -> Option<(u32, u32)> {
        Some((self.nbif(sdma7::GPU_HDP_FLUSH_DONE)? / 4, self.nbif(sdma7::GPU_HDP_FLUSH_REQ)? / 4))
    }

    /// The engine and register at a BAR5 byte, if it is one of `sdma7::ALL` of an engine.
    pub fn reg_at(&self, byte: u32) -> Option<(u32, Reg)> {
        (0..INSTANCES).find_map(|i| sdma7::ALL.iter().find(|r| self.byte(**r, i) == Some(byte)).map(|r| (i, *r)))
    }

    /// The bytes of `regs` for engine `i` (a guard list); a register without a place is left out, and `from`
    /// made sure there is none.
    pub fn bytes(&self, regs: &[Reg], i: u32) -> Vec<u32> {
        regs.iter().filter_map(|r| self.byte(*r, i)).collect()
    }
}

/// Both engines' registers as they read now, for the stage `state`: Linux's dump list as far as it tells a
/// halted or faulting engine from a running one (`sdma_reg_list_7_0`, sdma_v7_0.c:62-118). Reads only, and only
/// asked for once `sdma` wrote the block's registers in this boot (`stage::run`).
pub fn state_line(sr: &SdmaRegs, hw: &mut dyn Hw) -> String {
    let mut parts = Vec::new();
    for i in 0..INSTANCES {
        let mut read = |r: Reg| sr.byte(r, i).and_then(|b| hw.reg_read(b)).map_or(String::from("-"), |v| format!("{v:#x}"));
        parts.push(format!(
            "sdma{i} MCU_CNTL {} STATUS_REG {} RB_CNTL {} RPTR {} WPTR {} RPTR_FETCH {} IB_CNTL {} UTCL1 RD/WR status {}/{} INT_STATUS {} CNTL {}",
            read(sdma7::MCU_CNTL),
            read(sdma7::STATUS_REG),
            read(sdma7::RB_CNTL),
            read(sdma7::RB_RPTR),
            read(sdma7::RB_WPTR),
            read(sdma7::RB_RPTR_FETCH),
            read(sdma7::IB_CNTL),
            read(sdma7::UTCL1_RD_STATUS),
            read(sdma7::UTCL1_WR_STATUS),
            read(sdma7::INT_STATUS),
            read(sdma7::CNTL),
        ));
    }
    format!("sdma registers: {}", parts.join("; "))
}
