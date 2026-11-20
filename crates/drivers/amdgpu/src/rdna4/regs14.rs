//! GP-GPU2: the BAR5 bytes of every register the G2 stages read or write, placed by the card's OWN discovery table
//! (never by a number in code), and the reverse look-up the write guard needs. The register numbers are
//! `rdna4_core::regmap`'s (Linux 7.3.1 headers, file:line there; `rdna4-regs`' test holds them equal to the
//! generated crate).

use crate::gpucore::disc::Discovery;
use crate::ipmod::psp13;
use alloc::vec::Vec;
use rdna4_core::refuse::NEVER_WRITTEN;
use rdna4_core::regmap::{self as rm, Reg};
use rdna4_core::thm14;

/// Every register a G2 stage names, by BAR5 byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Regs14 {
    /// The KM ring's registers: psp_v14_0's bare-metal ring code is psp_v13_0_4's line for line (psp_v14_0.c:239-371
    /// against psp_v13_0_4.c:194-310), on MP0 base index 0 instead of 1, so the 780M's `Mailbox13` drives it.
    pub psp: psp13::Addrs,
    pub c2pmsg_33: u32,
    /// The bootloader's buffer address (psp_v14_0.c:147-148); with C2PMSG_35 open only through the guard's gate.
    pub c2pmsg_36: u32,
    pub smu_message: u32,
    pub smu_argument: u32,
    pub smu_answer: u32,
    pub rsmu_index: u32,
    pub rsmu_data: u32,
    pub fb_base: u32,
    pub cp_stat: u32,
    pub bootload: u32,
    /// Where a stage flushes the HDP: Linux's remapped place (`rm::HDP_FLUSH_REMAPPED_BYTE`), a fixed byte of BAR5.
    pub hdp_flush: u32,
    /// The flush register itself, where a flush goes until the stage `nbif` has written the remap (no stage of
    /// Linux's chain flushes that early; `nb::State::hdp_flush` chooses).
    pub hdp_direct: u32,
    /// The register that must hold `hdp_flush` for a flush there to do anything (NBIF's remap, soc24's hw_init).
    pub hdp_remap: u32,
    pub memsize: u32,
    /// The thermal alert's four registers (`rdna4_core::thm14`), written at the end of the power controller's start.
    pub thm_int_ctrl: u32,
    pub thm_int_ena: u32,
    pub mp1_ih_sw_int: u32,
    pub mp1_ih_sw_int_ctrl: u32,
    /// (byte, register) of everything above plus the refused registers, for `refuse::write_refused`.
    pub named: Vec<(u32, Reg)>,
}

fn byte(d: &Discovery, r: Reg) -> Option<u32> {
    let dw = d.reg(r.hw_id, 0, usize::from(r.base_index), r.offset)?;
    (dw < rm::BAR5_BYTES / 4).then_some(dw * 4)
}

impl Regs14 {
    /// `None` when the table lacks an IP or base, or a register lies past BAR5.
    pub fn from(d: &Discovery) -> Option<Regs14> {
        let b = |r| byte(d, r);
        let psp = psp13::Addrs {
            c2pmsg_35: b(rm::PSP_C2PMSG_35)?,
            c2pmsg_64: b(rm::PSP_C2PMSG_64)?,
            c2pmsg_67: b(rm::PSP_C2PMSG_67)?,
            c2pmsg_69: b(rm::PSP_C2PMSG_69)?,
            c2pmsg_70: b(rm::PSP_C2PMSG_70)?,
            c2pmsg_71: b(rm::PSP_C2PMSG_71)?,
            c2pmsg_81: b(rm::PSP_C2PMSG_81)?,
        };
        let known = [
            rm::PSP_C2PMSG_33,
            rm::PSP_C2PMSG_35,
            rm::PSP_C2PMSG_36,
            rm::PSP_C2PMSG_64,
            rm::PSP_C2PMSG_67,
            rm::PSP_C2PMSG_69,
            rm::PSP_C2PMSG_70,
            rm::PSP_C2PMSG_71,
            rm::PSP_C2PMSG_81,
            rm::PSP_C2PMSG_101,
            rm::PSP_C2PMSG_102,
            rm::PSP_C2PMSG_103,
            rm::SMU_MESSAGE,
            rm::SMU_ARGUMENT,
            rm::SMU_ANSWER,
            rm::SMU_DEBUG_ARGUMENT,
            rm::SMU_DEBUG_ANSWER,
            rm::SMU_DEBUG_MESSAGE,
            rm::RSMU_INDEX,
            rm::RSMU_DATA,
            rm::MMHUB_FB_LOCATION_BASE,
            rm::GC_CP_STAT,
            rm::GC_RLC_BOOTLOAD_STATUS,
            rm::HDP_MEM_FLUSH,
            rm::REMAP_HDP_MEM_FLUSH,
            rm::MEMSIZE,
            thm14::THERMAL_INT_CTRL,
            thm14::THERMAL_INT_ENA,
            thm14::MP1_IH_SW_INT,
            thm14::MP1_IH_SW_INT_CTRL,
        ];
        let mut named = Vec::new();
        for r in known.into_iter().chain(NEVER_WRITTEN.iter().map(|(r, _)| *r)) {
            named.push((b(r)?, r));
        }
        // No place of the table: Linux's fixed byte (nbif_v6_3_1.c:613-617).
        named.push((rm::HDP_FLUSH_REMAPPED_BYTE, rm::HDP_FLUSH_REMAPPED));
        Some(Regs14 {
            psp,
            c2pmsg_33: b(rm::PSP_C2PMSG_33)?,
            c2pmsg_36: b(rm::PSP_C2PMSG_36)?,
            smu_message: b(rm::SMU_MESSAGE)?,
            smu_argument: b(rm::SMU_ARGUMENT)?,
            smu_answer: b(rm::SMU_ANSWER)?,
            rsmu_index: b(rm::RSMU_INDEX)?,
            rsmu_data: b(rm::RSMU_DATA)?,
            fb_base: b(rm::MMHUB_FB_LOCATION_BASE)?,
            cp_stat: b(rm::GC_CP_STAT)?,
            bootload: b(rm::GC_RLC_BOOTLOAD_STATUS)?,
            hdp_flush: rm::HDP_FLUSH_REMAPPED_BYTE,
            hdp_direct: b(rm::HDP_MEM_FLUSH)?,
            hdp_remap: b(rm::REMAP_HDP_MEM_FLUSH)?,
            memsize: b(rm::MEMSIZE)?,
            thm_int_ctrl: b(thm14::THERMAL_INT_CTRL)?,
            thm_int_ena: b(thm14::THERMAL_INT_ENA)?,
            mp1_ih_sw_int: b(thm14::MP1_IH_SW_INT)?,
            mp1_ih_sw_int_ctrl: b(thm14::MP1_IH_SW_INT_CTRL)?,
            named,
        })
    }

    /// The register at a byte, if it is one G2 knows.
    pub fn reg_at(&self, byte: u32) -> Option<Reg> {
        self.named.iter().find(|(b, _)| *b == byte).map(|(_, r)| *r)
    }

    pub fn name(&self, byte: u32) -> &'static str {
        self.reg_at(byte).map_or("?", |r| r.name)
    }
}
