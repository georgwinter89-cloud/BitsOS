//! Wave 44, items CP and RG: what the model's command processor does with a queue MES mapped. MES's model hands
//! each kernel queue it maps on (`Model::cp_place_queue`, from the queue's MQD as the firmware reads it) and takes
//! it back at its unmap (`cp_unplace_queue`), its engine starting at the MQD's read pointer (an MQD whose pointers
//! are not Linux's 0 is refused); the doorbell (`doorbell_write64` at the queue's dword, as Linux's
//! `set_wptr_gfx` and `set_wptr_compute` ring it after the write pointer's 64-bit shadow, gfx_v12_0.c:4456-4471,
//! :4494-4506) runs the queue up to the new write pointer.
//!
//! The interpreter is the 780M's (`amdgpu_fmt::sim::Engine`) speaking GFX 12 (`speak_gfx12`): Linux's packets for
//! this card run, a CE packet or any packet gfx_v12_0.c never emits halts the queue at the packet, where its read
//! pointer then stands, and raises the bad-opcode interrupt; a packet that writes a register the model does not
//! know halts it with the privileged-register interrupt ("Illegal register access in command stream",
//! gfx_v12_0.c:5104-5110). Every address goes through the GC hub as the card would walk it: the GART through
//! GFXHUB's walker (`Model::gc_walk`: an entry counts only after an HDP flush and GFXHUB's own invalidation), VRAM
//! through the hub's frame-buffer aperture; an address with neither is a fault latched in GFXHUB's status with the
//! CP's client id (`gfxhub_client_ids`, gfxhub_v12_0.c:37-59: CPF 4 for a fetch, CPC 5 or CPG 6 for data) and
//! raised to GFXHUB's source in the IH (`Model::raise_fault`), never a silent success. A RELEASE_MEM with an
//! interrupt raises the end-of-pipe entry (client GRBM_CP, source 181, the queue's ring id, its doorbell in the
//! first data dword) when the pipe's TIME_STAMP and GENERIC0 enables are on (`fakering`). The read pointer goes
//! back into its write-back slot as 32 bits of dwords (gfx_v12_0.c:4434-4438). VRAM is seen as it stands: the
//! CP's model keeps no HDP staleness of its own (the copy engines' model does).
//!
//! A doorbell the card takes but no queue answers (no queue mapped at the dword, or its engine halted) is a posted
//! write: the card ignores it and nothing runs, so the ring test behind it times out as on the card; the model
//! takes the write and names it in `refused`, never silently.

use super::fakemodel::{MC_BASE, Model};
use alloc::format;
use alloc::string::String;
use amdgpu_fmt::bus::{GpuBus, MemFault};
use amdgpu_fmt::sim::{Engine, Event, Hang, IntSrc, Isa, Limits, Stop};
use rdna4_core::fault12::st;
use rdna4_core::gc12cp as cp;
use rdna4_core::gc12ring as rg;
use rdna4_core::ih7::{Iv, Ring};
use rdna4_core::{doorbell12 as db, hub41, layout, nbreg, pte12};

/// `gfxhub_client_ids` (gfxhub_v12_0.c:37-59).
const CID_CPF: u32 = 4;
const CID_CPC: u32 = 5;
const CID_CPG: u32 = 6;
const PERM_VALID: u32 = 1;
const PERM_READ: u32 = 2;
const PERM_WRITE: u32 = 4;
/// One doorbell's run: a ring's worth of dwords several times over; ring, IB and IB2 (`max_ib_depth` 2).
const LIMITS: Limits = Limits { max_dw: 1 << 16, max_bytes: 1 << 20, max_ib_depth: 2 };

/// A kernel queue as MES places it: its place on the CP (ME 0 for gfx, MEC 1 for compute), its ring (GPU
/// addresses of VMID 0), its write-back slots, its doorbell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueAt {
    /// A graphics queue (PFP and ME) or a compute one (MEC).
    pub gfx: bool,
    pub me: u32,
    pub pipe: u32,
    pub queue: u32,
    pub ring_mc: u64,
    pub ring_dw: u64,
    /// The 32-bit read pointer's slot and the 64-bit write pointer's shadow.
    pub rptr_mc: u64,
    pub wptr_mc: u64,
    /// The queue's doorbell, a BAR2 dword (`ring->doorbell_index`).
    pub doorbell_dw: u32,
    /// The read and write pointers the MQD holds for the queue's start (dwords).
    pub mqd_rptr: u32,
    pub mqd_wptr: u64,
}

pub struct CpQueue {
    pub at: QueueAt,
    pub eng: Engine,
    /// Why the queue stopped, once it did: it runs no more.
    pub halted: Option<String>,
    /// Why its last run ended with work left (a poll not satisfied, the run's budget spent, a packet not complete),
    /// until a later doorbell's run ends idle.
    pub waits: Option<String>,
}

impl Model {
    /// What MES does when it maps a kernel queue: onto a pipe of the unhalted command processor. The queue's
    /// doorbell must lie in the CP's range for its kind and be no other queue's, its ring on a page.
    pub(super) fn cp_place_queue(&mut self, q: QueueAt) -> Result<(), String> {
        let n = self.nb.n.clone();
        if !(self.gc_awake() && if q.gfx { self.cp.gfx_on } else { self.cp.mec_on }) {
            return Err(format!("no queue on a halted {}", if q.gfx { "PFP and ME" } else { "MEC" }));
        }
        let (lo, hi) = if q.gfx { (cp::CP_RB_DOORBELL_RANGE_LOWER, cp::CP_RB_DOORBELL_RANGE_UPPER) } else { (cp::CP_MEC_DOORBELL_RANGE_LOWER, cp::CP_MEC_DOORBELL_RANGE_UPPER) };
        if !(self.cp.val(&n, lo)..=self.cp.val(&n, hi)).contains(&(q.doorbell_dw * 4)) {
            return Err(format!("doorbell {:#x} outside the CP's range for the queue's kind", q.doorbell_dw));
        }
        if self.cp.queues.iter().any(|x| x.at.doorbell_dw == q.doorbell_dw) {
            return Err(format!("doorbell {:#x} is another queue's", q.doorbell_dw));
        }
        if q.ring_mc % 256 != 0 {
            return Err(format!("a ring at {:#x}: the queue's base is in units of 256 bytes", q.ring_mc));
        }
        // Linux maps a kernel queue from an MQD whose pointers are 0, the restored one of a resume included
        // (gfx_v12_0.c:3012-3013, :3086, :3225-3227, :3288, with GFX 12's own defaults :61, :68).
        if q.mqd_rptr != 0 || q.mqd_wptr != 0 {
            return Err(format!("the queue's MQD holds read pointer {:#x} and write pointer {:#x}: Linux maps a kernel queue with both 0", q.mqd_rptr, q.mqd_wptr));
        }
        let mut eng = Engine::new(Isa::Pm4, q.ring_mc, q.ring_dw, u64::from(q.mqd_rptr), LIMITS).map_err(|e| format!("{e:?}"))?;
        eng.speak_gfx12();
        self.cp.queues.push(CpQueue { at: q, eng, halted: None, waits: None });
        Ok(())
    }

    /// What MES's unmap leaves: the queue off its pipe, its engine gone (a later map starts it anew).
    pub(super) fn cp_unplace_queue(&mut self, doorbell_dw: u32) -> bool {
        let before = self.cp.queues.len();
        self.cp.queues.retain(|q| q.at.doorbell_dw != doorbell_dw);
        self.cp.queues.len() != before
    }

    /// A doorbell below SDMA's: the command processor's or MES's (`None`: not the CP's dword range).
    pub(super) fn cp_doorbell(&mut self, index_dw: u32, v: u64) -> Option<bool> {
        if index_dw >= db::dword(db::FIRST_NON_CP) {
            return None;
        }
        let n = self.nb.n.clone();
        if !self.gc_awake() {
            return Some(self.refuse(format!("doorbell {index_dw:#x} rung while the GC block has no power or its autoload is not complete")));
        }
        let mes = index_dw == db::MES_SCHED_DW || index_dw == db::MES_KIQ_DW;
        let (entry, want) = if mes { (self.cp.entries[1], db::ENTRY_3_MES) } else { (self.cp.entries[0], db::ENTRY_0_GFX) };
        if self.nb.val(nbreg::DOORBELL_APER_EN) & 1 == 0 || entry != Some(want) {
            return Some(self.refuse(format!("doorbell {index_dw:#x} rung before the aperture and NBIF's doorbell entry {} were set up as Linux sets them", if mes { 3 } else { 0 })));
        }
        if !(self.cp.ranges(&n, true) && self.cp.ranges(&n, false)) {
            return Some(self.refuse(format!("doorbell {index_dw:#x} rung before the CP's doorbell ranges hold Linux's values")));
        }
        if mes {
            return self.mes_doorbell(index_dw, v);
        }
        let Some(i) = self.cp.queues.iter().position(|q| q.at.doorbell_dw == index_dw) else {
            self.refused.push(format!("doorbell {index_dw:#x}: no queue was placed with this doorbell (the card takes the write and nothing runs)"));
            return Some(true);
        };
        let q = self.cp.queues[i].at;
        if !(if q.gfx { self.cp.gfx_on } else { self.cp.mec_on }) {
            self.refused.push(format!("doorbell {index_dw:#x} of a queue on a halted engine (the card takes the write and nothing runs)"));
            return Some(true);
        }
        let mut b = [0u8; 8];
        let held = self.cp_piece(q.wptr_mc, Some(&mut b), None).then(|| u64::from_le_bytes(b));
        if held != Some(v) {
            return Some(self.refuse(format!("doorbell {index_dw:#x} written {v:#x} while the queue's write-pointer shadow holds {held:x?}: Linux writes the shadow first (gfx_v12_0.c:4460-4461)")));
        }
        self.cp.doorbells.push((index_dw, v));
        self.cp_run(i, v);
        Some(true)
    }

    /// One piece of an engine's access, inside one page: the GART through GFXHUB's walker, VRAM directly.
    fn cp_piece(&mut self, mc: u64, read: Option<&mut [u8]>, write: Option<&[u8]>) -> bool {
        let sys = if mc < layout::GART_BYTES { self.gc_walk(mc) } else { None };
        let vram = (mc >= layout::GART_BYTES && self.in_vram(mc, 1)).then(|| mc - MC_BASE);
        match (sys, vram, read, write) {
            (Some(s), _, Some(out), _) => self.nb.sys_peek(s, out),
            (Some(s), _, _, Some(data)) => self.nb.sys_poke(s, data),
            (_, Some(off), Some(out), _) => self.peek(off, out),
            (_, Some(off), _, Some(data)) => {
                let end = off + data.len() as u64;
                let held = self.nb.vram_held().into_iter().find(|(_, r)| off < r.end && r.start < end).map(|(w, _)| w);
                if let Some(what) = held.or((off < self.picture.end && end > self.picture.start).then_some("the firmware's picture")) {
                    self.refused.push(format!("the command processor told to write into {what} (vram {off:#x})"));
                    return false;
                }
                self.poke(off, data);
            }
            _ => return false,
        }
        true
    }

    /// GFXHUB refused the queue's access: latched with the CP's client id and raised to GFXHUB's fault source.
    fn cp_fault(&mut self, gfx: bool, fetch: bool, f: MemFault) {
        let cid = if fetch { CID_CPF } else if gfx { CID_CPG } else { CID_CPC };
        let known = f.addr < layout::GART_BYTES || self.in_vram(f.addr, 1);
        let kind = if known { st::PERMISSION_FAULTS.put(0, PERM_VALID | if f.write { PERM_WRITE } else { PERM_READ }) } else { st::MAPPING_ERROR.put(0, 1) };
        let status = st::VMID.put(st::RW.put(st::CID.put(kind, cid), u32::from(f.write)), u32::from(f.vmid));
        let _ = self.raise_fault(hub41::Kind::Gc, f.addr, status, f.write);
    }

    /// An entry of the CP's sources for the queue, when its pipe's enables `bits` are on (none otherwise, as the
    /// card raises none); a ring that is off loses it, as the card's IH drops it.
    fn cp_raise(&mut self, q: &QueueAt, src: u8, bits: u32) {
        if self.cp_int_on(q.me, q.pipe, bits) {
            let iv = Iv { client_id: rg::CLIENT_GRBM_CP, src_id: src, ring_id: rg::ring_id(q.me, q.pipe, q.queue), src_data: [q.doorbell_dw, 0, 0, 0], ..Iv::default() };
            let _ = self.raise(Ring::Ring0, &iv);
            self.cp.raised.push((src, iv.ring_id));
        }
    }

    /// Queue `i` runs up to the write pointer (dwords) it was rung with, then writes its read pointer back.
    fn cp_run(&mut self, i: usize, wptr: u64) {
        if self.cp.queues[i].halted.is_some() {
            return;
        }
        let gfx = self.cp.queues[i].at.gfx;
        let mut eng = self.cp.queues[i].eng.clone();
        let mut bus = Bus { m: self, fetch_fault: false };
        let mut stop = Stop::Budget;
        let (mut eops, mut unknown_reg) = (0, false);
        for _ in 0..64 {
            let r = eng.run(&mut bus, wptr);
            for e in &r.events {
                match e {
                    Event::Interrupt { src: IntSrc::EndOfPipe, .. } => eops += 1,
                    Event::RegWrite { modelled: false, .. } => unknown_reg = true,
                    _ => {}
                }
            }
            stop = r.stop;
            if stop != Stop::Budget || unknown_reg {
                break;
            }
        }
        let fetch = bus.fetch_fault;
        let at = self.cp.queues[i].at;
        for _ in 0..eops {
            self.cp_raise(&at, rg::SRC_EOP, rg::EOP_ENABLES);
        }
        let hang = match stop {
            Stop::Hang(h) => Some(format!("{h:?}")),
            _ if unknown_reg => Some(String::from("Illegal register access in command stream")),
            _ => None,
        };
        self.cp.queues[i].waits = match stop {
            Stop::Wait(w) => Some(format!("waits in a poll ({w:?})")),
            Stop::Budget => Some(String::from("the run's budget was spent with packets left")),
            Stop::Incomplete { need, have } => Some(format!("a packet is not complete ({have} of {need} dwords)")),
            Stop::Idle | Stop::Hang(_) => None,
        };
        if let Some(h) = hang {
            match stop {
                Stop::Hang(Hang::Fault(f)) => self.cp_fault(gfx, fetch, f),
                Stop::Hang(Hang::Unsupported { .. } | Hang::Pm4(_)) => self.cp_raise(&at, rg::SRC_BAD_OPCODE, rg::f::OPCODE_ERROR_INT_ENABLE.mask),
                _ if unknown_reg => self.cp_raise(&at, rg::SRC_PRIV_REG, rg::f::PRIV_REG_INT_ENABLE.mask),
                _ => {}
            }
            let why = format!("CP queue at doorbell {:#x} stopped at read pointer {:#x}: {h}", at.doorbell_dw, eng.rptr());
            self.refused.push(why.clone());
            self.cp.queues[i].halted = Some(why);
        }
        let (rptr_mc, rptr) = (self.cp.queues[i].at.rptr_mc, eng.rptr() as u32);
        self.cp.queues[i].eng = eng;
        if !self.cp_piece(rptr_mc, None, Some(&rptr.to_le_bytes())) {
            self.cp_fault(gfx, false, MemFault { vmid: 0, addr: rptr_mc, write: true });
            let why = format!("the CP queue's read-pointer slot at mc {rptr_mc:#x} is not reachable");
            self.refused.push(why.clone());
            self.cp.queues[i].halted = Some(why);
        }
    }
}

/// Memory and registers as the command processor reaches them.
struct Bus<'a> {
    m: &'a mut Model,
    /// The last fault was a fetch of the ring's or an IB's dwords.
    fetch_fault: bool,
}

impl Bus<'_> {
    fn each(&mut self, vmid: u8, addr: u64, len: usize, write: bool, mut f: impl FnMut(&mut Model, u64, core::ops::Range<usize>) -> bool) -> Result<(), MemFault> {
        let mut done = 0;
        while done < len {
            let at = addr.wrapping_add(done as u64);
            let n = ((pte12::PAGE - at % pte12::PAGE) as usize).min(len - done);
            // Only the kernel's address space exists for the CP so far (VMID 0).
            if vmid != 0 || !f(self.m, at, done..done + n) {
                return Err(MemFault { vmid, addr: at, write });
            }
            done += n;
        }
        Ok(())
    }
}

impl GpuBus for Bus<'_> {
    fn read(&mut self, vmid: u8, addr: u64, out: &mut [u8]) -> Result<(), MemFault> {
        self.fetch_fault = false;
        let len = out.len();
        self.each(vmid, addr, len, false, |m, at, r| m.cp_piece(at, Some(&mut out[r]), None))
    }
    fn write(&mut self, vmid: u8, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        self.fetch_fault = false;
        self.each(vmid, addr, data.len(), true, |m, at, r| m.cp_piece(at, None, Some(&data[r])))
    }
    fn fetch(&mut self, vmid: u8, addr: u64, out: &mut [u8]) -> Result<(), MemFault> {
        let r = self.read(vmid, addr, out);
        self.fetch_fault = r.is_err();
        r
    }
    /// SCRATCH_REG0 and NBIF's HDP flush done register (a ring's HDP flush polls it).
    fn reg_read(&mut self, reg: u32) -> Option<u32> {
        let n = &self.m.nb.n;
        if n.byte(cp::SCRATCH_REG0) == Some(reg * 4) {
            return Some(self.m.cp.scratch);
        }
        let (done, _) = self.m.sdma.r.hdp_flush_dwords()?;
        (reg == done).then_some(self.m.cp.hdp_done)
    }
    /// SET_UCONFIG_REG or WRITE_DATA of SCRATCH_REG0 (the ring test), and the HDP flush request.
    fn reg_write(&mut self, reg: u32, value: u32) -> bool {
        if self.m.nb.n.byte(cp::SCRATCH_REG0) == Some(reg * 4) {
            self.m.cp.scratch = value;
            return true;
        }
        match self.m.sdma.r.hdp_flush_dwords() {
            Some((_, req)) if reg == req => {
                self.m.cp.hdp_done |= value;
                self.m.hdp_flushed();
                true
            }
            _ => false,
        }
    }
}
