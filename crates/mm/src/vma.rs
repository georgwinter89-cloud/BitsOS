//! The VMA tree of one address space (pure, host-tested): disjoint, sorted ranges with their protection and
//! kind, merged with equal neighbours like Linux's `vma_merge`, split by partial unmaps and protects.
//!
//! The backing store is a sorted `Vec` searched by bisection rather than a `BTreeMap`: every operation reserves
//! its room first (`try_reserve`), so running out of kernel heap is an `ENOMEM` for the asking program and
//! never an allocation abort of the kernel (R2). A program's VMA count stays in the thousands (Y-NUMBERS:
//! ~2,250 commits), where moving the tail on insert costs microseconds; MM-5b may switch to a node-preallocating
//! B-tree behind the same API if the numbers ask for it.

use alloc::sync::Arc;
use alloc::vec::Vec;
use crate::device::DeviceMap;
use vfs::mmap::MapBacking;

/// Linux protection bits (PROT_*).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Prot {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

impl Prot {
    pub const NONE: Prot = Prot { read: false, write: false, exec: false };
    pub fn from_linux(bits: u64) -> Prot {
        Prot { read: bits & 1 != 0, write: bits & 2 != 0, exec: bits & 4 != 0 }
    }
    pub fn rw() -> Prot {
        Prot { read: true, write: true, exec: false }
    }
    /// x86 cannot map write-only or execute-only pages (without protection keys): any right means readable,
    /// as on Linux (`vma_is_accessible`).
    pub fn accessible(self) -> bool {
        self.read || self.write || self.exec
    }
    /// The `rwx` column of `/proc/<pid>/maps`.
    pub fn letters(self) -> [u8; 3] {
        [if self.read { b'r' } else { b'-' }, if self.write { b'w' } else { b'-' }, if self.exec { b'x' } else { b'-' }]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmaKind {
    Anonymous,
    Stack,
    Heap,
    /// ELF segment of the program (copied into anonymous memory at load).
    Image,
    /// A file mapping (FS-5): pages come from the file's page cache (`Vma::file`).
    File,
}

/// What a file-backed VMA maps (FS-5): the mapping object, the file page of `start`, Linux's VM_SHARED and
/// VM_MAYWRITE. MM-5b: a device mapping (GX-D7, fbdev) is a file VMA too: `backing` is its `DeviceMap` (named
/// after the device file in `/proc/<pid>/maps`) and `device` the same object with its fault hook and page types.
#[derive(Clone)]
pub struct FileRef {
    pub backing: Arc<dyn MapBacking>,
    pub pgoff: u64,
    pub shared: bool,
    pub may_write: bool,
    /// Linux's VM_MAYEXEC: false for a file on a noexec mount (`vfs::mmap::Mapped::may_exec`, FS-13 CR2), so
    /// mprotect cannot add PROT_EXEC later (linux-6.6/mm/mprotect.c:789).
    pub may_exec: bool,
    pub device: Option<Arc<DeviceMap>>,
}

impl FileRef {
    /// A plain file mapping (no device), VM_MAYEXEC set (`no_exec` clears it).
    pub fn file(backing: Arc<dyn MapBacking>, pgoff: u64, shared: bool, may_write: bool) -> FileRef {
        FileRef { backing, pgoff, shared, may_write, may_exec: true, device: None }
    }
    /// The same mapping with VM_MAYEXEC as `may_exec`.
    pub fn exec(mut self, may_exec: bool) -> FileRef {
        self.may_exec = may_exec;
        self
    }
}

/// Linux's VMA flags MM-5b needs beyond the rights.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct VmaFlags {
    /// VM_ACCOUNT: the VMA's pages are charged to the commit (`commit`).
    pub account: bool,
    /// VM_DONTCOPY (MADV_DONTFORK): a fork's child does not get it.
    pub dontfork: bool,
    /// VM_WIPEONFORK (MADV_WIPEONFORK): the child gets it empty.
    pub wipeonfork: bool,
}

impl FileRef {
    /// Same mapping object, sharing and rights (merging and splitting keep the object).
    pub fn same_object(&self, o: &FileRef) -> bool {
        Arc::ptr_eq(&self.backing, &o.backing) && self.shared == o.shared && self.may_write == o.may_write && self.may_exec == o.may_exec
    }
}

impl PartialEq for FileRef {
    fn eq(&self, o: &FileRef) -> bool {
        self.same_object(o) && self.pgoff == o.pgoff
    }
}
impl Eq for FileRef {}

impl core::fmt::Debug for FileRef {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "FileRef(pgoff {:#x}, shared {}, may_write {})", self.pgoff, self.shared, self.may_write)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vma {
    pub start: u64,
    pub end: u64,
    pub prot: Prot,
    pub kind: VmaKind,
    /// The file behind a `VmaKind::File` VMA.
    pub file: Option<FileRef>,
    pub flags: VmaFlags,
}

impl Vma {
    fn joins(&self, next: &Vma) -> bool {
        let files = match (&self.file, &next.file) {
            (None, None) => true,
            // Same mapping object and contiguous file pages (Linux's vma_merge for file VMAs).
            (Some(a), Some(b)) => a.same_object(b) && b.pgoff == a.pgoff + (self.end - self.start) / 4096,
            _ => false,
        };
        self.end == next.start && self.prot == next.prot && self.kind == next.kind && self.flags == next.flags && files
    }
    /// An anonymous VMA (no file, no device).
    pub fn anon(start: u64, end: u64, prot: Prot, kind: VmaKind) -> Vma {
        Vma { start, end, prot, kind, file: None, flags: VmaFlags::default() }
    }
    pub fn pages(&self) -> u64 {
        (self.end - self.start) / 4096
    }
    /// Private memory (anonymous or a file's private copies): its frames are the program's own and shared
    /// copy-on-write after a fork. Shared file mappings and device mappings are not.
    pub fn private(&self) -> bool {
        self.file.as_ref().is_none_or(|f| !f.shared && f.device.is_none())
    }
    /// The file page that `addr` (inside the VMA) maps.
    pub fn pgoff_at(&self, addr: u64) -> Option<u64> {
        self.file.as_ref().map(|f| f.pgoff + (addr - self.start) / 4096)
    }
}

/// The heap had no room for the new entries; nothing changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoRoom;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VmaTree {
    v: Vec<Vma>,
    /// Pages of all VMAs (Linux `total_vm`), kept by `insert` and `remove` (`update` never moves an end).
    pages: u64,
    /// The most `pages` ever was (Linux `hiwater_vm`, VmPeak).
    peak: u64,
}

impl VmaTree {
    pub const fn new() -> Self {
        VmaTree { v: Vec::new(), pages: 0, peak: 0 }
    }
    /// Pages of all VMAs (VmSize / 4 KiB), without a walk.
    pub fn pages(&self) -> u64 {
        self.pages
    }
    /// The highest `pages` so far (VmPeak / 4 KiB).
    pub fn peak_pages(&self) -> u64 {
        self.peak.max(self.pages)
    }
    /// Sets the peak back to `peak` (or to the current size if larger): mremap's move counts the moved VMA
    /// twice for a moment, which Linux's `move_vma` hides from `hiwater_vm` in the same way.
    pub fn restore_peak(&mut self, peak: u64) {
        self.peak = peak.max(self.pages);
    }
    pub fn len(&self) -> usize {
        self.v.len()
    }
    pub fn is_empty(&self) -> bool {
        self.v.is_empty()
    }
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Vma> {
        self.v.iter()
    }
    pub fn to_vec(&self) -> Vec<Vma> {
        self.v.clone()
    }

    /// Index of the first VMA that ends above `addr`.
    fn first_ending_above(&self, addr: u64) -> usize {
        self.v.partition_point(|x| x.end <= addr)
    }

    /// The VMA containing `addr`.
    pub fn find(&self, addr: u64) -> Option<&Vma> {
        self.v.get(self.first_ending_above(addr)).filter(|x| x.start <= addr)
    }

    /// The first VMA that ends above `addr` (contains it or lies above it).
    pub fn next_from(&self, addr: u64) -> Option<&Vma> {
        self.v.get(self.first_ending_above(addr))
    }

    /// The VMAs overlapping [start, end).
    pub fn overlapping(&self, start: u64, end: u64) -> &[Vma] {
        let i = self.first_ending_above(start);
        let j = self.v.partition_point(|x| x.start < end);
        if i < j { &self.v[i..j] } else { &[] }
    }

    pub fn is_free(&self, start: u64, end: u64) -> bool {
        self.overlapping(start, end).is_empty()
    }

    /// End of the part of [start, end) that VMAs cover without a hole, starting at `start` (`start` itself
    /// when `start` is not mapped).
    pub fn covered_from(&self, start: u64, end: u64) -> u64 {
        let mut at = start;
        for x in self.overlapping(start, end) {
            if x.start > at {
                break;
            }
            at = x.end.min(end);
        }
        at
    }

    /// Room for `n` more entries, so the next changes cannot fail for lack of memory.
    pub fn reserve(&mut self, n: usize) -> Result<(), NoRoom> {
        self.v.try_reserve(n).map_err(|_| NoRoom)
    }

    /// Adds [start, end) (must be free), merged with equal neighbours.
    pub fn insert(&mut self, vma: Vma) -> Result<(), NoRoom> {
        debug_assert!(vma.start < vma.end && self.is_free(vma.start, vma.end));
        self.reserve(1)?;
        let i = self.first_ending_above(vma.start);
        self.pages += vma.pages();
        self.peak = self.peak.max(self.pages);
        self.v.insert(i, vma);
        self.merge_around(i, i);
        Ok(())
    }

    /// Splits the VMA containing `at` (if `at` lies strictly inside one) into two. Needs room for one entry.
    fn split_at(&mut self, at: u64) {
        let i = self.first_ending_above(at);
        if let Some(x) = self.v.get(i)
            && x.start < at
        {
            let mut right = Vma { start: at, ..x.clone() };
            if let Some(f) = right.file.as_mut() {
                f.pgoff += (at - x.start) / 4096;
            }
            self.v[i].end = at;
            self.v.insert(i + 1, right);
        }
    }

    /// Removes [start, end) from every VMA, splitting the ones it cuts. True if anything was mapped there.
    pub fn remove(&mut self, start: u64, end: u64) -> Result<bool, NoRoom> {
        if self.is_free(start, end) {
            return Ok(false);
        }
        self.reserve(2)?;
        self.split_at(start);
        self.split_at(end);
        let i = self.first_ending_above(start);
        let j = self.v.partition_point(|x| x.start < end);
        for x in self.v.drain(i..j) {
            self.pages -= x.pages();
        }
        Ok(true)
    }

    /// Sets `prot` on [start, end) (which VMAs must cover; see `covered_from`), splitting at both ends and
    /// merging what became equal.
    pub fn protect(&mut self, start: u64, end: u64, prot: Prot) -> Result<(), NoRoom> {
        self.update(start, end, &mut |x| x.prot = prot)
    }

    /// Applies `f` to the VMAs (and parts) inside [start, end), splitting at both ends and merging what became
    /// equal (madvise's flag changes, mprotect).
    pub fn update(&mut self, start: u64, end: u64, f: &mut dyn FnMut(&mut Vma)) -> Result<(), NoRoom> {
        if start >= end {
            return Ok(());
        }
        self.reserve(2)?;
        self.split_at(start);
        self.split_at(end);
        let i = self.first_ending_above(start);
        let j = self.v.partition_point(|x| x.start < end);
        for x in &mut self.v[i..j] {
            let (s, e) = (x.start, x.end);
            f(x);
            debug_assert!(x.start == s && x.end == e, "update must not move a VMA's ends (pages would be wrong)");
        }
        self.merge_around(i, j.saturating_sub(1));
        Ok(())
    }

    /// Merges equal neighbours among entries i-1 ..= j+1.
    fn merge_around(&mut self, i: usize, j: usize) {
        let lo = i.saturating_sub(1);
        let mut k = (j + 1).min(self.v.len().saturating_sub(1));
        while k > lo {
            if self.v[k - 1].joins(&self.v[k]) {
                self.v[k - 1].end = self.v[k].end;
                self.v.remove(k);
            }
            k -= 1;
        }
    }

    /// The highest start of a free range of `len` bytes inside [lo, hi) (Linux's top-down
    /// `unmapped_area_topdown`), or `None`. All three are page multiples.
    pub fn gap_topdown(&self, len: u64, lo: u64, hi: u64) -> Option<u64> {
        if len == 0 || hi < lo {
            return None;
        }
        let fits = |floor: u64, top: u64| top >= floor && top - floor >= len;
        let mut top = hi;
        for x in self.v[..self.v.partition_point(|x| x.start < hi)].iter().rev() {
            if fits(x.end.max(lo), top) {
                return Some(top - len);
            }
            top = top.min(x.start);
            if !fits(lo, top) {
                return None;
            }
        }
        fits(lo, top).then(|| top - len)
    }

    /// The lowest start of a free range of `len` bytes inside [lo, hi) (for `MAP_32BIT`).
    pub fn gap_bottomup(&self, len: u64, lo: u64, hi: u64) -> Option<u64> {
        if len == 0 || hi < lo {
            return None;
        }
        let mut at = lo;
        for x in self.overlapping(lo, hi) {
            if x.start >= at && x.start - at >= len {
                return Some(at);
            }
            at = at.max(x.end);
        }
        (hi >= at && hi - at >= len).then_some(at)
    }
}

#[cfg(test)]
#[path = "vma_tests.rs"]
mod tests;
