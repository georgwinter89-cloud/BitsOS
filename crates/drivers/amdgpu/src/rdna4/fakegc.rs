//! Wave 43, item HB: the model's graphics block as `gfx_v12_0_hw_init` meets it after the firmware is loaded, and
//! its memory hub (`fakegchub`, with the page faults of both hubs). Like `fakenb` it answers as the real card
//! answered Linux (the read values of the trace's sections gc-boot and gc-hub) and refuses, and keeps, what Linux
//! would not do:
//!
//! - any write to a graphics register before the block has power (the power controller's features are on,
//!   `Model::gc_powered` in `fakechain`) and its autoload is complete; until then the block answers nothing useful (every read is 0, as the one
//!   early read of the trace was: sw-init.txt line 316);
//! - a program counter start written under another selection of GRBM_GFX_CNTL than Linux's (PFP and ME: pipe 0
//!   and 1 of ME 0; MEC: pipe 0 to 3 of ME 1), a selection Linux never makes, and ANY other graphics register
//!   written while a pipe other than 0 is still selected (a stage that leaves the selection behind shows at its
//!   next write, or at its end: `FakeGc::grbm`);
//! - a write of CP_ME_CNTL or CP_MEC_RS64_CNTL that changes anything but the pipes' reset bits, or pulses an
//!   engine's reset before its starts were written on every pipe (a write that clears or sets a HALT bit is the
//!   command processor's unhalt or halt, judged by `fakecp` before this model sees it);
//! - DB_MEM_CONFIG or CP_DEBUG written with anything but Linux's one change of what it holds.

use super::fakemodel::Model;
use super::nbregs::NbRegs;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::vec::Vec;
use rdna4_core::gc12;
use rdna4_core::hub41::{self, Kind};
use rdna4_core::nbreg::member;
use rdna4_core::regmap::Reg;

/// What the bootload status answered Linux once the autoload was complete (init.txt line 9152): the six low status
/// bits with BOOTLOAD_COMPLETE.
pub const BOOTLOAD_DONE: u32 = 0x8000_003F;

/// The three RS64 engines: (start, its high word, the selections Linux writes them under).
const ENGINES: [(Reg, Reg, core::ops::RangeInclusive<u32>); 3] = [
    (gc12::CP_PFP_PRGRM_CNTR_START, gc12::CP_PFP_PRGRM_CNTR_START_HI, 0..=1),
    (gc12::CP_ME_PRGRM_CNTR_START, gc12::CP_ME_PRGRM_CNTR_START_HI, 0..=1),
    (gc12::CP_MEC_RS64_PRGRM_CNTR_START, gc12::CP_MEC_RS64_PRGRM_CNTR_START_HI, 4..=7),
];

pub struct FakeGc {
    /// Tests: this many reads of the bootload status still answer "not complete" (`u32::MAX`: never complete).
    pub not_yet: u32,
    /// Every graphics register the model knows, by BAR5 byte, with what it reads once the block is awake.
    pub(super) vals: BTreeMap<u32, u32>,
    /// GRBM_GFX_CNTL's selection as the last write left it.
    pub grbm: u32,
    /// Program counter starts as written: (selection, BAR5 byte) to value.
    pub starts: BTreeMap<(u32, u32), u32>,
    /// Reset pulses completed per engine (PFP, ME, MEC), and whether one is raised now.
    pub pulses: [u32; 3],
    pub(super) raised: [bool; 3],
    // The hub (`fakegchub`).
    pub(super) ctx0: u8,
    pub context0: bool,
    pub(super) acked: bool,
    pub tlb_flushes: u32,
    /// Tests: the acknowledge never comes.
    pub ack_never: bool,
    /// Table entries GFXHUB's walker does not see yet (`fakegart`).
    pub(super) pending: Vec<super::fakegart::Pending>,
    /// Page faults, per hub (GFXHUB, MMHUB): the status as it stands, faults raised, status reads since the last
    /// fault was raised, clears.
    pub fault_status: [u32; 2],
    pub faults_raised: [u32; 2],
    pub fault_reads: [u32; 2],
    pub faults_cleared: [u32; 2],
}

pub(super) fn hub_index(k: Kind) -> usize {
    match k {
        Kind::Gc => 0,
        Kind::Mm => 1,
    }
}

impl FakeGc {
    pub fn new(n: &NbRegs) -> FakeGc {
        let mut g = FakeGc {
            not_yet: 0,
            vals: BTreeMap::new(),
            grbm: 0,
            starts: BTreeMap::new(),
            pulses: [0; 3],
            raised: [false; 3],
            ctx0: 0,
            context0: false,
            acked: false,
            tlb_flushes: 0,
            ack_never: false,
            pending: Vec::new(),
            fault_status: [0; 2],
            faults_raised: [0; 2],
            fault_reads: [0; 2],
            faults_cleared: [0; 2],
        };
        g.cold(n);
        g
    }

    /// The registers as the autoload leaves them: what the card answered Linux's first reads (init.txt lines
    /// 9153-9357), 0 where Linux only wrote.
    fn cold(&mut self, n: &NbRegs) {
        let h = &hub41::GC;
        let vals = &mut self.vals;
        vals.clear();
        let mut put = |r: Reg, v: u32| {
            if let Some(b) = n.byte(r) {
                vals.insert(b, v);
            }
        };
        hub41::gart_enable_regs(h, |r| put(r, 0));
        for r in gc12::RS64_REGS {
            put(r, 0);
        }
        for (r, v) in [
            (gc12::DB_MEM_CONFIG, 0x0000_8035),
            (gc12::GB_ADDR_CONFIG, 0x0820_0545),
            (gc12::CP_ME_CNTL, 0x1514_A000),
            (gc12::CP_MEC_RS64_CNTL, 0x4003_0000),
            (gc12::CP_DEBUG, 0x2040_0000),
            (h.fault_cntl2, 0x0002_0000),
            (h.l1_tlb, 0x0000_0501),
            (h.l2_cntl, 0x0008_0602),
            (h.fault_cntl, 0x3FFF_FFFC),
            (h.fault_status, 0),
            (member(h.eng0_req, hub41::GART_ENGINE, 1), 0),
            (member(h.eng0_ack, hub41::GART_ENGINE, 1), 0),
        ] {
            put(r, v);
        }
        for i in 0..=hub41::USER_CONTEXTS {
            put(member(h.context0_cntl, i, 1), 0x03FF_FC00);
        }
        (self.grbm, self.ctx0, self.context0, self.acked, self.raised) = (0, 0, false, false, [false; 3]);
        self.starts.clear();
    }

    /// Every pipe of an engine has both words of its start.
    pub(super) fn started(&self, n: &NbRegs, engine: usize) -> bool {
        let (start, hi, sels) = &ENGINES[engine];
        sels.clone().all(|sel| [start, hi].into_iter().all(|r| n.byte(*r).is_some_and(|b| self.starts.contains_key(&(sel, b)))))
    }
}

fn name(byte: u32) -> &'static str {
    super::linuxseq::name_of(byte / 4).unwrap_or("a graphics register")
}

impl Model {
    /// Power and a complete autoload: the block answers and takes writes.
    pub fn gc_awake(&self) -> bool {
        self.gc_powered() && gc12::autoload_complete(self.cp_stat, self.bootload)
    }

    /// The secure OS took AUTOLOAD_RLC: the block's firmware starts it anew.
    pub fn gc_autoloaded(&mut self) {
        (self.cp_stat, self.bootload) = (0, BOOTLOAD_DONE);
        let n = self.nb.n.clone();
        self.gc.cold(&n);
        self.mes_cold();
        self.cp.cold();
        self.rings.cold();
    }

    /// A read of a graphics register (or of MMHUB's fault status), or `None` for any other byte.
    pub(super) fn gc_read(&mut self, byte: u32) -> Option<u32> {
        // Wave 44: MES's registers are `fakemes`'s.
        if let Some(v) = self.mes_read(byte) {
            return Some(v);
        }
        let n = &self.nb.n;
        let is = |r: Reg| n.byte(r) == Some(byte);
        if let Some(i) = [hub41::GC.fault_status, hub41::MM.fault_status].into_iter().position(is) {
            if i == 0 && !self.gc_awake() {
                return Some(0);
            }
            self.gc.fault_reads[i] += 1;
            return Some(self.gc.fault_status[i]);
        }
        let (cp, boot) = (is(gc12::CP_STAT), is(gc12::RLC_RLCS_BOOTLOAD_STATUS));
        if !cp && !boot && !self.gc.vals.contains_key(&byte) {
            return None;
        }
        // No power: nothing useful, the two registers of Linux's wait included.
        if !self.gc_powered() {
            return Some(0);
        }
        if cp {
            return Some(self.cp_stat);
        }
        if boot {
            if self.gc.not_yet > 0 {
                self.gc.not_yet -= u32::from(self.gc.not_yet != u32::MAX);
                return Some(0);
            }
            return Some(self.bootload);
        }
        if !self.gc_awake() {
            return Some(0);
        }
        if is(member(hub41::GC.eng0_ack, hub41::GART_ENGINE, 1)) {
            return Some(self.gchub_ack());
        }
        self.gc.vals.get(&byte).copied()
    }

    /// A write to a graphics register (or MMHUB's fault clear): `Some(reached)`, or `None` for any other byte.
    pub(super) fn gc_write(&mut self, byte: u32, v: u32) -> Option<bool> {
        let n = self.nb.n.clone();
        let is = |r: Reg| n.byte(r) == Some(byte);
        if is(hub41::MM.fault_cntl) && hub41::f::CLEAR_FAULT_STATUS.get(v) != 0 {
            return Some(self.fault_cleared(Kind::Mm, byte, v));
        }
        // Wave 44: MES's registers and its selections of GRBM_GFX_CNTL are `fakemes`'s.
        if let Some(reached) = self.mes_write(byte, v) {
            return Some(reached);
        }
        let cur = *self.gc.vals.get(&byte)?;
        if !self.gc_awake() {
            return Some(self.refuse(format!("{} written {v:#x} before the graphics block has power and its autoload is complete: Linux's first write there follows the power controller's features and the autoload wait (gfx_v12_0.c:3797-3808)", name(byte))));
        }
        if is(gc12::GRBM_GFX_CNTL) {
            if !ENGINES.iter().any(|(_, _, sels)| sels.contains(&v)) {
                return Some(self.refuse(format!("GRBM_GFX_CNTL written {v:#x}: Linux selects pipe 0 and 1 of ME 0 and pipe 0 to 3 of ME 1 here, queue 0, VMID 0 (gfx_v12_0.c:2156-2206)")));
            }
            self.gc.grbm = v;
            self.gc.vals.insert(byte, v);
            return Some(true);
        }
        if let Some((_, _, sels)) = ENGINES.iter().find(|(start, hi, _)| is(*start) || is(*hi)) {
            if !sels.contains(&self.gc.grbm) {
                return Some(self.refuse(format!("{} written while GRBM_GFX_CNTL selects {:#x}: Linux writes it under {:#x} to {:#x} only", name(byte), self.gc.grbm, sels.start(), sels.end())));
            }
            self.gc.starts.insert((self.gc.grbm, byte), v);
            self.gc.vals.insert(byte, v);
            return Some(true);
        }
        if self.gc.grbm != 0 {
            return Some(self.refuse(format!("{} written while GRBM_GFX_CNTL still selects {:#x}: Linux selects pipe 0 again first (gfx_v12_0.c:2163)", name(byte), self.gc.grbm)));
        }
        let reached = if is(gc12::CP_ME_CNTL) {
            self.gc_cntl(&n, byte, v, cur, &[(gc12::PFP_PIPE_RESETS, 0), (gc12::ME_PIPE_RESETS, 1)], gc12::GFX_HALTS)
        } else if is(gc12::CP_MEC_RS64_CNTL) {
            self.gc_cntl(&n, byte, v, cur, &[(gc12::MEC_PIPE_RESETS, 2)], gc12::f::MEC_HALT.mask)
        } else if is(gc12::DB_MEM_CONFIG) && v != gc12::golden_db_mem_config(cur) {
            self.refuse(format!("DB_MEM_CONFIG written {v:#x} over {cur:#x}: Linux's golden setting sets bit 15 and nothing else"))
        } else if is(gc12::CP_DEBUG) && v != gc12::cp_debug(cur) {
            self.refuse(format!("CP_DEBUG written {v:#x} over {cur:#x}: Linux sets CPG_UTCL1_ERROR_HALT_DISABLE and nothing else"))
        } else if is(gc12::GB_ADDR_CONFIG) {
            self.refuse(format!("GB_ADDR_CONFIG written {v:#x}: Linux only reads it here"))
        } else {
            return Some(self.gchub_write(&n, byte, v));
        };
        if reached && !is(gc12::CP_ME_CNTL) && !is(gc12::CP_MEC_RS64_CNTL) {
            self.gc.vals.insert(byte, v);
        }
        Some(reached)
    }

    /// CP_ME_CNTL or CP_MEC_RS64_CNTL: only the pipes' reset bits move, an engine's only once its starts are
    /// written, and no HALT bit falls. The register reads back what was written (init.txt line 9173).
    fn gc_cntl(&mut self, n: &NbRegs, byte: u32, v: u32, cur: u32, resets: &[(u32, usize)], halts: u32) -> bool {
        if cur & halts & !v != 0 {
            return self.refuse(format!("{} written {v:#x} over {cur:#x}: a HALT bit is cleared; this wave only sets where the engines will start (their unhalt is gfx_v12_0_cp_resume's)", name(byte)));
        }
        let all = resets.iter().fold(0, |m, (mask, _)| m | mask);
        if (v ^ cur) & !all != 0 {
            return self.refuse(format!("{} written {v:#x} over {cur:#x}: Linux changes only the pipes' reset bits here", name(byte)));
        }
        for &(mask, engine) in resets {
            if v & mask & !cur != 0 && !self.gc.started(n, engine) {
                return self.refuse(format!("{} written {v:#x}: a reset pulse of an engine whose program counter start is not written on every pipe", name(byte)));
            }
            // Linux's pulse: every reset bit of the engine set, then every one cleared.
            if v & mask == mask && cur & mask != mask {
                self.gc.raised[engine] = true;
            } else if v & mask == 0 && self.gc.raised[engine] {
                (self.gc.raised[engine], self.gc.pulses[engine]) = (false, self.gc.pulses[engine] + 1);
            }
        }
        self.gc.vals.insert(byte, v);
        true
    }
}
