//! Wave 43: what the model's SDMA engines do with a ring. The interpreter is the one the 780M's fake uses
//! (`amdgpu_fmt::sim::Engine`), speaking SDMA 7.0's packets (`amdgpu_fmt::sdma7`): Linux's own packets for this
//! card run, and anything else stops the engine at the packet, where its read pointer and its status then stand,
//! as a ring test that runs out shows it on the real card.
//!
//! Every address an engine touches goes through the GC hub as the card would walk it (`gc_hub_walk`, which is
//! `fakegchub`'s walker through GFXHUB): the GART's table for an address inside the GART, valid and seen by
//! GFXHUB's own walker (HDP-flushed and invalidated on GFXHUB, not only on MMHUB) or it is a fault, latched in
//! GFXHUB's status and raised to its fault source (`Model::raise_fault`), never a silent success; VRAM through
//! the hub's frame-buffer aperture.
//! VRAM the CPU wrote through BAR0 is seen by an engine only after an HDP flush, the driver's or the one the
//! engine makes from its ring: until then it reads what lay there before. A trap goes into the IH ring as the
//! entry the real card sent Linux (client 10, source 49, ring id engine << 4: plan/hw/gaming-pc-gpu-2026-11-18's
//! load trace), only while SDMA_CNTL's trap enable is set; the read pointer goes back into its write-back slot as
//! 64 bits of bytes.

use super::fakemodel::{MC_BASE, Model};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use amdgpu_fmt::bus::{GpuBus, MemFault};
use amdgpu_fmt::sim::{Engine, EngineError, Event, Hang, IntSrc, Isa, Limits, Stop};
use rdna4_core::ih7::{Iv, Ring};
use rdna4_core::sdma7::{self, f};
use rdna4_core::fault12::st;
use rdna4_core::{hub41, layout, pte12};

/// What GFXHUB's status says an engine's access failed on, in the bits of PERMISSION_FAULTS: the order of the
/// fault control's enables (valid, read, write, execute; gc_12_0_0_sh_mask.h's
/// GCVM_L2_PROTECTION_FAULT_CNTL__VALID/READ/WRITE/EXECUTE_PROTECTION_FAULT_ENABLE_DEFAULT). Which bits the real
/// hub sets for a page with no valid entry is in no source and no trace (plan/spec/gpu-rdna4.md section 7).
const PERM_VALID: u32 = 1;
const PERM_READ: u32 = 2;
const PERM_WRITE: u32 = 4;
/// `gfxhub_client_ids` (gfxhub_v12_0.c:37-59): SDMA0 is client 13, SDMA1 14.
const CID_SDMA0: u32 = 13;

/// One run's bounds: a ring's worth of dwords several times over, Linux's largest copy.
const LIMITS: Limits = Limits { max_dw: 1 << 16, max_bytes: 1 << 30, max_ib_depth: 1 };

/// An engine on a ring of `ring_dw` dwords at `base`, its pointers at `start` (dwords).
pub(super) fn engine(base: u64, ring_dw: u64, start: u64) -> Result<Engine, EngineError> {
    let mut e = Engine::new(Isa::Sdma, base, ring_dw, start, LIMITS)?;
    e.speak_sdma7();
    Ok(e)
}

/// What lies behind an address an engine names: system memory behind the GART, or an offset into VRAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Behind {
    Sys(u64),
    Vram(u64),
}

impl Behind {
    /// How far `self` lies behind `from` in the same memory, if it does.
    fn after(self, from: Behind) -> Option<u64> {
        match (self, from) {
            (Behind::Sys(a), Behind::Sys(b)) | (Behind::Vram(a), Behind::Vram(b)) => a.checked_sub(b),
            _ => None,
        }
    }
}

/// Tests: an engine that gets its data wrong, so a rung's own comparison has something to find.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Spoil {
    /// After each piece the engine writes into the `bytes` bytes at `when`, `data` lands at `at`.
    Also { when: Behind, bytes: u64, at: Behind, data: Vec<u8> },
    /// The engine reads the byte at this place with its bits turned.
    Misread(Behind),
}

impl Model {
    /// The system address behind an address of the GART as an engine of the GC block reaches it: GFXHUB's VMID 0
    /// on the GART's one table, as GFXHUB's own walker sees it (`fakegchub`'s `gc_walk`: the hub on, the entry
    /// HDP-flushed and invalidated on GFXHUB). Both engines' `vm_hub` is GFXHUB (sdma_v7_0.c:1328), so a bind
    /// that only MMHUB was flushed for is not seen here.
    pub(super) fn gc_hub_walk(&self, mc: u64) -> Option<u64> {
        self.gc_walk(mc)
    }

    /// GFXHUB refused engine `i`'s access: kept, latched in GFXHUB's status with the engine's client id and
    /// raised to GFXHUB's fault source in the IH (`raise_fault`: client 0x0a, source 0, gmc_v12_0.c:890-893 for
    /// GC 12.0.1), where the driver's fault decode reads it. An address that lies in the GART or in VRAM failed
    /// on its page's entry (permission bits), any other on the mapping.
    pub(super) fn gc_hub_fault(&mut self, i: usize, vmid: u8, mc: u64, write: bool) {
        self.sdma.hub_faults.push((mc, write));
        let known = mc < layout::GART_BYTES || self.in_vram(mc, 1);
        let kind = if known { st::PERMISSION_FAULTS.put(0, PERM_VALID | if write { PERM_WRITE } else { PERM_READ }) } else { st::MAPPING_ERROR.put(0, 1) };
        let status = st::VMID.put(st::RW.put(st::CID.put(kind, CID_SDMA0 + i as u32), u32::from(write)), u32::from(vmid));
        // A hub that is off, or an IH ring that is off, takes no fault: the engine has stopped in any case.
        let _ = self.raise_fault(hub41::Kind::Gc, mc, status, write);
    }

    fn behind(&self, mc: u64) -> Option<Behind> {
        if mc < layout::GART_BYTES {
            return self.gc_hub_walk(mc).map(Behind::Sys);
        }
        self.in_vram(mc, 1).then(|| Behind::Vram(mc - MC_BASE))
    }

    /// VRAM as an engine reads it: what the CPU wrote since the last HDP flush is not there yet.
    fn vram_seen(&self, off: u64, out: &mut [u8]) {
        self.peek(off, out);
        // The oldest write's bytes from before are what the card still has: laid on last.
        for (at, old) in self.sdma.stale.iter().rev() {
            let (lo, hi) = (off.max(*at), (off + out.len() as u64).min(at + old.len() as u64));
            if lo < hi {
                out[(lo - off) as usize..(hi - off) as usize].copy_from_slice(&old[(lo - at) as usize..(hi - at) as usize]);
            }
        }
    }

    /// A CPU write into VRAM is about to land: an engine that runs keeps seeing what lay there.
    pub(super) fn sdma_mem_note(&mut self, at: u64, data: &[u8]) {
        if self.sdma.any_on() {
            let mut old = alloc::vec![0u8; data.len()];
            self.peek(at, &mut old);
            self.sdma.stale.push((at, old));
        }
    }

    pub(super) fn sdma_hdp_flushed(&mut self) {
        self.sdma.stale.clear();
    }

    /// One piece of an engine's access, inside one page.
    fn eng_piece(&mut self, mc: u64, read: Option<&mut [u8]>, write: Option<&[u8]>) -> bool {
        let Some(b) = self.behind(mc) else { return false };
        match (b, read, write) {
            (Behind::Sys(sys), Some(out), _) => {
                self.nb.sys_peek(sys, out);
                self.spoil_read(b, out);
            }
            (Behind::Vram(off), Some(out), _) => {
                self.vram_seen(off, out);
                self.spoil_read(b, out);
            }
            (Behind::Sys(sys), _, Some(data)) => {
                self.nb.sys_poke(sys, data);
                self.spoil_written(b, data.len() as u64);
            }
            (Behind::Vram(off), _, Some(data)) => {
                let end = off + data.len() as u64;
                let held = self.nb.vram_held().into_iter().find(|(_, r)| off < r.end && r.start < end).map(|(what, _)| what);
                let what = if off < self.picture.end && end > self.picture.start { Some("the firmware's picture") } else { held };
                if let Some(what) = what {
                    self.refused.push(format!("an SDMA engine told to write into {what} (vram {off:#x})"));
                    return false;
                }
                self.poke(off, data);
                self.spoil_written(b, data.len() as u64);
            }
            _ => return false,
        }
        true
    }

    fn spoil_read(&self, from: Behind, out: &mut [u8]) {
        if let Some(Spoil::Misread(at)) = &self.sdma.spoil
            && let Some(byte) = at.after(from).and_then(|k| out.get_mut(k as usize))
        {
            *byte = !*byte;
        }
    }

    fn spoil_written(&mut self, from: Behind, len: u64) {
        let Some(Spoil::Also { when, bytes, at, data }) = self.sdma.spoil.clone() else { return };
        let touched = from.after(when).is_some_and(|k| k < bytes) || when.after(from).is_some_and(|k| k < len);
        match at {
            _ if !touched => {}
            Behind::Sys(sys) => self.nb.sys_poke(sys, &data),
            Behind::Vram(off) => self.poke(off, &data),
        }
    }

    /// Engine `i` runs its ring up to the write pointer it was given.
    pub(super) fn sdma_run(&mut self, i: usize) {
        if self.sdma.dead[i] || self.sdma.e[i].halted.is_some() {
            return;
        }
        let Some(mut eng) = self.sdma.e[i].eng.take() else { return };
        let wptr = self.sdma.e[i].wptr >> 2;
        let mut traps = 0u32;
        let mut stop = Stop::Budget;
        // A run that meets its bound goes on where it stopped; a ring's worth of runs is more than any ring holds.
        for _ in 0..64 {
            let rep = eng.run(&mut Bus { m: self }, wptr);
            traps += rep.events.iter().filter(|e| matches!(e, Event::Interrupt { src: IntSrc::Trap, .. })).count() as u32;
            stop = rep.stop;
            if stop != Stop::Budget {
                break;
            }
        }
        let cntl = self.sdma.e[i].val(sdma7::CNTL);
        for _ in 0..traps {
            let iv = Iv { client_id: sdma7::CLIENT_GFX, src_id: sdma7::SRC_TRAP, ring_id: sdma7::trap_ring_id(i as u32), ..Iv::default() };
            if f::TRAP_ENABLE.get(cntl) != 0 && self.raise(Ring::Ring0, &iv).is_ok() {
                self.sdma.e[i].traps += 1;
            } else {
                self.sdma.e[i].traps_lost += 1;
            }
        }
        self.sdma.e[i].rptr = eng.rptr() << 2;
        if let Stop::Hang(h) = stop {
            let why = format!("SDMA{i} stopped at read pointer {:#x}: {h:?}", eng.rptr() << 2);
            if let Hang::Fault(MemFault { vmid, addr, write }) = h {
                self.gc_hub_fault(i, vmid, addr, write);
            }
            self.refused.push(why.clone());
            self.sdma.e[i].halted = Some(why);
        }
        self.sdma_rptr_back(i);
        self.sdma.e[i].eng = Some(eng);
    }

    /// The read pointer into its write-back slot, through the hub like every access of the engine.
    fn sdma_rptr_back(&mut self, i: usize) {
        let e = &self.sdma.e[i];
        if f::RPTR_WRITEBACK_ENABLE.get(e.val(sdma7::RB_CNTL)) == 0 {
            return;
        }
        let (at, rptr) = (e.addr(sdma7::RB_RPTR_ADDR_LO, sdma7::RB_RPTR_ADDR_HI), e.rptr);
        match self.gc_hub_walk(at) {
            Some(sys) => self.nb.sys_poke(sys, &rptr.to_le_bytes()),
            None => {
                self.gc_hub_fault(i, 0, at, true);
                let why: String = format!("SDMA{i}'s read-pointer slot at mc {at:#x} is not bound any more");
                self.refused.push(why.clone());
                self.sdma.e[i].halted = Some(why);
            }
        }
    }
}

/// Memory and registers as an engine reaches them.
struct Bus<'a> {
    m: &'a mut Model,
}

impl Bus<'_> {
    /// Page by page: neighbouring pages of the GART need not be neighbours in system memory.
    fn each(&mut self, vmid: u8, addr: u64, len: usize, write: bool, mut f: impl FnMut(&mut Model, u64, core::ops::Range<usize>) -> bool) -> Result<(), MemFault> {
        let mut done = 0;
        while done < len {
            let at = addr.wrapping_add(done as u64);
            let n = ((pte12::PAGE - at % pte12::PAGE) as usize).min(len - done);
            // Only the kernel's address space exists for these engines so far (VMID 0, `ring->vm_hub`'s GART).
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
        let len = out.len();
        self.each(vmid, addr, len, false, |m, at, r| m.eng_piece(at, Some(&mut out[r]), None))
    }
    fn write(&mut self, vmid: u8, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        self.each(vmid, addr, data.len(), true, |m, at, r| m.eng_piece(at, None, Some(&data[r])))
    }
    /// The in-ring HDP flush polls NBIF's done register (`sdma_v7_0_ring_emit_hdp_flush`).
    fn reg_read(&mut self, reg: u32) -> Option<u32> {
        let (done, _) = self.m.sdma.r.hdp_flush_dwords()?;
        (reg == done).then_some(self.m.sdma.hdp_done)
    }
    /// ... after it wrote its bit to the request register: that is an HDP flush like the driver's own.
    fn reg_write(&mut self, reg: u32, value: u32) -> bool {
        match self.m.sdma.r.hdp_flush_dwords() {
            Some((_, req)) if reg == req => {
                self.m.sdma.hdp_done |= value;
                self.m.sdma.ring_flushes += 1;
                self.m.hdp_flushed();
                true
            }
            _ => false,
        }
    }
}
