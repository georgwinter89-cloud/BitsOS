//! A GPU VM's 4-level page tables, built and changed by the CPU (Linux's CPU update path,
//! amdgpu_vm_cpu.c; BitsOS never updates page tables with SDMA).
//!
//! Geometry of GFX 11 (gmc_v11_0.c:791, amdgpu_vm_pt.c:49-62): 48-bit VA, root PDB2 -> PDB1 -> PDB0 -> PTB,
//! 512 entries per table, 4 KiB pages. Huge leaves (PDE_PTE) on PDB0 (2 MiB) and PDB1 (1 GiB), never on the
//! root. Every change is a commit in three steps: allocate every table it can need (the only step that can
//! fail; nothing is written before it succeeds), write the entries, then unlink tables left empty. Unlinked
//! tables go to a pending list with the VM's TLB sequence number and return to the store only through
//! [`Vm::reclaim`] once the caller has flushed the GPU TLB for that number: until then the walker of a busy
//! GPU may still read them.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::addr::{self, Carveout, VA_SIZE};
use crate::blocks::{self, Blocks, Ent};
use crate::pte::{self, INVALID_PDE, INVALID_PTE, MapFlags, Target};
use crate::store::{ENTRIES, TableAddr, TableStore};
use crate::{GPU_PAGE, GPU_PAGE_SHIFT, errno};

mod tree;
use tree::Node;

/// Page-table levels, root first (amdgpu_vm.h:142-145).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Pdb2,
    Pdb1,
    Pdb0,
    Ptb,
}

impl Level {
    /// All levels from the root down.
    pub const ALL: [Level; 4] = [Level::Pdb2, Level::Pdb1, Level::Pdb0, Level::Ptb];

    /// Right shift of a VA page number to this level's index: 9 per level above the PTB (block size 9).
    pub const fn shift(self) -> u32 {
        match self {
            Level::Pdb2 => 27,
            Level::Pdb1 => 18,
            Level::Pdb0 => 9,
            Level::Ptb => 0,
        }
    }

    /// Bytes one entry of this level covers.
    pub const fn span(self) -> u64 {
        GPU_PAGE << self.shift()
    }

    pub(crate) const fn depth(self) -> usize {
        self as usize
    }

    pub(crate) fn invalid(self) -> u64 {
        if self == Level::Ptb { INVALID_PTE } else { INVALID_PDE }
    }

    pub(crate) fn index(self, pfn: u64) -> usize {
        ((pfn >> self.shift()) as usize) & (ENTRIES - 1)
    }
}

/// What backs a new mapping.
#[derive(Clone, Copy, Debug)]
pub enum Backing<'a> {
    /// Contiguous carve-out memory at this host-physical address (fragments and huge leaves allowed). The
    /// whole range must lie inside the VM's carve-out, or the map is refused (a local PTE can reach any host
    /// address, so this is the bound Linux gets from TTM's VRAM offsets, gmc_v11_0.c:497-500).
    Local { phys: u64 },
    /// System pages, one DMA (= physical, IOMMU off) address per 4 KiB page; always 4 KiB PTEs, frag 0.
    System { pages: &'a [u64], snooped: bool },
    /// Partially resident range without memory (AMDGPU_VM_PAGE_PRT with no BO).
    Prt,
}

/// The stored form of a mapping's memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Local { phys: u64 },
    System { pages: Vec<u64>, snooped: bool },
    Prt,
}

/// One mapping of the VM: `start` and the length are in bytes of the 48-bit index space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapping {
    pub start: u64,
    pub pages: u64,
    pub kind: Kind,
    pub flags: MapFlags,
}

impl Mapping {
    pub fn end(&self) -> u64 {
        self.start + (self.pages << GPU_PAGE_SHIFT)
    }
    fn start_pfn(&self) -> u64 {
        self.start >> GPU_PAGE_SHIFT
    }
    fn end_pfn(&self) -> u64 {
        self.start_pfn() + self.pages
    }

    fn blocks(&self) -> Blocks {
        let (pa, scattered) = match &self.kind {
            Kind::Local { phys } => (Some(phys >> GPU_PAGE_SHIFT), false),
            Kind::System { .. } => (None, true),
            Kind::Prt => (None, false),
        };
        Blocks::new(self.start_pfn(), self.end_pfn(), pa, scattered)
    }

    /// The entry value for `e`.
    fn value(&self, e: Ent) -> Result<u64, pte::EncodeError> {
        let off = e.pfn - self.start_pfn();
        let huge = e.level != Level::Ptb;
        match &self.kind {
            Kind::Local { phys } => {
                pte::leaf(phys + (off << GPU_PAGE_SHIFT), e.level.span(), Target::Local, self.flags, e.frag, huge)
            }
            Kind::System { pages, snooped } => {
                let p = pages.get(off as usize).copied().ok_or(pte::EncodeError::Misaligned(off))?;
                pte::leaf(p, GPU_PAGE, Target::System { snooped: *snooped }, self.flags, e.frag, huge)
            }
            Kind::Prt => pte::prt(self.flags, e.frag, huge),
        }
    }

    /// The part `[from, to)` (pages, inside this mapping) as its own mapping.
    fn piece(&self, from: u64, to: u64) -> Mapping {
        let skip = from - self.start_pfn();
        let kind = match &self.kind {
            Kind::Local { phys } => Kind::Local { phys: phys + (skip << GPU_PAGE_SHIFT) },
            Kind::System { pages, snooped } => {
                let part = pages.get(skip as usize..(to - self.start_pfn()) as usize).unwrap_or_default();
                Kind::System { pages: part.to_vec(), snooped: *snooped }
            }
            Kind::Prt => Kind::Prt,
        };
        Mapping { start: from << GPU_PAGE_SHIFT, pages: to - from, kind, flags: self.flags }
    }
}

/// Why a VM operation was refused. Nothing changed when one is returned (except [`VmError::Broken`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmError {
    /// VA in the canonical hole, not page aligned, or the range leaves the 48-bit space.
    BadVa(u64),
    /// Length 0, not a page multiple, or overflowing.
    BadLength(u64),
    /// Physical address unaligned, wider than 48 bits, or (local memory) outside the carve-out.
    BadPhys(u64),
    /// Page list length does not match the mapping length.
    PageCount { expected: u64, got: u64 },
    /// The range overlaps the mapping starting at this address (amdgpu_vm.c bo_map: -EINVAL).
    Overlap(u64),
    /// No mapping starts here (amdgpu_vm.c bo_unmap: -ENOENT).
    NotMapped(u64),
    /// The store had no table left.
    NoMemory,
    /// The store returned a table address a PDE cannot hold.
    BadTable(u64),
    /// An internal invariant broke during a write; the VM refuses further changes, destroy it.
    Broken,
}

impl VmError {
    /// The Linux errno for this error.
    pub fn errno(&self) -> i32 {
        match self {
            VmError::NotMapped(_) => errno::ENOENT,
            VmError::NoMemory => errno::ENOMEM,
            VmError::BadTable(_) | VmError::Broken => errno::EIO,
            _ => errno::EINVAL,
        }
    }
}

/// One VM: its tables (shadowed on the CPU side), its mappings and its TLB sequence.
pub struct Vm {
    local: Carveout,
    root: Node,
    maps: BTreeMap<u64, Mapping>,
    seq: u64,
    pending: Vec<(u64, TableAddr)>,
    broken: bool,
}

/// Validates `[va, va + len)`; returns (start, end) in pages of the index space.
fn range(va: u64, len: u64) -> Result<(u64, u64), VmError> {
    let start = addr::va_index(va).ok_or(VmError::BadVa(va))?;
    if start % GPU_PAGE != 0 {
        return Err(VmError::BadVa(va));
    }
    if len == 0 || !len.is_multiple_of(GPU_PAGE) {
        return Err(VmError::BadLength(len));
    }
    let end = start.checked_add(len).ok_or(VmError::BadLength(len))?;
    if end > VA_SIZE {
        return Err(VmError::BadVa(va));
    }
    Ok((start >> GPU_PAGE_SHIFT, end >> GPU_PAGE_SHIFT))
}

fn check_phys(p: u64) -> Result<(), VmError> {
    if !p.is_multiple_of(GPU_PAGE) || p >> addr::VA_BITS != 0 {
        return Err(VmError::BadPhys(p));
    }
    Ok(())
}

impl Vm {
    /// A VM with an empty root table from `store`; local mappings must lie inside `local`.
    pub fn new(store: &mut dyn TableStore, local: Carveout) -> Result<Vm, VmError> {
        let t = tree::alloc_checked(store)?;
        store.fill(t.phys, INVALID_PDE);
        Ok(Vm { local, root: Node::new(t), maps: BTreeMap::new(), seq: 0, pending: Vec::new(), broken: false })
    }

    /// The root table.
    pub fn root(&self) -> TableAddr {
        self.root.table
    }

    /// The page-table base register value of this VM (root PDE).
    pub fn pd_addr(&self) -> u64 {
        // The root's address was checked by alloc_checked, so pde() cannot fail.
        pte::pd_addr(self.root.table).unwrap_or(0)
    }

    /// Counter of committed changes; a TLB flush is due when a VMID last flushed a lower number.
    pub fn tlb_seq(&self) -> u64 {
        self.seq
    }

    /// The mappings, ordered by start.
    pub fn mappings(&self) -> impl Iterator<Item = &Mapping> {
        self.maps.values()
    }

    /// The mapping containing this VA, if any.
    pub fn lookup(&self, va: u64) -> Option<&Mapping> {
        let idx = addr::va_index(va)?;
        let (_, m) = self.maps.range(..=idx).next_back()?;
        (idx < m.end()).then_some(m)
    }

    /// Tables reachable from the root, the root included.
    pub fn table_count(&self) -> usize {
        self.root.count()
    }

    /// Unlinked tables waiting for a TLB flush.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Maps `[va, va + len)`. Refused when it overlaps a mapping.
    pub fn map(&mut self, store: &mut dyn TableStore, va: u64, len: u64, backing: Backing, flags: MapFlags) -> Result<u64, VmError> {
        let (s, e) = range(va, len)?;
        let kind = match backing {
            Backing::Local { phys } => {
                check_phys(phys)?;
                let end = phys.checked_add(len).ok_or(VmError::BadPhys(phys))?;
                if end > VA_SIZE || !self.local.contains_phys(phys, len) {
                    return Err(VmError::BadPhys(phys));
                }
                Kind::Local { phys }
            }
            Backing::System { pages, snooped } => {
                if pages.len() as u64 != e - s {
                    return Err(VmError::PageCount { expected: e - s, got: pages.len() as u64 });
                }
                pages.iter().try_for_each(|&p| check_phys(p))?;
                let mut copy = Vec::new();
                copy.try_reserve_exact(pages.len()).map_err(|_| VmError::NoMemory)?;
                copy.extend_from_slice(pages);
                Kind::System { pages: copy, snooped }
            }
            Backing::Prt => Kind::Prt,
        };
        if let Some(m) = self.overlapping(s, e).next() {
            return Err(VmError::Overlap(m.start));
        }
        let m = Mapping { start: s << GPU_PAGE_SHIFT, pages: e - s, kind, flags };
        self.commit(store, &[], core::slice::from_ref(&m))?;
        self.maps.insert(m.start, m);
        Ok(self.seq)
    }

    /// Removes the mapping that starts at `va` (the whole mapping, like amdgpu_vm_bo_unmap).
    pub fn unmap(&mut self, store: &mut dyn TableStore, va: u64) -> Result<u64, VmError> {
        let idx = addr::va_index(va).ok_or(VmError::BadVa(va))?;
        let m = self.maps.get(&idx).cloned().ok_or(VmError::NotMapped(va))?;
        self.commit(store, core::slice::from_ref(&m), &[])?;
        self.maps.remove(&idx);
        Ok(self.seq)
    }

    /// Removes every translation in `[va, va + len)`, splitting mappings that stick out (like
    /// amdgpu_vm_bo_clear_mappings for AMDGPU_VA_OP_CLEAR/REPLACE). Entries outside the range that stay the same
    /// are never cleared on the way. A range with nothing mapped is fine.
    pub fn clear(&mut self, store: &mut dyn TableStore, va: u64, len: u64) -> Result<u64, VmError> {
        let (s, e) = range(va, len)?;
        let old: Vec<Mapping> = self.overlapping(s, e).cloned().collect();
        if old.is_empty() {
            return Ok(self.seq);
        }
        let mut keep = Vec::new();
        for m in &old {
            if m.start_pfn() < s {
                keep.push(m.piece(m.start_pfn(), s));
            }
            if m.end_pfn() > e {
                keep.push(m.piece(e, m.end_pfn()));
            }
        }
        self.commit(store, &old, &keep)?;
        for m in &old {
            self.maps.remove(&m.start);
        }
        for m in keep {
            self.maps.insert(m.start, m);
        }
        Ok(self.seq)
    }

    /// Returns unlinked tables whose sequence number is at most `flushed` to the store.
    pub fn reclaim(&mut self, store: &mut dyn TableStore, flushed: u64) -> usize {
        let before = self.pending.len();
        self.pending.retain(|&(seq, t)| {
            if seq <= flushed {
                store.free(t);
                false
            } else {
                true
            }
        });
        before - self.pending.len()
    }

    /// Frees every table, pending ones included. Only once no VMID uses this VM any more and its TLB is
    /// flushed. (Explicit, no Drop: the store is not owned.)
    pub fn destroy(mut self, store: &mut dyn TableStore) {
        self.root.free_all(store);
        for (_, t) in self.pending.drain(..) {
            store.free(t);
        }
    }

    fn overlapping(&self, s: u64, e: u64) -> impl Iterator<Item = &Mapping> {
        let first = self
            .maps
            .range(..(s << GPU_PAGE_SHIFT))
            .next_back()
            .filter(|(_, m)| m.end_pfn() > s)
            .map(|(_, m)| m);
        let rest = self.maps.range((s << GPU_PAGE_SHIFT)..(e << GPU_PAGE_SHIFT)).map(|(_, m)| m);
        first.into_iter().chain(rest)
    }

    /// Removes `old` and writes `new` (pieces of `old` for a clear). See the module comment.
    fn commit(&mut self, store: &mut dyn TableStore, old: &[Mapping], new: &[Mapping]) -> Result<(), VmError> {
        if self.broken {
            return Err(VmError::Broken);
        }
        let need = self.root.count_missing(new, store.available()).ok_or(VmError::NoMemory)?;
        let mut pool = Vec::new();
        pool.try_reserve_exact(need).map_err(|_| VmError::NoMemory)?;
        for _ in 0..need {
            match tree::alloc_checked(store) {
                Ok(t) => pool.push(t),
                Err(err) => {
                    pool.into_iter().for_each(|t| store.free(t));
                    return Err(err);
                }
            }
        }
        let next = self.seq + 1;
        let mut ok = true;
        // New entries first: an entry that stays at the same slot is overwritten in one store (old value to
        // new value, no hole in between); where a piece needs a table under an old leaf, set() replaces the
        // leaf (the GPU may fault on the kept pages of that one block meanwhile, as with Linux).
        'write: for m in new {
            for (pfn, frag) in m.blocks() {
                for en in blocks::entries(pfn, frag) {
                    let Ok(v) = m.value(en) else {
                        ok = false;
                        break 'write;
                    };
                    if !self.root.set(store, &mut pool, &mut self.pending, next, en, v) {
                        ok = false;
                        break 'write;
                    }
                }
            }
        }
        // Then old entries that no new entry replaced. One that a piece covers only in part became a table
        // in set(), so clear() finds no leaf there.
        for m in old {
            for (pfn, frag) in m.blocks() {
                for en in blocks::entries(pfn, frag) {
                    let end = en.pfn + (1u64 << en.level.shift());
                    if !new.iter().any(|p| p.start_pfn() <= en.pfn && end <= p.end_pfn()) {
                        self.root.clear(store, en);
                    }
                }
            }
        }
        for m in old {
            self.root.prune(store, Level::Pdb2, 0, m.start_pfn(), m.end_pfn(), &mut self.pending, next);
        }
        pool.into_iter().for_each(|t| store.free(t));
        self.seq = next;
        if !ok {
            self.broken = true;
            return Err(VmError::Broken);
        }
        Ok(())
    }
}
