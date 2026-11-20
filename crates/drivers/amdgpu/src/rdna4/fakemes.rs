//! Wave 44, item ME: the model's MES 12, as Linux's `mes_v12_0_hw_init` meets it after the firmware load. It runs
//! only as Linux starts it and refuses, and keeps, what Linux would not do:
//!
//! - any register of MES before the GC block has power and its autoload is complete (`Model::gc_awake`);
//! - a pipe started without MES's four images loaded by the secure processor before the autoload, without its
//!   program counter written under its own selection, or without its reset bit set first; CP_MES_CNTL written
//!   with anything but Linux's steps (a pipe's reset, its start, the halt of a Reload); the KIQ pipe's queue
//!   before the 500 us after both pipes started (mes_v12_0.c:1353-1356);
//! - the KIQ pipe's HQD registers under another selection, out of Linux's order, with other values than Linux's,
//!   or made active on an MQD that is not bound through GFXHUB or does not hold what the registers say;
//! - a doorbell of a pipe whose queue is not on, through a closed aperture, outside the pipes' two (0x16, 0x18),
//!   or with a value that is not the write pointer's slot;
//! - the unmapped and aggregated doorbells before the pipe Linux programs them after took its resources, or with
//!   other values than Linux's.
//!
//! Each doorbell runs the pipe's ring from where it stood to the new write pointer, 64 dwords a frame, every
//! frame read through GFXHUB's walker (`Model::gc_walk`): a frame on a page the walker does not find is a fault
//! of GFXHUB (client CPC, an assumption: plan/spec/gpu-rdna4.md section 7) and the pipe stops there. What each
//! command does and refuses is `fakemesapi`. The card's answers are the trace's: CP_MES_CNTL 0x40000000 cold,
//! CP_MQD_CONTROL 0x100, CP_UNMAPPED_DOORBELL 0x0c30, CP_MES_GP3_LO 0x01029097 on a running pipe.

use super::fakemodel::Model;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::mes12::{self, Pipe, f};
use rdna4_core::mes12api as api;
use rdna4_core::regmap::Reg;

/// CP_MES_CNTL and the HQD registers as the cold card read for Linux (init.txt 9503, 9516-9522).
pub const CNTL_COLD: u32 = 0x4000_0000;
pub const MQD_CONTROL_COLD: u32 = 0x100;
pub const UNMAPPED_COLD: u32 = 0x0C30;
/// The secure processor's image types of MES's one file, both pipes (`fwlist::Id::CpMes..CpMes1Data`).
pub const IMAGES: [u32; 4] = [33, 34, 81, 82];

/// A pipe's queue as the model runs it: its ring, the slots it reads, the doorbell that starts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Q {
    pub ring_mc: u64,
    pub ring_dw: u64,
    pub wptr_mc: u64,
    pub rptr_mc: u64,
    pub doorbell: u32,
    pub mqd_mc: u64,
    /// Dwords the pipe has read.
    pub rptr: u64,
}

/// A queue MES mapped (`ADD_QUEUE` with `map_legacy_kq`), as the command processor's model takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapped {
    pub queue_type: u32,
    pub pipe: u32,
    pub queue: u32,
    pub doorbell: u32,
    pub mqd: u64,
    pub wptr: u64,
}

/// What SET_HW_RESOURCES told a pipe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HwSeen {
    pub compute_hqd: [u32; 8],
    pub gfx_hqd: [u32; 2],
}

pub struct FakeMes {
    pub cntl: u32,
    /// Image types of MES the secure processor loaded since the block was last cold, and whether the autoload
    /// found all four (the pipes have their program).
    pub loaded: Vec<u32>,
    pub fw: bool,
    /// Per pipe: the program counter's start as written under its selection (low, high), its reset seen.
    pub pc: [[Option<u32>; 2]; 2],
    pub(super) reset_seen: [bool; 2],
    pub running: [bool; 2],
    /// The card's clock when the second pipe started: the KIQ pipe's queue is taken only 500 us later.
    pub(super) started_us: Option<u64>,
    /// What CP_MES_GP3_LO reads on a running pipe (tests may change it).
    pub gp3: u32,
    /// The KIQ pipe's HQD registers by byte, and the order they were written in since its doorbell went off.
    pub hqd: BTreeMap<u32, u32>,
    pub(super) hqd_order: Vec<u32>,
    pub queue: [Option<Q>; 2],
    pub hw: [Option<HwSeen>; 2],
    pub hw1: [u32; 2],
    pub unmapped: u32,
    pub agg: [u32; 5],
    pub gfx_control: u32,
    pub mapped: Vec<Mapped>,
    /// Every frame as received: (pipe, its 64 dwords).
    pub frames: Vec<(usize, Vec<u32>)>,
    /// What the misc commands wrote through the model: (register dword, value).
    pub misc_writes: Vec<(u32, u32)>,
    pub change_config: u32,
    /// The measured table's register bases SET_HW_RESOURCES must name (GC, MMHUB, OSSSYS).
    pub bases: [[u32; 5]; 3],
    /// Tests: a pipe that never answers; the frame with this number (in `frames`) answered with status 0; the
    /// trailing fence written wrong.
    pub dead: [bool; 2],
    pub status_zero: Option<usize>,
    pub wrong_fence: bool,
}

impl FakeMes {
    pub fn new(d: &crate::gpucore::disc::Discovery) -> FakeMes {
        use crate::gpucore::hwid;
        let seg = |hw: u16| core::array::from_fn(|i| d.reg(hw, 0, i, 0).unwrap_or(0));
        FakeMes { bases: [seg(hwid::GC), seg(hwid::MMHUB), seg(hwid::OSSSYS)], ..FakeMes::new_cold() }
    }

    /// The commands received per opcode on a pipe (tests).
    pub fn count(&self, pipe: Pipe, opcode: u32) -> usize {
        self.frames.iter().filter(|(p, d)| *p == pipe.index() && api::header_fields(d[0]).1 == opcode).count()
    }

    /// MES runs a pipe's queue now.
    pub fn runs(&self, p: Pipe) -> bool {
        self.running[p.index()] && self.queue[p.index()].is_some()
    }
}

impl Model {
    /// GRBM_GFX_CNTL's selections MES's start makes: the two MES pipes (mes_v12_0.c:1309, :1599, :1722; a pipe
    /// without its firmware is refused at its start, `mes_cntl`), the four compute queues once the scheduler pipe
    /// runs (their MQDs, gfx_v12_0.c:3480). The gfx queue's selection is 0, the graphics block's own.
    fn mes_selection(&self, v: u32) -> bool {
        let pipes = Pipe::BOTH.iter().any(|p| mes12::mes_select(*p) == v);
        let queues = self.mes.runs(Pipe::Sched) && mes12::KERNEL_QUEUES.iter().any(|q| !q.gfx && mes12::select(q.me, q.pipe, q.queue) == v);
        pipes || queues
    }

    pub(super) fn mes_byte(&self, r: Reg) -> Option<u32> {
        self.nb.n.byte(r)
    }

    pub(super) fn mes_is(&self, byte: u32, r: Reg) -> bool {
        self.mes_byte(r) == Some(byte)
    }

    /// The MES pipe GRBM_GFX_CNTL selects now.
    pub(super) fn mes_selected(&self) -> Option<Pipe> {
        Pipe::BOTH.into_iter().find(|p| self.gc.grbm == mes12::mes_select(*p))
    }

    /// The secure processor took a LOAD_IP_FW of this type (`fakecmd`).
    pub(super) fn mes_image_loaded(&mut self, ty: u32) {
        if IMAGES.contains(&ty) && !self.mes.loaded.contains(&ty) {
            self.mes.loaded.push(ty);
        }
    }

    /// The autoload: the block starts cold, and MES has its program when the four images went before it.
    pub(super) fn mes_cold(&mut self) {
        let (loaded, bases, gp3) = (core::mem::take(&mut self.mes.loaded), self.mes.bases, self.mes.gp3);
        let tests = (self.mes.dead, self.mes.status_zero, self.mes.wrong_fence);
        let mut m = FakeMes { bases, gp3, ..FakeMes::new_cold() };
        m.fw = IMAGES.iter().all(|t| loaded.contains(t));
        (m.dead, m.status_zero, m.wrong_fence) = tests;
        *self.mes = m;
    }

    /// A read of a register of MES, or `None` for any other byte.
    pub(super) fn mes_read(&mut self, byte: u32) -> Option<u32> {
        let known = [mes12::CP_MES_CNTL, mes12::CP_MES_GP3_LO, mes12::CP_UNMAPPED_DOORBELL, mes12::CP_MES_PRGRM_CNTR_START, mes12::CP_MES_PRGRM_CNTR_START_HI].iter().any(|r| self.mes_is(byte, *r));
        let agg = (0..5).position(|i| self.mes_is(byte, mes12::doorbell_control(i)));
        let hqd = mes12::HQD.iter().any(|r| self.mes_is(byte, *r));
        if !known && agg.is_none() && !hqd {
            return None;
        }
        if !self.gc_awake() {
            return Some(0);
        }
        if self.mes_is(byte, mes12::CP_MES_CNTL) {
            return Some(self.mes.cntl);
        }
        if self.mes_is(byte, mes12::CP_MES_GP3_LO) {
            return Some(self.mes_selected().filter(|p| self.mes.running[p.index()]).map_or(0, |_| self.mes.gp3));
        }
        if self.mes_is(byte, mes12::CP_UNMAPPED_DOORBELL) {
            return Some(self.mes.unmapped);
        }
        if let Some(i) = agg {
            return Some(self.mes.agg[i]);
        }
        if self.mes_is(byte, mes12::CP_HQD_GFX_CONTROL) {
            return Some(self.mes.gfx_control);
        }
        let cold = if self.mes_is(byte, mes12::CP_MQD_CONTROL) { MQD_CONTROL_COLD } else { 0 };
        Some(self.mes.hqd.get(&byte).copied().unwrap_or(cold))
    }

    /// A write to a register of MES, or to GRBM_GFX_CNTL with one of MES's selections: `Some(reached)`, or `None`
    /// for any other byte.
    pub(super) fn mes_write(&mut self, byte: u32, v: u32) -> Option<bool> {
        let grbm = self.mes_is(byte, mes12::GRBM_GFX_CNTL) && self.mes_selection(v);
        let pc = [mes12::CP_MES_PRGRM_CNTR_START, mes12::CP_MES_PRGRM_CNTR_START_HI].iter().position(|r| self.mes_is(byte, *r));
        let agg = (0..5).position(|i| self.mes_is(byte, mes12::doorbell_control(i)));
        let hqd = mes12::HQD.iter().any(|r| self.mes_is(byte, *r));
        let own = grbm || pc.is_some() || agg.is_some() || hqd || self.mes_is(byte, mes12::CP_MES_CNTL) || self.mes_is(byte, mes12::CP_UNMAPPED_DOORBELL) || self.mes_is(byte, mes12::CP_MES_GP3_LO);
        if !own {
            return None;
        }
        let name = super::linuxseq::name_of(byte / 4).unwrap_or("a register of MES");
        if !self.gc_awake() {
            return Some(self.refuse(format!("{name} written {v:#x} before the graphics block has power and its autoload is complete")));
        }
        if grbm {
            self.gc.grbm = v;
            self.gc.vals.insert(byte, v);
            return Some(true);
        }
        if let Some(half) = pc {
            let Some(p) = self.mes_selected() else { return Some(self.refuse(format!("{name} written while GRBM_GFX_CNTL selects {:#x}, not a MES pipe (mes_v12_0.c:1309)", self.gc.grbm))) };
            self.mes.pc[p.index()][half] = Some(v);
            return Some(true);
        }
        if self.mes_is(byte, mes12::CP_MES_CNTL) {
            return Some(self.mes_cntl(v));
        }
        if self.mes_is(byte, mes12::CP_MES_GP3_LO) {
            return Some(self.refuse(String::from("CP_MES_GP3_LO written: Linux only reads it")));
        }
        if self.mes_is(byte, mes12::CP_UNMAPPED_DOORBELL) {
            if self.mes.hw[Pipe::Kiq.index()].is_none() || v != mes12::unmapped_doorbell(self.mes.unmapped) || self.gc.grbm != 0 {
                return Some(self.refuse(format!("CP_UNMAPPED_DOORBELL written {v:#x} over {:#x}: Linux sets PROC_LSB 0xd and ENABLE under selection 0, after the KIQ pipe took its resources (mes_v12_0.c:1076-1094, :2049)", self.mes.unmapped)));
            }
            self.mes.unmapped = v;
            return Some(true);
        }
        if let Some(i) = agg {
            if self.mes.hw[Pipe::Sched.index()].is_none() || v != mes12::aggregated(self.mes.agg[i], mes12::AGGREGATED_DOORBELLS[i]) || self.gc.grbm != 0 {
                return Some(self.refuse(format!("{name} written {v:#x}: Linux writes level {i}'s aggregated doorbell {:#x} with its enable, after the scheduler pipe took its resources (mes_v12_0.c:1021-1068)", mes12::AGGREGATED_DOORBELLS[i])));
            }
            self.mes.agg[i] = v;
            return Some(true);
        }
        Some(self.mes_hqd(byte, v, name))
    }

    /// CP_MES_CNTL: a pipe's reset under its selection, its start (HALT and the resets clear, the active bits of
    /// it and every pipe before it), or the halt of `mes_v12_0_enable(adev, false)`.
    fn mes_cntl(&mut self, v: u32) -> bool {
        let cur = self.mes.cntl;
        if v == mes12::halted(cur) {
            let (fw, bases, gp3) = (self.mes.fw, self.mes.bases, self.mes.gp3);
            *self.mes = FakeMes { fw, bases, gp3, cntl: v, ..FakeMes::new_cold() };
            return true;
        }
        let Some(p) = self.mes_selected() else { return self.refuse(format!("CP_MES_CNTL written {v:#x} while GRBM_GFX_CNTL selects {:#x}, not a MES pipe", self.gc.grbm)) };
        let i = p.index();
        let reset = if p == Pipe::Sched { f::MES_PIPE0_RESET } else { f::MES_PIPE1_RESET };
        if v == reset.put(cur, 1) {
            self.mes.reset_seen[i] = true;
            self.mes.cntl = v;
            return true;
        }
        if v != mes12::active(p) {
            return self.refuse(format!("CP_MES_CNTL written {v:#x} over {cur:#x} under {}: not one of Linux's steps (mes_v12_0.c:1327-1345)", p.name()));
        }
        if !self.mes.reset_seen[i] || self.mes.pc[i].iter().any(Option::is_none) {
            return self.refuse(format!("{} started before its reset bit was set and its program counter written under its selection", p.name()));
        }
        if !self.mes.fw {
            return self.refuse(format!("{} started without MES's four images loaded by the secure processor before the autoload", p.name()));
        }
        if !self.cp_ready_for_mes() {
            return self.refuse(format!("{} started before the command processor's resume: Linux's cp_resume unhalts MEC, PFP and ME and sets the KIQ's place before amdgpu_mes_kiq_hw_init (gfx_v12_0.c:3543-3550)", p.name()));
        }
        if p == Pipe::Kiq && !self.mes.running[0] {
            return self.refuse(String::from("the KIQ pipe started before the scheduler pipe (Linux's loop starts pipe 0 first)"));
        }
        self.mes.running[i] = true;
        self.mes.cntl = v;
        if p == Pipe::Kiq {
            self.mes.started_us = Some(self.card.now_us);
        }
        true
    }

    /// The pipes settled after their start (`udelay(500)` with uni_mes, mes_v12_0.c:1353-1356): nothing of the KIQ
    /// pipe's queue is taken earlier.
    pub(super) fn mes_settled(&self) -> bool {
        self.mes.started_us.is_some_and(|t| self.card.now_us.saturating_sub(t) >= mes12::ENABLE_SETTLE_US)
    }

    /// A 64-bit doorbell write: one of the MES pipes' two, or `None` for any other dword.
    pub(super) fn mes_doorbell(&mut self, index_dw: u32, v: u64) -> Option<bool> {
        let p = Pipe::BOTH.into_iter().find(|p| p.doorbell() == index_dw)?;
        self.deepest = self.deepest.min(super::fakemodel::stack_at());
        if !self.gc_awake() || self.nb.val(rdna4_core::nbreg::DOORBELL_APER_EN) & 1 == 0 {
            return Some(self.refuse(format!("MES doorbell {index_dw:#x} rung before the GC block has power or with the doorbell aperture closed")));
        }
        let Some(q) = self.mes.queue[p.index()] else { return Some(self.refuse(format!("MES doorbell {index_dw:#x} rung with the {} queue not on", p.name()))) };
        if !self.mes.running[p.index()] {
            return Some(self.refuse(format!("MES doorbell {index_dw:#x} rung while the {} does not run", p.name())));
        }
        let mut b = [0u8; 8];
        let held = self.gc_walk(q.wptr_mc).map(|sys| {
            self.nb.sys_peek(sys, &mut b);
            u64::from_le_bytes(b)
        });
        if held != Some(v) || v < q.rptr || (v - q.rptr) % api::FRAME_DW as u64 != 0 {
            return Some(self.refuse(format!("MES doorbell {index_dw:#x} written {v:#x} while the write pointer's slot holds {held:x?} and the pipe read to {:#x}: Linux writes the slot first, whole frames (mes_v12_0.c:51-62)", q.rptr)));
        }
        self.mes_run(p, v);
        Some(true)
    }

    /// The pipe reads its ring from where it stood to `wptr`, frame by frame.
    fn mes_run(&mut self, p: Pipe, wptr: u64) {
        let i = p.index();
        while let Some(q) = self.mes.queue[i].filter(|q| q.rptr < wptr) {
            if self.mes.dead[i] {
                return;
            }
            let at = q.ring_mc + (q.rptr % q.ring_dw) * 4;
            let Some(sys) = self.gc_walk(at) else {
                self.refused.push(format!("{} read a frame at {at:#x} that GFXHUB's walker does not find: the pipe stops", p.name()));
                self.mes_fault(at);
                self.mes.running[i] = false;
                return;
            };
            let mut b = alloc::vec![0u8; api::FRAME_BYTES as usize];
            self.nb.sys_peek(sys, &mut b);
            let frame: Vec<u32> = b.chunks(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect();
            if let Some(q) = self.mes.queue[i].as_mut() {
                q.rptr += api::FRAME_DW as u64;
            }
            self.mes.frames.push((i, frame.clone()));
            self.mes_frame(p, &frame);
        }
    }

    /// A page GFXHUB does not find, read by MES: latched in GFXHUB's status as client CPC and raised to the IH.
    pub(super) fn mes_fault(&mut self, mc: u64) {
        use rdna4_core::fault12::st;
        let status = st::CID.put(st::MAPPING_ERROR.put(0, 1), 5);
        let _ = self.raise_fault(rdna4_core::hub41::Kind::Gc, mc, status, false);
    }
}

impl FakeMes {
    /// The block as an autoload leaves it (the bases and the firmware's version are the card's, kept apart).
    fn new_cold() -> FakeMes {
        FakeMes {
            cntl: CNTL_COLD,
            loaded: Vec::new(),
            fw: false,
            pc: [[None; 2]; 2],
            reset_seen: [false; 2],
            running: [false; 2],
            started_us: None,
            gp3: mes12::GP3_OF_THIS_CARD,
            hqd: BTreeMap::new(),
            hqd_order: Vec::new(),
            queue: [None; 2],
            hw: [None, None],
            hw1: [0; 2],
            unmapped: UNMAPPED_COLD,
            agg: [0; 5],
            gfx_control: 0,
            mapped: Vec::new(),
            frames: Vec::new(),
            misc_writes: Vec::new(),
            change_config: 0,
            bases: [[0; 5]; 3],
            dead: [false; 2],
            status_zero: None,
            wrong_fence: false,
        }
    }
}
