//! Wave 43, item HB: the model's GFXHUB and the page faults of both hubs. GFXHUB is the hub MMHUB is, with other
//! registers and two differences the trace and Linux's source show: no semaphore around an invalidation
//! (gmc_v12_0.c:189-194), and its acknowledge reads 0x00000001 where MMHUB's reads 0x00010001. The model refuses,
//! and keeps:
//!
//! - GFXHUB's context 0 enabled before its page table base and range were written, or on another table than
//!   the GART table MMHUB was given (Linux hands both hubs `adev->gart.bo`);
//! - an invalidation request before GFXHUB's context 0 is on, or one that is not Linux's request for VMID 0;
//! - a system address (the fault's dummy page) before the DMAR survey allowed it;
//! - the SMU's firmware loaded anew while GFXHUB's context 0 is on (`fakecmd`): Linux restarts the block only
//!   after its fini switched the hub off (gfx_v12_0.c:3917).
//!
//! The card's walker through GFXHUB sees a table entry only after an HDP flush AND an invalidation on GFXHUB's
//! engine 17 that follow the write (`fakegart`): a bind that flushes MMHUB alone leaves GFXHUB's view old.
//!
//! Faults: `raise_fault` latches a status in the hub's register and sends the IH entry as the card would (client
//! 10 source 0 through ring 1 with the IH's own entry on ring 0 once `ih` redirected it; client 18 on ring 0).
//! The status stands until the hub's fault control is written with its clear bit, so a fault nobody read and
//! cleared stays visible (`faults_open`). What the real card does with that bit is in no trace
//! (plan/spec/gpu-rdna4.md section 7): the model lets it clear the status and read back 0.

use super::fakegc::hub_index;
use super::fakemodel::Model;
use super::nbregs::NbRegs;
use alloc::format;
use alloc::string::String;
use rdna4_core::fault12::{self, st};
use rdna4_core::hub41::{self, Kind};
use rdna4_core::ih7::{self, Iv, Ring};
use rdna4_core::nbreg::member;
use rdna4_core::pte12;
use rdna4_core::regmap::Reg;

impl super::fakegc::FakeGc {
    /// The GART table's VRAM offset as GFXHUB was told, once its base was written.
    fn table(&self, n: &NbRegs) -> Option<u64> {
        let h = &hub41::GC;
        let v = |r: Reg| n.byte(r).and_then(|b| self.vals.get(&b)).copied().unwrap_or(0);
        (self.ctx0 & 3 == 3).then(|| (u64::from(v(h.ctx0_base_hi)) << 32 | u64::from(v(h.ctx0_base_lo))) & pte12::ADDR_MASK)
    }

    /// An HDP flush arrived: what was written reaches VRAM; the hub's TLB still holds what it held.
    pub(super) fn table_flushed(&mut self) {
        self.pending.iter_mut().for_each(|p| *p.flushed() = true);
    }

    /// The hub's TLB is empty (an invalidation of VMID 0, or context 0 switched on).
    fn tlb_emptied(&mut self) {
        self.pending.retain_mut(|p| !*p.flushed());
    }

    /// Entries GFXHUB's walker does not see yet (tests).
    pub fn entries_pending(&self) -> usize {
        self.pending.len()
    }
}

impl Model {
    /// The acknowledge of GFXHUB's engine 17: bit 0 for VMID 0 (init.txt line 9361), once per request. Linux
    /// reads it until the bit shows and no further; that a second wait without a request of its own runs out is
    /// the model's strictness, not a fact of the card.
    pub(super) fn gchub_ack(&mut self) -> u32 {
        let v = u32::from(self.gc.acked && !self.gc.ack_never);
        self.gc.acked &= v == 0;
        v
    }

    /// A write to a register of GFXHUB (the block is awake and pipe 0 is selected).
    pub(super) fn gchub_write(&mut self, n: &NbRegs, byte: u32, v: u32) -> bool {
        let h = &hub41::GC;
        let is = |r: Reg| n.byte(r) == Some(byte);
        if is(h.fault_cntl) && hub41::f::CLEAR_FAULT_STATUS.get(v) != 0 {
            return self.fault_cleared(Kind::Gc, byte, v);
        }
        if is(h.fault_default_lo) && !self.nb.dma_ok() {
            return self.refuse(String::from("a system address handed to GFXHUB before the DMAR survey allowed it"));
        }
        let ctx0 = [h.ctx0_base_lo, h.ctx0_base_hi, h.ctx0_start_lo, member(h.ctx0_start_lo, 1, 1), h.ctx0_end_lo, member(h.ctx0_end_lo, 1, 1)];
        if let Some(i) = ctx0.iter().position(|r| is(*r)) {
            self.gc.ctx0 |= 1 << i;
        }
        if is(h.context0_cntl) && hub41::f::ENABLE_CONTEXT.get(v) != 0 {
            if self.gc.ctx0 != 0x3F {
                return self.refuse(String::from("GFXHUB's context 0 enabled before its page table base and range were written"));
            }
            if !self.nb.context0 || self.gc.table(n) != self.nb.table() {
                return self.refuse(format!("GFXHUB's context 0 enabled on the page table {:x?}, which is not the GART table MMHUB runs on ({:x?}): Linux hands both hubs the one table", self.gc.table(n), self.nb.table()));
            }
            if !self.gc.context0 {
                // A context that starts has no translation cached.
                self.gc.tlb_emptied();
            }
            self.gc.context0 = true;
        } else if is(h.context0_cntl) {
            // `gfxhub_v12_0_gart_disable` writes 0: the hub translates nothing any more.
            self.gc.context0 = false;
        }
        if is(member(h.eng0_req, hub41::GART_ENGINE, 1)) {
            if !self.gc.context0 {
                return self.refuse(String::from("an invalidation request on GFXHUB before its context 0 is on"));
            }
            if v != hub41::invalidate_req(0) {
                return self.refuse(format!("GFXHUB invalidation request {v:#x}: Linux flushes VMID 0 with {:#x}", hub41::invalidate_req(0)));
            }
            (self.gc.acked, self.gc.tlb_flushes) = (true, self.gc.tlb_flushes + 1);
            self.gc.tlb_emptied();
        }
        self.gc.vals.insert(byte, v);
        true
    }

    /// The system address behind an MC address of the GART as the card's walker through GFXHUB finds it now
    /// (what an engine on that hub, SDMA or GFX, would reach).
    pub fn gc_walk(&self, mc: u64) -> Option<u64> {
        let table = self.gc.table(&self.nb.n)?;
        self.walk(table, self.gc_awake() && self.gc.context0, &self.gc.pending, mc)
    }

    /// A hub's fault control written with its clear bit: the status is gone, and the bit does not stay.
    pub(super) fn fault_cleared(&mut self, hub: Kind, byte: u32, v: u32) -> bool {
        let i = hub_index(hub);
        if self.gc.fault_status[i] != 0 {
            (self.gc.fault_status[i], self.gc.faults_cleared[i]) = (0, self.gc.faults_cleared[i] + 1);
        }
        let stored = hub41::f::CLEAR_FAULT_STATUS.put(v, 0);
        match hub {
            Kind::Gc => self.gc.vals.insert(byte, stored),
            Kind::Mm => self.nb.vals.insert(byte, stored),
        };
        true
    }

    /// Faults whose status nobody read and cleared yet, per hub (GFXHUB, MMHUB).
    pub fn faults_open(&self) -> [bool; 2] {
        self.gc.fault_status.map(|s| s != 0)
    }

    /// The card raises a page fault of `hub` at `addr` (a page of the faulting VM) with this status: the status
    /// into the hub's register (a fault behind one that still stands only sets MORE_FAULTS there), the entry
    /// into the IH.
    pub fn raise_fault(&mut self, hub: Kind, addr: u64, status: u32, write: bool) -> Result<(), String> {
        let on = match hub {
            Kind::Gc => self.gc_awake() && self.gc.context0,
            Kind::Mm => self.nb.context0,
        };
        if !on {
            return Err(format!("{hub:?}: the hub is not on"));
        }
        let i = hub_index(hub);
        self.gc.fault_status[i] = if self.gc.fault_status[i] == 0 { status } else { st::MORE_FAULTS.put(self.gc.fault_status[i], 1) };
        (self.gc.faults_raised[i], self.gc.fault_reads[i]) = (self.gc.faults_raised[i] + 1, 0);
        let flags = if write { fault12::DATA_WRITE } else { fault12::DATA_READ };
        let iv = Iv {
            client_id: if hub == Kind::Gc { fault12::CLIENT_GFX } else { fault12::CLIENT_VMC },
            src_id: fault12::SRC_FAULT,
            vmid_src: hub == Kind::Mm,
            vmid: st::VMID.get(status) as u8,
            src_data: [(addr >> 12) as u32, (addr >> 44) as u32 & 0xF | flags, 0, 0],
            ..Iv::default()
        };
        // "Redirect the interrupts to IH RB1 for dGPU" (ih_v7_0.c:386-389): client 10 source 0 lands on ring 1,
        // and the IH's own entry on ring 0 brings ring 1's write pointer (ih_v7_0_self_irq :552-566).
        let cfg = self.nb.val(ih7::RING1_CLIENT_CFG_DATA);
        let ring1 = hub == Kind::Gc && self.nb.ih_on[1] && cfg == ih7::ring1_client_cfg(cfg) && ih7::f::CFG_SOURCE_ID_MATCH_ENABLE.get(cfg) != 0;
        if !ring1 {
            return self.raise(Ring::Ring0, &iv);
        }
        self.raise(Ring::Ring1, &iv)?;
        let wptr = self.nb.wptr[1] & ih7::PTR_MASK;
        self.raise(Ring::Ring0, &Iv { client_id: ih7::CLIENT_IH, ring_id: 1, src_data: [wptr, 0, 0, 0], ..Iv::default() })
    }
}
