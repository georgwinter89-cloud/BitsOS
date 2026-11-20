//! Wave 42 (piece 10): the card's GART, VMID 0's flat table of 131072 entries in VRAM, and the TLB flush Linux
//! makes after changing it. A: `amdgpu_gart.c` (bind :474-484, map :354-374, unbind :308-338, invalidate_tlb
//! :492-506), `gmc_v12_0.c` (flush_gpu_tlb :305-330, flush_vm_hub :213-303), entries `rdna4_core::pte12`.
//!
//! The table's memory is written through the card's `Hw` at MC addresses, so a stage that binds lists
//! `Gart::table_mc` in its guard. One table serves both hubs (Linux's `adev->gart.bo`), so from the stage that
//! starts GFXHUB on a bind or unbind flushes both, GFXHUB first (`FlushRegs::gc`, `Session::flush_regs`). `map` and `unmap` are what a stage of a later item calls for a buffer of system
//! pages (the secure processor's firmware buffer among them); `linuxseq`'s `Rule::BindBefore` holds their flush
//! against Linux's.

use super::host::Card;
use super::nbregs::NbRegs;
use crate::gpucore::psp::{self, Cond, Hw, LINUX_POLLS, PspError};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;
use rdna4_core::hub41::{self, Layout12};
use rdna4_core::nbreg::member;
use rdna4_core::pte12::{self, PAGE, PteError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GartStop {
    /// No run of free pages that long is left in the GART.
    Full { pages: usize },
    Page(PteError),
    /// The table's memory could not be written.
    Table { mc: u64 },
    /// Nothing is bound at that address.
    NotBound { mc: u64 },
    Reg(PspError),
}

/// The registers of a TLB flush, as BAR5 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlushRegs {
    /// Where the HDP flush goes now (the remapped place after `nbif`).
    pub hdp: u32,
    pub memsize: u32,
    /// MMHUB's engine 17 and its private invalidation.
    pub sem: u32,
    pub req: u32,
    pub ack: u32,
    pub cid2: u32,
    /// GFXHUB's engine 17 (request, acknowledge), from the stage that starts that hub on: a flush after a bind
    /// then walks both hubs, as Linux's does once `gfx.is_poweron` (gmc_v12_0.c:308).
    pub gc: Option<(u32, u32)>,
}

/// One hub's part of a flush: its request and acknowledge, and what only MMHUB has (`hub41::Hub::semaphore`,
/// `cid2`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HubFlush {
    kind: hub41::Kind,
    sem: Option<u32>,
    req: u32,
    ack: u32,
    cid2: Option<u32>,
}

impl FlushRegs {
    /// MMHUB's flush alone: all there is until the graphics block's hub is on.
    pub fn of(n: &NbRegs, hdp: u32) -> Option<FlushRegs> {
        let h = &hub41::MM;
        let eng = |first| n.byte(member(first, hub41::GART_ENGINE, 1));
        Some(FlushRegs { hdp, memsize: n.memsize, sem: eng(h.eng0_sem)?, req: eng(h.eng0_req)?, ack: eng(h.eng0_ack)?, cid2: n.byte(h.cid2?)?, gc: None })
    }

    /// Both hubs' flush.
    pub fn with_gc(self, n: &NbRegs) -> Option<FlushRegs> {
        let eng = |first| n.byte(member(first, hub41::GART_ENGINE, 1));
        Some(FlushRegs { gc: Some((eng(hub41::GC.eng0_req)?, eng(hub41::GC.eng0_ack)?)), ..self })
    }

    /// Every register a flush writes (a stage's guard list).
    pub fn written(&self) -> Vec<u32> {
        [self.hdp, self.sem, self.req, self.cid2].into_iter().chain(self.gc.map(|(req, _)| req)).collect()
    }

    fn hub(&self, kind: hub41::Kind) -> Option<HubFlush> {
        match kind {
            hub41::Kind::Mm => Some(HubFlush { kind, sem: Some(self.sem), req: self.req, ack: self.ack, cid2: Some(self.cid2) }),
            hub41::Kind::Gc => self.gc.map(|(req, ack)| HubFlush { kind, sem: None, req, ack, cid2: None }),
        }
    }
}

/// Linux's HDP flush (amdgpu_hdp.c:51-60): 0 into the flush register, then a read of the memory size.
pub fn flush_hdp(hw: &mut dyn Hw, f: &FlushRegs) -> Result<(), PspError> {
    psp::write(hw, f.hdp, 0)?;
    psp::read(hw, f.memsize).map(|_| ())
}

/// One of the flush's two waits. Linux logs a wait that runs out and goes on (gmc_v12_0.c:247-249, :289-292):
/// so does this, with a line.
fn wait(hw: &mut dyn Hw, byte: u32, what: &'static str, out: &mut Vec<String>) -> Result<(), PspError> {
    match psp::wait_reads(hw, byte, Cond::Masked { mask: 1, value: 1 }, LINUX_POLLS, what) {
        Ok(_) => Ok(()),
        Err(PspError::Timeout { last, .. }) => {
            klog::event!(Error, "gx", "rdna4.tlb_flush.timeout", what = what, last = klog::Hex(u64::from(last)); "a wait of the TLB flush ran out; going on as Linux does");
            out.push(format!("tlb flush: the wait for the {what} ran out after {LINUX_POLLS} reads (last {last:#x}); Linux logs it and goes on, so does this"));
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// `gmc_v12_0_flush_vm_hub` (gmc_v12_0.c:213-303) for VMID 0 on one hub: the engine's semaphore where the hub
/// has one, the request, its acknowledge, the semaphore's release, MMHUB's private invalidation.
fn flush_vm_hub(hw: &mut dyn Hw, h: &HubFlush, out: &mut Vec<String>) -> Result<(), PspError> {
    let gc = h.kind == hub41::Kind::Gc;
    if let Some(sem) = h.sem {
        wait(hw, sem, "semaphore", out)?;
    }
    psp::write(hw, h.req, hub41::invalidate_req(0))?;
    wait(hw, h.ack, if gc { "acknowledge of the graphics hub" } else { "acknowledge" }, out)?;
    if let Some(sem) = h.sem {
        psp::write(hw, sem, 0)?;
    }
    if let Some(cid2) = h.cid2 {
        let v = psp::read(hw, cid2)?;
        psp::write(hw, cid2, v | hub41::RESERVED_CACHE_PRIVATE_INVALIDATION)?;
        psp::read(hw, cid2)?;
    }
    Ok(())
}

/// `gmc_v12_0_flush_gpu_tlb` (gmc_v12_0.c:305-330) for VMID 0 on one hub by registers (before MES runs): the
/// HDP flush, then the hub's. On GFXHUB it returns at once, without the HDP flush, while that hub is not on
/// (Linux: `!adev->gfx.is_poweron`, :308).
pub fn flush_hub_tlb(hw: &mut dyn Hw, f: &FlushRegs, kind: hub41::Kind, out: &mut Vec<String>) -> Result<(), PspError> {
    let Some(h) = f.hub(kind) else { return Ok(()) };
    flush_hdp(hw, f)?;
    flush_vm_hub(hw, &h, out)
}

/// `amdgpu_gart_invalidate_tlb` (amdgpu_gart.c:492-506): an HDP flush of its own, then every hub of
/// `vmhubs_mask` in the mask's order, GFXHUB (bit 0) before MMHUB (bit 8, gmc_v12_0.c:834-835).
pub fn invalidate_tlb(hw: &mut dyn Hw, f: &FlushRegs, out: &mut Vec<String>) -> Result<(), PspError> {
    flush_hdp(hw, f)?;
    flush_hub_tlb(hw, f, hub41::Kind::Gc, out)?;
    flush_hub_tlb(hw, f, hub41::Kind::Mm, out)
}

/// The TLB flush after a GART change in the form this boot is in: by registers (`FlushRegs`) until MES runs,
/// through MES from then on (`mesflush`, Linux's `gmc_v12_0_flush_gpu_tlb` with `mes.ring[0].sched.ready`,
/// gmc_v12_0.c:315-327). The MES form writes system memory and rings a doorbell, so it takes the card with its
/// host side.
pub trait TlbFlush {
    fn invalidate(&mut self, card: &mut dyn Card, out: &mut Vec<String>) -> Result<(), PspError>;
}

impl TlbFlush for FlushRegs {
    fn invalidate(&mut self, card: &mut dyn Card, out: &mut Vec<String>) -> Result<(), PspError> {
        invalidate_tlb(card.hw(), self, out)
    }
}

/// The GART of one card in this boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gart {
    pub l: Layout12,
    /// VRAM offsets of the table and of the default page.
    pub table: Range<u64>,
    pub scratch: Range<u64>,
    /// The system page an unbound entry points at and a fault reads.
    pub dummy: u64,
    /// (what, first page of the GART, the system pages) of everything bound.
    pub bound: Vec<(&'static str, u64, Vec<u64>)>,
}

impl Gart {
    pub fn new(l: Layout12, table: Range<u64>, scratch: Range<u64>, dummy: u64) -> Gart {
        Gart { l, table, scratch, dummy, bound: Vec::new() }
    }

    pub fn pages(&self) -> u64 {
        self.l.gart_bytes / PAGE
    }

    /// The table as MC addresses (what a stage that binds may write).
    pub fn table_mc(&self) -> Range<u64> {
        self.l.mc(self.table.start)..self.l.mc(self.table.end)
    }

    /// The system address behind an MC address of the GART.
    pub fn system_of(&self, mc: u64) -> Option<u64> {
        let page = mc.checked_sub(self.l.gart_start)? / PAGE;
        self.bound.iter().find_map(|(_, first, pages)| pages.get(usize::try_from(page.checked_sub(*first)?).ok()?)).map(|p| p + mc % PAGE)
    }

    /// The lowest run of `n` free pages. Page 0 stays unbound: no buffer of the card gets MC address 0.
    fn room(&self, n: u64) -> Option<u64> {
        let mut at = 1;
        let mut busy: Vec<(u64, u64)> = self.bound.iter().map(|(_, first, p)| (*first, *first + p.len() as u64)).collect();
        busy.sort_unstable();
        for (start, end) in busy {
            if at + n <= start {
                break;
            }
            at = at.max(end);
        }
        (at + n <= self.pages()).then_some(at)
    }

    fn write(&self, hw: &mut dyn Hw, first: u64, entries: &[u64]) -> Result<(), GartStop> {
        let mc = self.l.mc(self.table.start + first * 8);
        let bytes: Vec<u8> = entries.iter().flat_map(|e| e.to_le_bytes()).collect();
        // One block: the kernel's glue maps a window of VRAM around each write.
        if hw.mem_write(mc, &bytes) { Ok(()) } else { Err(GartStop::Table { mc }) }
    }

    /// `amdgpu_gart_bind`: the entries of `pages` (system addresses) at the lowest free place; the MC address of
    /// the first. No flush: `invalidate_tlb` after it, unless the GART is not on yet.
    pub fn bind(&mut self, hw: &mut dyn Hw, what: &'static str, pages: &[u64]) -> Result<u64, GartStop> {
        let entries = pages.iter().map(|p| pte12::bound(*p)).collect::<Result<Vec<u64>, PteError>>().map_err(GartStop::Page)?;
        let first = self.room(pages.len() as u64).filter(|_| !pages.is_empty()).ok_or(GartStop::Full { pages: pages.len() })?;
        self.write(hw, first, &entries)?;
        hw.store_barrier();
        self.bound.push((what, first, pages.to_vec()));
        Ok(self.l.gart_start + first * PAGE)
    }

    /// `amdgpu_gart_unbind`'s entries for what `bind` returned `mc` for: the dummy page with no flag.
    pub fn unbind(&mut self, hw: &mut dyn Hw, mc: u64) -> Result<(), GartStop> {
        let page = mc.checked_sub(self.l.gart_start).filter(|o| o.is_multiple_of(PAGE)).map(|o| o / PAGE);
        let at = self.bound.iter().position(|(_, first, _)| Some(*first) == page).ok_or(GartStop::NotBound { mc })?;
        let (_, first, pages) = self.bound.remove(at);
        self.write(hw, first, &alloc::vec![pte12::unbound(self.dummy); pages.len()])?;
        hw.store_barrier();
        Ok(())
    }

    /// Binds and flushes the TLB as Linux does after a bind.
    pub fn map(&mut self, hw: &mut dyn Hw, f: &FlushRegs, what: &'static str, pages: &[u64], out: &mut Vec<String>) -> Result<u64, GartStop> {
        let mc = self.bind(hw, what, pages)?;
        invalidate_tlb(hw, f, out).map_err(GartStop::Reg)?;
        Ok(mc)
    }

    /// Unbinds and flushes the TLB (`amdgpu_gart_unbind` ends in `amdgpu_gart_invalidate_tlb`).
    pub fn unmap(&mut self, hw: &mut dyn Hw, f: &FlushRegs, mc: u64, out: &mut Vec<String>) -> Result<(), GartStop> {
        self.unbind(hw, mc)?;
        invalidate_tlb(hw, f, out).map_err(GartStop::Reg)
    }

    /// `map` with the flush in this boot's form (`TlbFlush`): what a stage that may run after MES binds with.
    pub fn map_with(&mut self, card: &mut dyn Card, f: &mut dyn TlbFlush, what: &'static str, pages: &[u64], out: &mut Vec<String>) -> Result<u64, GartStop> {
        let mc = self.bind(card.hw(), what, pages)?;
        f.invalidate(card, out).map_err(GartStop::Reg)?;
        Ok(mc)
    }

    /// `unmap` with the flush in this boot's form.
    pub fn unmap_with(&mut self, card: &mut dyn Card, f: &mut dyn TlbFlush, mc: u64, out: &mut Vec<String>) -> Result<(), GartStop> {
        self.unbind(card.hw(), mc)?;
        f.invalidate(card, out).map_err(GartStop::Reg)
    }
}
