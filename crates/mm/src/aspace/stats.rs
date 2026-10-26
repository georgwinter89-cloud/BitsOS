//! Per-process memory numbers (MM-5b, MM-ctr): what `/proc/<pid>/status`, `statm`, `smaps`, `smaps_rollup`
//! and `ps` show.
//!
//! `mem_stats` reads kept numbers (`counters.rs`, the VMA tree's size and peak): no walk, so a `ps` over every
//! process stays cheap. `page_usage` (smaps) walks the present pages of each VMA as Linux's `smap_gather_stats`
//! does (fs/proc/task_mmu.c:768): resident, Referenced (the accessed bit), dirty (the dirty bit; a program's own
//! frame always counts dirty, as Linux's anonymous pages are dirty from their first write fault and Linux shows
//! read-only touches as the shared zero page, which it does not count at all) and Pss from the number of spaces
//! that map the frame (`share::others` + 1 for a program's own frame, `share::file_maps` for a page-cache frame;
//! Linux `page_mapcount`). Device pages count neither in VmRSS nor in smaps (Linux's VM_PFNMAP pages).

use super::counters::Class;
use super::AsCore;
use crate::vma::Vma;
use crate::{share, MmError, PAGE};
use alloc::vec::Vec;
use hal::paging::{self, PageFlags};

/// Pss values are bytes shifted left by this (Linux `PSS_SHIFT`): `pss >> PSS_SHIFT` bytes, `>> 10` more for kB.
pub const PSS_SHIFT: u32 = 12;

/// One address space's numbers (pages unless named otherwise).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemStats {
    /// All VMAs together (VmSize; Linux `total_vm`).
    pub size: u64,
    /// The most `size` ever was (VmPeak; Linux `hiwater_vm`).
    pub peak: u64,
    /// VmRSS = anon + file + shmem.
    pub rss: u64,
    /// The most `rss` ever was (VmHWM; Linux `hiwater_rss`).
    pub hiwater_rss: u64,
    /// Present pages of every kind (`rss` + `device`; `resident_pages`).
    pub resident: u64,
    /// The program's own frames (anonymous memory and private copies; RssAnon).
    pub anon: u64,
    /// Page-cache frames of file mappings (RssFile).
    pub file: u64,
    /// Page-cache frames of shared anonymous memory (RssShmem; tmpfs files mapped by name count as file).
    pub shmem: u64,
    /// Device pages (fbdev, GPU apertures): not in VmRSS.
    pub device: u64,
    /// Pages held of the system's commit (VM_ACCOUNT mappings).
    pub committed: u64,
    /// This process's own commit limit, if any.
    pub commit_limit: Option<u64>,
    pub vmas: u64,
    /// Page tables of the user half (PDPT, PD, PT; VmPTE = `page_table_bytes` / 1024 kB).
    pub page_tables: u64,
}

impl MemStats {
    /// Linux `mm_pgtables_bytes` (VmPTE in bytes).
    pub fn page_table_bytes(&self) -> u64 {
        self.page_tables * PAGE
    }
}

/// smaps numbers of one VMA or of several summed (smaps_rollup). Sizes in bytes, Pss values in bytes <<
/// `PSS_SHIFT` (Linux `struct mem_size_stats`; fields V2 never has, as Swap, Locked or huge pages, are 0 there).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageUsage {
    /// Rss.
    pub resident: u64,
    pub pss: u64,
    pub pss_dirty: u64,
    pub pss_anon: u64,
    pub pss_file: u64,
    pub pss_shmem: u64,
    pub shared_clean: u64,
    pub shared_dirty: u64,
    pub private_clean: u64,
    pub private_dirty: u64,
    pub referenced: u64,
    /// Anonymous: the program's own frames.
    pub anonymous: u64,
}

impl PageUsage {
    pub fn add(&mut self, o: &PageUsage) {
        self.resident += o.resident;
        self.pss += o.pss;
        self.pss_dirty += o.pss_dirty;
        self.pss_anon += o.pss_anon;
        self.pss_file += o.pss_file;
        self.pss_shmem += o.pss_shmem;
        self.shared_clean += o.shared_clean;
        self.shared_dirty += o.shared_dirty;
        self.private_clean += o.private_clean;
        self.private_dirty += o.private_dirty;
        self.referenced += o.referenced;
        self.anonymous += o.anonymous;
    }

    /// One present 4 KiB page of class `c`, mapped by `maps` spaces (Linux `smaps_account` and
    /// `smaps_page_accumulate`: private when one space maps it, Pss = size / mapcount).
    pub(crate) fn account(&mut self, c: Class, maps: u64, young: bool, dirty: bool) {
        let size = PAGE;
        let pss = if maps >= 2 { (size << PSS_SHIFT) / maps } else { size << PSS_SHIFT };
        self.resident += size;
        self.referenced += if young { size } else { 0 };
        self.pss += pss;
        match c {
            Class::Anon => {
                self.anonymous += size;
                self.pss_anon += pss;
            }
            Class::Shmem => self.pss_shmem += pss,
            _ => self.pss_file += pss,
        }
        let field = match (dirty, maps < 2) {
            (true, true) => &mut self.private_dirty,
            (true, false) => &mut self.shared_dirty,
            (false, true) => &mut self.private_clean,
            (false, false) => &mut self.shared_clean,
        };
        *field += size;
        if dirty {
            self.pss_dirty += pss;
        }
    }
}

/// Address range walked per hold of the space's lock by the smaps readers: at most 8192 leaves, so a reader of
/// a huge mapping (another process reading /proc/<pid>/smaps) stalls the program's faults, mmaps and munmaps
/// for one such piece at a time, never for the whole mapping (B4). Linux's smaps holds the mmap lock only for
/// reading and drops it when contended (fs/proc/task_mmu.c:877 show_smaps_rollup); V2's lock is a mutex.
const USAGE_STEP: u64 = 32 << 20;

/// The smaps numbers of the present pages of `v` in [lo, hi) of `root`.
fn vma_usage(root: u64, v: &Vma, lo: u64, hi: u64) -> PageUsage {
    let mut u = PageUsage::default();
    if v.file.as_ref().is_some_and(|f| f.device.is_some()) {
        return u;
    }
    paging::for_each_leaf(root, lo, hi, &mut |l| {
        let c = Class::in_vma(l.phys, Some(v));
        let (maps, dirty) = match c {
            Class::Anon => (1 + share::others(l.phys) as u64, true),
            _ => (share::file_maps(l.phys) as u64, l.flags.contains(PageFlags::DIRTY)),
        };
        u.account(c, maps, l.flags.contains(PageFlags::ACCESSED), dirty);
    });
    u
}

impl AsCore {
    /// The numbers now, from the kept counters (no page walk).
    pub fn mem_stats(&self) -> MemStats {
        let g = self.inner.lock();
        let c = &self.ctr;
        MemStats {
            size: g.space.tree.pages(),
            peak: g.space.tree.peak_pages(),
            rss: c.rss(),
            hiwater_rss: c.hiwater_rss(),
            resident: c.present(),
            anon: c.get(Class::Anon),
            file: c.get(Class::File),
            shmem: c.get(Class::Shmem),
            device: c.get(Class::Device),
            committed: g.space.committed,
            commit_limit: g.limit,
            vmas: g.space.tree.len() as u64,
            page_tables: c.tables(),
        }
    }

    /// VmRSS in pages, without the lock.
    pub fn rss_pages(&self) -> u64 {
        self.ctr.rss()
    }

    /// VmHWM in pages, without the lock.
    pub fn hiwater_rss_pages(&self) -> u64 {
        self.ctr.hiwater_rss()
    }

    /// ktests: the numbers `mem_stats` keeps, counted again by walking the page tables (sizes from the VMAs);
    /// the high-water marks are copied from the kept ones (a walk cannot know them).
    #[doc(hidden)]
    pub fn mem_stats_walked(&self) -> MemStats {
        let g = self.inner.lock();
        let tree = &g.space.tree;
        let mut s = MemStats {
            size: tree.iter().map(|v| v.pages()).sum(),
            peak: tree.peak_pages(),
            hiwater_rss: self.ctr.hiwater_rss(),
            committed: g.space.committed,
            commit_limit: g.limit,
            vmas: tree.len() as u64,
            page_tables: paging::count_user_tables(self.root),
            ..MemStats::default()
        };
        paging::for_each_leaf(self.root, 0, hal::USER_TOP, &mut |u| {
            s.resident += 1;
            match Class::in_vma(u.phys, tree.find(u.virt)) {
                Class::Anon => s.anon += 1,
                Class::File => s.file += 1,
                Class::Shmem => s.shmem += 1,
                Class::Device => s.device += 1,
            }
        });
        s.rss = s.anon + s.file + s.shmem;
        s
    }

    /// ktests: how often a count of this space would have gone below zero (0 unless a path is wrong).
    #[doc(hidden)]
    pub fn counter_underflows(&self) -> u64 {
        self.ctr.underflows()
    }

    /// Walks the VMAs in pieces of at most `USAGE_STEP`, taking the lock per piece; `each(vma, first, usage)`
    /// gets every piece, `first` for the first piece of a VMA. A VMA that changes between pieces (another
    /// thread's munmap or mprotect) is shown as the reader met it first, its later pieces as they are then,
    /// like a smaps read that spans several reads on Linux.
    fn usage_pieces(&self, each: &mut dyn FnMut(&Vma, bool, &PageUsage) -> Result<(), MmError>) -> Result<(), MmError> {
        let (mut addr, mut cur_end) = (0u64, 0u64);
        loop {
            let g = self.inner.lock();
            let Some(v) = g.space.tree.next_from(addr) else { return Ok(()) };
            let lo = addr.max(v.start);
            let hi = v.end.min(lo.saturating_add(USAGE_STEP));
            let first = v.start >= cur_end;
            if first {
                cur_end = v.end;
            }
            each(v, first, &vma_usage(self.root, v, lo, hi))?;
            drop(g);
            addr = hi;
        }
    }

    /// smaps: every VMA with the numbers of its present pages (walked in pieces, see `usage_pieces`).
    pub fn page_usage(&self) -> Result<Vec<(Vma, PageUsage)>, MmError> {
        let mut out: Vec<(Vma, PageUsage)> = Vec::new();
        self.usage_pieces(&mut |v, first, u| {
            match out.last_mut() {
                Some(last) if !first => last.1.add(u),
                _ => {
                    out.try_reserve(1).map_err(|_| MmError::NoMemory)?;
                    out.push((v.clone(), *u));
                }
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// smaps_rollup: the numbers of all VMAs summed (no list built).
    pub fn page_usage_total(&self) -> PageUsage {
        let mut t = PageUsage::default();
        let _ = self.usage_pieces(&mut |_, _, u| {
            t.add(u);
            Ok(())
        });
        t
    }
}

#[cfg(test)]
mod tests {
    use super::{Class, PageUsage, PAGE, PSS_SHIFT};

    #[test]
    fn pss_and_sharing_follow_linux_smaps_account() {
        let mut u = PageUsage::default();
        u.account(Class::Anon, 1, true, true);
        u.account(Class::Anon, 3, false, true);
        u.account(Class::File, 2, true, false);
        u.account(Class::Shmem, 1, false, false);
        let full = PAGE << PSS_SHIFT;
        assert_eq!(u.resident, 4 * PAGE);
        assert_eq!(u.pss, full + full / 3 + full / 2 + full, "Pss = size / mapcount per page");
        assert_eq!((u.pss_anon, u.pss_file, u.pss_shmem), (full + full / 3, full / 2, full));
        assert_eq!(u.pss_dirty, full + full / 3);
        assert_eq!((u.private_dirty, u.shared_dirty, u.private_clean, u.shared_clean), (PAGE, PAGE, PAGE, PAGE));
        assert_eq!((u.referenced, u.anonymous), (2 * PAGE, 2 * PAGE));
        let mut t = PageUsage::default();
        t.add(&u);
        t.add(&u);
        assert_eq!(t.pss, 2 * u.pss, "rollup sums");
    }
}
