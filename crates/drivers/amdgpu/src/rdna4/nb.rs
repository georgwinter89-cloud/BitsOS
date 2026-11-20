//! Wave 42 (pieces 6, 10 without GFXHUB, 11): the three blocks Linux starts before the secure processor, as stages
//! of `gx_bringup` in Linux's order (`amdgpu_device_ip_init`, amdgpu_device.c:2369-2480):
//!
//! | stage | Linux | trace section | writes |
//! |---|---|---|---|
//! | nbif | `soc24_common_hw_init` soc24.c:437-458 | nbif | STRAP2 and DF's NCSConfigurationRegister1 through the index pair, the HDP remap, the doorbell aperture; AtomicOp requester enable in the card's own Device Control 2 before them |
//! | gmc | `gmc_v12_0_sw_init`'s reads, `gmc_v12_0_hw_init` gmc_v12_0.c:1037-1060 | mmhub | the GART table and the default page in VRAM; MMHUB's GART start; the first TLB flush |
//! | ih | `ih_v7_0_irq_init` ih_v7_0.c:310-424 | ih | the rings' pages into the GART and their flush; both rings; NBIF's interrupt control and doorbell entry; the card's bus master (only if off) and its MSI vector |
//!
//! Each checks that the one before it PASSED and refuses a second run, before any write; a stage that stopped
//! half-way did not pass and runs again. This file holds what the three share: the session's state, each stage's
//! write list, and the register helpers in Linux's bus form.

use super::gart12::{FlushRegs, Gart};
use super::guard::{Allow, HostAsk, IndexRule};
use super::host::Card;
use super::ihring::IhUp;
use super::nbregs::NbRegs;
use super::stage::{Stage, Stop, psp_stop, stop};
use crate::gpucore::psp::{self, PspError};
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;
use rdna4_core::hub41::{self, Layout12, Op};
use rdna4_core::nbreg;
use rdna4_core::regmap::Reg;
use rdna4_core::{ih7, pte12};

/// What the gmc stage planned from its reads, before its guard exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub l: Layout12,
    pub table: Range<u64>,
    pub scratch: Range<u64>,
}

/// The three blocks of one card in this boot.
#[derive(Debug, Default)]
pub struct State {
    /// The HDP remap is written (by a `nbif` that passed or stopped after it): every flush of this boot goes
    /// through its remapped place from now on.
    pub remapped: bool,
    /// `nbif` passed, its last write (the doorbell aperture) included: what `gmc` asks for.
    pub nbif: bool,
    pub plan: Option<Plan>,
    /// `gmc` passed: the GART is on.
    pub gart: Option<Gart>,
    /// `ih` passed: both rings are on.
    pub ih: Option<IhUp>,
}

impl State {
    pub const fn new() -> State {
        State { remapped: false, nbif: false, plan: None, gart: None, ih: None }
    }

    /// Where an HDP flush goes: Linux flushes through the remapped register once the common hw_init moved it
    /// (BAR5 byte 0x7f000, every HDP line of the trace); before that the register itself is all there is.
    pub fn hdp_flush(&self, direct: u32) -> u32 {
        if self.remapped { nbreg::REMAPPED_MEM_FLUSH_BYTE } else { direct }
    }

    pub fn describe(&self) -> String {
        alloc::format!(
            "nbif {}, gart {}, ih {}",
            match (self.nbif, self.remapped) {
                (true, _) => "done (HDP flush at its remapped place)",
                (false, true) => "stopped half-way (HDP flush at its remapped place; nbif runs again)",
                (false, false) => "not run",
            },
            self.gart.as_ref().map_or(String::from("off"), |g| alloc::format!("on (table at vram {:#x}, {} buffers bound)", g.table.start, g.bound.len())),
            self.ih.as_ref().map_or(String::from("off"), |i| i.describe()),
        )
    }
}

pub fn is_nb(stage: Stage) -> bool {
    matches!(stage, Stage::Nbif | Stage::Gmc | Stage::Ih)
}

fn bytes(n: &NbRegs, regs: &[Reg]) -> Vec<u32> {
    regs.iter().filter_map(|r| n.byte(*r)).collect()
}

/// What a stage of this wave may write. The lists are Linux's writes of the stage's section and nothing else.
pub fn allow(stage: Stage, s: &State, n: &NbRegs, mc: impl Fn(&Range<u64>) -> Range<u64>) -> Allow {
    let hdp = s.hdp_flush(n.hdp_flush);
    let flush = FlushRegs::of(n, hdp).map(|f| f.written().to_vec()).unwrap_or_default();
    let (regs, mem) = match stage {
        Stage::Nbif => (alloc::vec![Some(n.index), Some(n.data), n.byte(nbreg::REMAP_HDP_MEM_FLUSH), n.byte(nbreg::REMAP_HDP_REG_FLUSH), n.byte(nbreg::DOORBELL_APER_EN)], Vec::new()),
        Stage::Gmc => {
            // Exactly the registers `hub41::gart_enable` names, then the fault control and the flush.
            let mut regs = Vec::new();
            hub41::gart_enable_regs(&hub41::MM, |r| regs.push(n.byte(r)));
            regs.push(n.byte(hub41::FAULT_CNTL));
            regs.extend(flush.iter().map(|b| Some(*b)));
            let mem = s.plan.as_ref().map(|p| alloc::vec![mc(&p.table), mc(&p.scratch)]).unwrap_or_default();
            (regs, mem)
        }
        Stage::Ih => {
            let mut regs = bytes(n, &[
                ih7::RB_CNTL, ih7::RB_RPTR, ih7::RB_WPTR, ih7::RB_BASE, ih7::RB_BASE_HI, ih7::RB_WPTR_ADDR_LO, ih7::RB_WPTR_ADDR_HI, ih7::DOORBELL_RPTR, ih7::RB_CNTL_RING1, ih7::RB_RPTR_RING1,
                ih7::RB_WPTR_RING1, ih7::RB_BASE_RING1, ih7::RB_BASE_HI_RING1, ih7::DOORBELL_RPTR_RING1, ih7::STORM_CLIENT_LIST_CNTL, ih7::INT_FLOOD_CNTL, ih7::MSI_STORM_CTRL, ih7::RING1_CLIENT_CFG_INDEX,
                ih7::RING1_CLIENT_CFG_DATA, ih7::CNTL2, nbreg::INTERRUPT_CNTL, nbreg::INTERRUPT_CNTL2, nbreg::DOORBELL_ENTRY_1,
            ]);
            regs.extend(&flush);
            let mem = s.gart.as_ref().map(|g| alloc::vec![g.table_mc()]).unwrap_or_default();
            (regs.into_iter().map(Some).collect(), mem)
        }
        _ => (Vec::new(), Vec::new()),
    };
    Allow { stage: stage.name(), regs: regs.into_iter().flatten().collect(), mem }
}

/// What of the host side a stage may change: `nbif` sets AtomicOp requester enable, `gmc` takes the fault's page,
/// `ih` the rings' pages, the PCI core's enable of the function and its MSI vector. No other stage changes
/// anything there.
pub fn host(stage: Stage) -> Vec<HostAsk> {
    match stage {
        Stage::Nbif => alloc::vec![HostAsk::Atomics],
        Stage::Gmc => alloc::vec![HostAsk::Pages],
        Stage::Ih => alloc::vec![HostAsk::Pages, HostAsk::BusMaster, HostAsk::Msi],
        _ => Vec::new(),
    }
}

/// Where a stage may aim the index pair, and what it may write there: `nbif` its two registers past BAR5, each
/// with Linux's one change; `smu` and `pmfw` the firmware flags, read only; no other stage anything.
pub fn index_rules(stage: Stage, n: &NbRegs) -> Vec<IndexRule> {
    let [flags, strap2, df] = n.index_targets();
    match stage {
        Stage::Nbif => alloc::vec![IndexRule { target: strap2, change: Some(nbreg::strap2) }, IndexRule { target: df, change: Some(nbreg::df_ncs_config1) }],
        Stage::Smu | Stage::Pmfw => alloc::vec![IndexRule { target: flags, change: None }],
        _ => Vec::new(),
    }
}

/// The card and its registers for a stage of this wave, with Linux's bus forms.
pub struct Io<'a> {
    pub card: &'a mut dyn Card,
    pub n: &'a NbRegs,
    pub flush: Option<FlushRegs>,
    /// The card is the driven one (the display card): `policy::may_send` asks.
    pub driven: bool,
}

fn gone(e: PspError) -> Stop {
    psp_stop(e)
}

impl Io<'_> {
    pub fn byte(&self, r: Reg) -> Result<u32, Stop> {
        self.n.byte(r).ok_or_else(|| stop("Bauteilliste nennt die Register nicht", alloc::format!("{} has no place in BAR5", r.name)))
    }

    pub fn read(&mut self, r: Reg) -> Result<u32, Stop> {
        let b = self.byte(r)?;
        psp::read(self.card.hw(), b).map_err(gone)
    }

    pub fn write(&mut self, r: Reg, v: u32) -> Result<(), Stop> {
        let b = self.byte(r)?;
        psp::write(self.card.hw(), b, v).map_err(gone)
    }

    /// `amdgpu_device_indirect_rreg` (amdgpu_reg_access.c:590-608): the address into the index register, the index
    /// read back, the data read.
    pub fn read_far(&mut self, target: u32) -> Result<u32, Stop> {
        let hw = self.card.hw();
        psp::write(hw, self.n.index, target).map_err(gone)?;
        psp::read(hw, self.n.index).map_err(gone)?;
        psp::read(hw, self.n.data).map_err(gone)
    }

    /// `amdgpu_device_indirect_wreg` (:758-776): the address, the index read back, the data, the data read back.
    pub fn write_far(&mut self, target: u32, v: u32) -> Result<(), Stop> {
        let hw = self.card.hw();
        psp::write(hw, self.n.index, target).map_err(gone)?;
        psp::read(hw, self.n.index).map_err(gone)?;
        psp::write(hw, self.n.data, v).map_err(gone)?;
        psp::read(hw, self.n.data).map(|_| ()).map_err(gone)
    }

    /// One operation of a sequence from `rdna4_core`.
    pub fn op(&mut self, op: Op) -> Result<(), Stop> {
        match op {
            Op::Write(r, v) => self.write(r, v),
            Op::Modify(r, f) => {
                let v = self.read(r)?;
                self.write(r, f(v))
            }
            Op::Pulse(r, bits) => {
                let v = self.read(r)?;
                self.write(r, v | bits)?;
                self.write(r, v & !bits)
            }
        }
    }

    pub fn flush(&self) -> Result<FlushRegs, Stop> {
        self.flush.ok_or_else(|| stop("Bauteilliste nennt die Register nicht", "the TLB flush's registers have no place in BAR5"))
    }
}

/// The pages of `bytes` at the system address `at`.
pub fn pages_of(at: u64, bytes: u64) -> Vec<u64> {
    (0..bytes.div_ceil(pte12::PAGE)).map(|i| at + i * pte12::PAGE).collect()
}

/// The three blocks' registers as they read now, for the stage `state` (reads only).
pub fn state_line(n: &NbRegs, hw: &mut dyn psp::Hw) -> String {
    let mut rd = |r: Reg| n.byte(r).and_then(|b| hw.reg_read(b)).map_or(String::from("-"), |v| alloc::format!("{v:#x}"));
    alloc::format!(
        "nb registers: doorbell aperture {}, MMVM_CONTEXT0_CNTL {}, IH_RB_CNTL {} RPTR {} WPTR {}, ring 1 CNTL {} RPTR {} WPTR {}",
        rd(nbreg::DOORBELL_APER_EN),
        rd(hub41::CONTEXT0_CNTL),
        rd(ih7::RB_CNTL),
        rd(ih7::RB_RPTR),
        rd(ih7::RB_WPTR),
        rd(ih7::RB_CNTL_RING1),
        rd(ih7::RB_RPTR_RING1),
        rd(ih7::RB_WPTR_RING1),
    )
}
