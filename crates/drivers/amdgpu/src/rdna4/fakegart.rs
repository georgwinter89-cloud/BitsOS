//! Wave 42: the model's GART as the card's own walker sees it, which is not what the driver last wrote. A table
//! entry written through BAR0 reaches VRAM with the next HDP flush, and the hub keeps the translation it had until
//! an invalidation on engine 17 after that; Linux never uses a page it bound before both
//! (`amdgpu_gart_invalidate_tlb`, amdgpu_gart.c:492-506: the HDP flush, then the hub's flush). So the walker
//! answers with an entry's value from BEFORE the write until then, and a ring that starts on such a page, or an
//! interrupt into it, is refused.
//!
//! Also here: what the model refuses of the table's content, on the table's own 8-byte grid. A write is checked
//! when it comes if the table's place is known; what was written before `MMVM_CONTEXT0_PAGE_TABLE_BASE_ADDR`
//! named the place (Linux fills the table when it allocates it, amdgpu_gart.c:278, long before
//! `gmc_v12_0_gart_enable` names it to the hub) is checked when context 0 is switched on.

use super::fakemodel::Model;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;
use rdna4_core::{layout, pte12};

/// Table bytes the walker does not see yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Pending {
    /// VRAM was written before the table's place was known and not flushed since: the walker finds nothing valid.
    Whole { flushed: bool },
    /// Entries from the VRAM offset `at` on, with their bytes from before the write.
    Piece { at: u64, old: Vec<u8>, flushed: bool },
}

impl Pending {
    pub(super) fn flushed(&mut self) -> &mut bool {
        match self {
            Pending::Whole { flushed } | Pending::Piece { flushed, .. } => flushed,
        }
    }
}

const ENTRY: u64 = 8;

impl super::fakenb::FakeNb {
    /// The table's VRAM range, once CONTEXT0's base was written.
    pub fn table_range(&self) -> Option<Range<u64>> {
        self.table().map(|t| t..t + layout::GART_BYTES / pte12::PAGE * ENTRY)
    }

    /// An HDP flush arrived: what was written reaches VRAM; the hub's TLB still holds what it held.
    pub(super) fn table_flushed(&mut self) {
        self.vram_dirty = false;
        self.pending.iter_mut().for_each(|p| *p.flushed() = true);
    }

    /// The hub's TLB is empty (an invalidation of VMID 0 on engine 17, or context 0 switched on): every entry
    /// that reached VRAM is what the walker finds now.
    pub(super) fn tlb_emptied(&mut self) {
        self.pending.retain_mut(|p| !*p.flushed());
    }

    /// CONTEXT0's base was written: the table's place is known from here.
    pub(super) fn table_named(&mut self) {
        self.pending.clear();
        if self.vram_dirty {
            self.pending.push(Pending::Whole { flushed: false });
        }
    }

    /// Entries the walker does not see yet (tests).
    pub fn entries_pending(&self) -> usize {
        self.pending.len()
    }
}

/// Why a valid system entry would not be Linux's, if it is not.
fn entry_refused(index: u64, entry: u64, dma_ok: bool) -> Option<String> {
    let d = pte12::decode(entry);
    if !(d.valid && d.system) {
        return None;
    }
    if !dma_ok {
        return Some(String::from("a system page put into the GART before the DMAR survey allowed it"));
    }
    (!layout::dma_reachable(d.addr, pte12::PAGE)).then(|| format!("GART entry {index} points at {:#x}, above the card's 44 bits", d.addr))
}

impl Model {
    /// The system address behind an MC address of the GART, as the card's walker through MMHUB finds it now.
    pub fn gart_walk(&self, mc: u64) -> Option<u64> {
        self.walk(self.nb.table()?, self.nb.context0, &self.nb.pending, mc)
    }

    /// One hub's walk: its table, whether its context 0 is on, and the writes its TLB has not let go of. Each
    /// hub keeps its own translations, so a write is seen by a hub only after the HDP flush and that hub's own
    /// invalidation (`amdgpu_gart_invalidate_tlb` walks every hub, amdgpu_gart.c:504-505).
    pub(super) fn walk(&self, table: u64, on: bool, pending: &[Pending], mc: u64) -> Option<u64> {
        if !on || mc >= layout::GART_BYTES {
            return None;
        }
        let at = table + mc / pte12::PAGE * ENTRY;
        let mut e = [0u8; ENTRY as usize];
        // The oldest write the walker has not seen decides: its bytes from before are what the card still has.
        let hidden = pending.iter().find(|p| match p {
            Pending::Whole { .. } => true,
            Pending::Piece { at: a, old, .. } => at >= *a && at + ENTRY <= *a + old.len() as u64,
        });
        match hidden {
            Some(Pending::Whole { .. }) => {}
            Some(Pending::Piece { at: a, old, .. }) => e.copy_from_slice(&old[(at - a) as usize..][..ENTRY as usize]),
            None => self.vram_peek(at, &mut e),
        }
        let d = pte12::decode(u64::from_le_bytes(e));
        (d.valid && d.system).then_some(d.addr + mc % pte12::PAGE)
    }

    /// The table entries a write of `len` bytes at the VRAM offset `at` touches: (first index, count).
    fn touched(&self, at: u64, len: usize) -> Option<(Range<u64>, u64, u64)> {
        let t = self.nb.table_range()?;
        let (lo, hi) = (at.max(t.start), at.saturating_add(len as u64).min(t.end));
        (lo < hi).then(|| {
            let first = (lo - t.start) / ENTRY;
            (t.clone(), first, (hi - 1 - t.start) / ENTRY - first + 1)
        })
    }

    /// Why a write into VRAM is refused as a GART entry, if the table is known and it is one. Entries are taken
    /// on the table's grid, whatever the write's own start: a byte the write leaves is the one in VRAM.
    pub(super) fn nb_mem_refused(&self, at: u64, data: &[u8]) -> Option<String> {
        let (t, first, count) = self.touched(at, data.len())?;
        (first..first + count).find_map(|i| {
            let mut e = [0u8; ENTRY as usize];
            self.vram_peek(t.start + i * ENTRY, &mut e);
            for (k, b) in e.iter_mut().enumerate() {
                if let Some(new) = (t.start + i * ENTRY + k as u64).checked_sub(at).and_then(|o| data.get(usize::try_from(o).ok()?)) {
                    *b = *new;
                }
            }
            entry_refused(i, u64::from_le_bytes(e), self.nb.dma_ok())
        })
    }

    /// A write into VRAM is about to land (it passed `nb_mem_refused`): the walker keeps what it saw.
    pub(super) fn nb_mem_note(&mut self, at: u64, data: &[u8]) {
        self.nb.vram_dirty = true;
        let Some((t, first, count)) = self.touched(at, data.len()) else { return };
        self.nb.table_unfenced = true;
        let mut old = alloc::vec![0u8; (count * ENTRY) as usize];
        self.vram_peek(t.start + first * ENTRY, &mut old);
        let piece = Pending::Piece { at: t.start + first * ENTRY, old, flushed: false };
        self.gc.pending.push(piece.clone());
        self.nb.pending.push(piece);
    }

    /// Context 0 is about to be switched on: the whole table as it stands in the model's VRAM, entry by entry
    /// (what was written before the table's place was known was never checked). The table starts on a page.
    pub(super) fn table_refused(&self) -> Option<String> {
        let t = self.nb.table_range()?;
        let fill = u64::from_le_bytes([pte12::FILL_BYTE; ENTRY as usize]);
        (t.start / pte12::PAGE..t.end / pte12::PAGE).find_map(|p| {
            self.vram_page(p)?.chunks_exact(ENTRY as usize).enumerate().find_map(|(i, e)| {
                let e = u64::from_le_bytes(e.try_into().ok()?);
                // Linux's fill and a zero entry are the common case and never valid.
                if e == fill || e == 0 { None } else { entry_refused((p * pte12::PAGE - t.start) / ENTRY + i as u64, e, self.nb.dma_ok()) }
            })
        })
    }
}
