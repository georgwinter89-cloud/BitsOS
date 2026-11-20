//! L7-VFS: guard regions (MADV_GUARD_INSTALL / MADV_GUARD_REMOVE, linux-7.3.1/mm/madvise.c:1124, :1252).
//!
//! Linux keeps a guard as a PTE marker (PTE_MARKER_GUARD) in the page table: a touch is SIGSEGV (`handle_pte_marker`,
//! memory.c:4736, SEGV_MAPERR), a kernel copy EFAULT; zapping (DONTNEED, FREE, reclaim, truncation) keeps it, munmap
//! and MADV_GUARD_REMOVE take it, mremap moves it with the page tables, fork copies it (VM_MAYBE_GUARD is in
//! VM_COPY_ON_FORK; a WIPEONFORK VMA's tables are not copied). V2 keeps the same thing beside the page tables: the
//! guarded pages of the space, by address, as sorted disjoint ranges. A guarded page is never mapped (installing
//! zaps it first), so every access faults, and every place that fills a page asks here first.

use alloc::vec::Vec;

/// The guarded page ranges [start, end) of one address space: sorted, disjoint and never touching.
#[derive(Default)]
pub(crate) struct Guards(Vec<(u64, u64)>);

impl Guards {
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The first range that ends after `va`.
    fn first_after(&self, va: u64) -> usize {
        self.0.partition_point(|&(_, e)| e <= va)
    }

    pub(crate) fn contains(&self, va: u64) -> bool {
        self.0.get(self.first_after(va)).is_some_and(|&(s, _)| s <= va)
    }

    /// Whether any page of [s, e) is guarded.
    pub(crate) fn any_in(&self, s: u64, e: u64) -> bool {
        self.0.get(self.first_after(s)).is_some_and(|&(gs, _)| gs < e)
    }

    /// The guarded pieces of [s, e).
    fn pieces(&self, s: u64, e: u64) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.0[self.first_after(s)..].iter().take_while(move |&&(gs, _)| gs < e).map(move |&(gs, ge)| (gs.max(s), ge.min(e)))
    }

    /// Guards [s, e). Err: no memory for the list (nothing changed).
    pub(crate) fn add(&mut self, s: u64, e: u64) -> Result<(), ()> {
        self.0.try_reserve(1).map_err(|_| ())?;
        // Every range touching or overlapping [s, e) merges into one.
        let lo = self.0.partition_point(|&(_, ge)| ge < s);
        let hi = self.0.partition_point(|&(gs, _)| gs <= e);
        let (mut ns, mut ne) = (s, e);
        if lo < hi {
            ns = ns.min(self.0[lo].0);
            ne = ne.max(self.0[hi - 1].1);
        }
        self.0.splice(lo..hi, core::iter::once((ns, ne)));
        Ok(())
    }

    /// Room for one `remove` that cuts a range in two, so that it cannot fail.
    pub(crate) fn reserve_cut(&mut self) -> Result<(), ()> {
        if self.0.is_empty() { Ok(()) } else { self.0.try_reserve(1).map_err(|_| ()) }
    }

    /// No page of [s, e) is guarded any more. Needs at most one more slot (`reserve_cut`).
    pub(crate) fn remove(&mut self, s: u64, e: u64) {
        let lo = self.first_after(s);
        let hi = self.0.partition_point(|&(gs, _)| gs < e);
        if lo >= hi {
            return;
        }
        let (first, last) = (self.0[lo], self.0[hi - 1]);
        let mut keep: [(u64, u64); 2] = [(0, 0); 2];
        let mut n = 0;
        if first.0 < s {
            keep[n] = (first.0, s);
            n += 1;
        }
        if last.1 > e {
            keep[n] = (e, last.1);
            n += 1;
        }
        self.0.splice(lo..hi, keep[..n].iter().copied());
    }

    /// The guards of a fork's child: those inside `kept` (start, end) VMAs.
    pub(crate) fn for_child(&self, kept: impl Iterator<Item = (u64, u64)>) -> Result<Guards, ()> {
        let mut c = Guards::default();
        if self.0.is_empty() {
            return Ok(c);
        }
        for (s, e) in kept {
            for (ps, pe) in self.pieces(s, e) {
                c.add(ps, pe)?;
            }
        }
        Ok(c)
    }

    /// mremap's first half: the guards of [old, old + len), with room to move them (Err: no memory).
    pub(crate) fn plan_move(&mut self, old: u64, len: u64) -> Result<Vec<(u64, u64)>, ()> {
        let mut v = Vec::new();
        if self.0.is_empty() {
            return Ok(v);
        }
        let n = self.pieces(old, old + len).count();
        v.try_reserve_exact(n).map_err(|_| ())?;
        v.extend(self.pieces(old, old + len));
        self.0.try_reserve(n + 1).map_err(|_| ())?;
        Ok(v)
    }

    /// mremap's second half, after the pages moved: the planned guards leave [old, old + len) for `to`.
    pub(crate) fn apply_move(&mut self, pieces: &[(u64, u64)], old: u64, len: u64, to: u64) {
        if pieces.is_empty() {
            return;
        }
        self.remove(old, old + len);
        for &(s, e) in pieces {
            // Reserved by `plan_move`: cannot fail.
            let _ = self.add(to + (s - old), to + (e - old));
        }
    }

    #[cfg(test)]
    fn ranges(&self) -> &[(u64, u64)] {
        &self.0
    }
}

/// /proc/<pid>/pagemap bits (linux-7.3.1/fs/proc/task_mmu.c:1952). No soft-dirty (bit 55) or uffd-wp (57):
/// V2 tracks neither, as a kernel without CONFIG_MEM_SOFT_DIRTY and userfaultfd write protection.
const PM_MMAP_EXCLUSIVE: u64 = 1 << 56;
const PM_GUARD_REGION: u64 = 1 << 58;
const PM_FILE: u64 = 1 << 61;
const PM_SWAP: u64 = 1 << 62;
const PM_PRESENT: u64 = 1 << 63;
/// A guard marker's frame field for a privileged reader: `swp_type | offset << MAX_SWAPFILES_SHIFT` of
/// `make_guard_swp_entry` (SWP_PTE_MARKER = 31, PTE_MARKER_GUARD = 4; include/linux/swap.h:56, swapops.h:300).
const GUARD_FRAME: u64 = 31 | (4 << 5);

impl super::AsCore {
    /// L7-RESTE: the pagemap entries of the pages [start, end) (`pte_to_pagemap_entry`, task_mmu.c:2018): 0 for
    /// no page (in a VMA or not), PM_SWAP | PM_GUARD_REGION for a guard, PM_PRESENT for a mapped frame with
    /// PM_FILE when it is not the program's own and PM_MMAP_EXCLUSIVE when no other space maps it. A device
    /// mapping (VM_PFNMAP) is all 0: pagemap_ops has no test_walk, so `walk_page_test` (mm/pagewalk.c:396) hands
    /// such a VMA to `pagemap_pte_hole` (task_mmu.c:1984) and its frames are never shown. Frame numbers only with
    /// `show_pfn`. One hold of the space's lock: callers keep the range short (pagemap reads 2 MiB at a time,
    /// PAGEMAP_WALK_SIZE).
    pub fn pagemap(&self, start: u64, end: u64, show_pfn: bool, out: &mut Vec<u64>) -> Result<(), crate::MmError> {
        use super::counters::Class;
        use hal::paging::{self, PageSize};
        let n = (end.saturating_sub(start) / crate::PAGE) as usize;
        out.try_reserve(n).map_err(|_| crate::MmError::NoMemory)?;
        let g = self.inner.lock();
        for i in 0..n as u64 {
            let va = start + i * crate::PAGE;
            let Some(v) = g.space.tree.find(va) else {
                out.push(0);
                continue;
            };
            if v.file.as_ref().is_some_and(|f| f.device.is_some()) {
                out.push(0);
                continue;
            }
            if g.guards.contains(va) {
                out.push(PM_SWAP | PM_GUARD_REGION | if show_pfn { GUARD_FRAME } else { 0 });
                continue;
            }
            let Some(leaf) = paging::translate_leaf(self.root, va) else {
                out.push(0);
                continue;
            };
            let phys = leaf.phys & !(crate::PAGE - 1);
            let mut e = PM_PRESENT | if show_pfn { phys >> 12 } else { 0 };
            if leaf.size == PageSize::Size2M {
                // MM-13: a 2 MiB leaf is the program's own memory, one block (one holder) or 512 counted frames.
                let head = leaf.phys & !(super::huge::HUGE - 1);
                if mm_core::is_user_block(head, mm_core::HUGE_ORDER) || crate::share::others(phys) == 0 {
                    e |= PM_MMAP_EXCLUSIVE;
                }
            } else {
                match Class::in_vma(phys, Some(v)) {
                    Class::Anon if crate::share::others(phys) == 0 => e |= PM_MMAP_EXCLUSIVE,
                    Class::Anon => {}
                    _ if crate::share::file_maps(phys) == 1 => e |= PM_FILE | PM_MMAP_EXCLUSIVE,
                    _ => e |= PM_FILE,
                }
            }
            out.push(e);
        }
        Ok(())
    }

    /// L7-RESTE: whether [start, end) holds a guard (smaps' "gu", VM_MAYBE_GUARD, while one is installed).
    pub fn has_guard(&self, start: u64, end: u64) -> bool {
        self.inner.lock().guards.any_in(start, end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_merge_cut_and_answer() {
        let mut g = Guards::default();
        assert!(g.is_empty() && !g.contains(0x1000));
        g.add(0x3000, 0x5000).unwrap();
        g.add(0x8000, 0x9000).unwrap();
        g.add(0x5000, 0x6000).unwrap();
        assert_eq!(g.ranges(), &[(0x3000, 0x6000), (0x8000, 0x9000)], "touching ranges merge");
        assert!(g.contains(0x3000) && g.contains(0x5fff) && !g.contains(0x6000) && !g.contains(0x2fff));
        assert!(g.any_in(0x6000, 0x8001) && !g.any_in(0x6000, 0x8000) && g.any_in(0, 0x3001));
        g.add(0x4000, 0x8800).unwrap();
        assert_eq!(g.ranges(), &[(0x3000, 0x9000)], "an overlap swallows both");
        g.reserve_cut().unwrap();
        g.remove(0x4000, 0x5000);
        assert_eq!(g.ranges(), &[(0x3000, 0x4000), (0x5000, 0x9000)], "a cut in the middle");
        g.remove(0x0, 0x6000);
        assert_eq!(g.ranges(), &[(0x6000, 0x9000)]);
        g.remove(0x8000, 0x10000);
        assert_eq!(g.ranges(), &[(0x6000, 0x8000)]);
        g.remove(0x6000, 0x8000);
        assert!(g.is_empty());
    }

    #[test]
    fn fork_and_mremap_carry_them() {
        let mut g = Guards::default();
        g.add(0x1000, 0x3000).unwrap();
        g.add(0x10000, 0x12000).unwrap();
        let c = g.for_child([(0x2000, 0x11000)].into_iter()).unwrap();
        assert_eq!(c.ranges(), &[(0x2000, 0x3000), (0x10000, 0x11000)], "only inside the kept VMAs");
        let p = g.plan_move(0x0, 0x4000).unwrap();
        g.apply_move(&p, 0x0, 0x4000, 0x40000);
        assert_eq!(g.ranges(), &[(0x10000, 0x12000), (0x41000, 0x43000)]);
        let p = g.plan_move(0x20000, 0x1000).unwrap();
        assert!(p.is_empty());
    }
}
