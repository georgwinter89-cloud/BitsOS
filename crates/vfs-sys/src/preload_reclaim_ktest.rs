//! FS-13b ktest `fs.preload.reclaim` (QemuOnly: it fills the machine's memory; skips above 2.5 GiB RAM;
//! host/tests/fs/53-preload-reclaim.sh runs it with 512M). A read file (its pages used), then a preload pass with
//! the service's floor (`memory_floor`) over a file larger than free memory: the pass fills the cache until free
//! memory reaches the floor (`Stop::Memory`). Then program memory beyond what is free (an allocation spike):
//! every page arrives, reclaim takes the preloaded pages nobody used first (`vfs.reclaim.preloaded_pages`), the
//! read file and the preloaded pages a reader used stay cached, the reclaimed pages read right again, and a
//! pass budget made before the spike stops with `Stop::Reclaim` at its next read.

use crate::preload::{self, Budget, Stop};
use alloc::format;
use alloc::sync::Arc;
use ktest::{TestResult, kassert, ktest};
use mm::{AddressSpace, Fill, Prot};
use vfs::pagecache::{Backend, CachePage, PageCache};
use vfs::preload::Ranges;
use vfs::reclaim as vr;
use vfs::Errno;

const P: u64 = 4096;
const MIB: u64 = 1 << 20;
const MAGIC: u64 = 0x13b0_0000_0000;

/// A device that is not there (it costs no memory): every page holds its number at both ends.
struct Pat;

impl Backend for Pat {
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), Errno> {
        let v = pgoff.to_le_bytes();
        page.write(0, &v);
        page.write(P as usize - 8, &v);
        Ok(())
    }
    fn write_run(&self, _first: u64, _pages: &[Arc<CachePage>], _size: u64) -> Result<(), Errno> {
        Ok(())
    }
    fn read_window(&self) -> u64 {
        vfs::pagecache::MAX_WINDOW
    }
}

fn file(pages: u64) -> PageCache {
    PageCache::with_backend(Arc::new(Pat), pages * P)
}

fn check(c: &PageCache, pages: u64) -> TestResult {
    for i in 0..pages {
        let (mut a, mut z) = ([0u8; 8], [0u8; 8]);
        c.read_at(i * P, &mut a).map_err(|e| format!("read page {i}: {e}"))?;
        c.read_at(i * P + P - 8, &mut z).map_err(|e| format!("read page {i}: {e}"))?;
        kassert!(u64::from_le_bytes(a) == i && u64::from_le_bytes(z) == i, "page {i} reads {:#x}/{:#x}", u64::from_le_bytes(a), u64::from_le_bytes(z));
    }
    Ok(())
}

fn all(pages: u64) -> Ranges {
    let mut r = Ranges::new();
    r.add(0, pages);
    r
}

fn free_pages() -> u64 {
    mm_core::frames().map_or(0, |f| f.stats().free_bytes / P)
}

/// The pages program memory may take beyond the file (strict commit: the room left under the limit).
fn commit_room() -> u64 {
    mm::commit::limit().saturating_sub(mm::commit::committed()).saturating_sub(8 * MIB / P)
}

fn preload_reclaim() -> TestResult {
    let total = mm_core::frames().map_or(0, |f| f.stats().total_bytes);
    if total == 0 || total > 5 * (512 * MIB) {
        return ktest::skip("fills all memory: small RAM only (53-preload-reclaim.sh, the 2G gate)");
    }
    let floor = preload::memory_floor();
    if free_pages() * P < floor + 64 * MIB {
        return ktest::skip("too little free memory above the preload floor");
    }
    let used_n = 16 * MIB / P;
    let used = file(used_n);
    check(&used, used_n)?;
    // The preload's file: larger than free memory, so the floor ends the pass.
    let free0 = free_pages();
    let big_n = free0 + 4096;
    let pre = file(big_n);
    let mut b = Budget::new(u64::MAX, floor, false);
    preload::prefetch_ranges(&pre, &all(big_n), &mut b).map_err(|e| format!("prefetch: {e}"))?;
    let free1 = free_pages();
    kassert!(b.stopped == Some(Stop::Memory), "stopped {:?} after {} pages", b.stopped, b.pages);
    kassert!(pre.pages() == b.pages, "cached {} vs counted {}", pre.pages(), b.pages);
    // Before every read free memory was above the floor (one read brings at most a window; slack for the
    // kernel's own allocations meanwhile).
    kassert!(free1 * P + 8 * MIB >= floor, "free {} MiB, floor {} MiB", free1 * P / MIB, floor / MIB);
    // The cache's own bookkeeping (index, page structs) takes frames too: a sixteenth as slack.
    let room = free0.saturating_sub(floor / P);
    kassert!(b.pages + room / 16 + 8 * MIB / P >= room, "preloaded {} MiB of the {} MiB above the floor", b.pages * P / MIB, room * P / MIB);
    // Programs read some preloaded pages, spread over the file (page 0 and every `stride`-th): they are no longer
    // never-used ones and must outlast the never-used ones (fixer F8).
    let stride = (b.pages / 1024).max(2);
    let touched: alloc::vec::Vec<u64> = (0..b.pages).step_by(stride as usize).collect();
    let mut v = [0u8; 8];
    for &i in &touched {
        pre.read_at(i * P, &mut v).map_err(|e| format!("touch page {i}: {e}"))?;
    }
    let before_spike = Budget::new(u64::MAX, 0, false);
    let (s0, d0) = (vr::stats(), mm_core::reclaim::direct_stats());
    // The spike: what is free plus half of the preloaded pages.
    let n = (free_pages() + b.pages / 2).min(commit_room());
    if n < free_pages() + 4 * MIB / P {
        return ktest::skip("the commit limit leaves no room past the free memory");
    }
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(|e| format!("address space: {e:?}"))?;
    let m = a.map_anywhere(n * P, Prot::rw()).map_err(|e| format!("mmap of {} MiB: {e:?}", n * P / MIB))?;
    for i in 0..n {
        a.write_u64(m + i * P, MAGIC | i).map_err(|e| format!("program page {i} of {n}: {e:?} (free {} pages, cache {:?})", free_pages(), vr::stats()))?;
    }
    let (s1, d1) = (vr::stats(), mm_core::reclaim::direct_stats());
    let (took, pre_took) = (s1.pages - s0.pages, s1.preloaded_pages - s0.preloaded_pages);
    klog::event!(Info, "vfs", "ktest.preload_reclaim", total_mb = total / MIB, floor_mb = floor / MIB, preloaded_mb = b.pages * P / MIB, free_mb = free1 * P / MIB,
        program_mb = n * P / MIB, reclaimed = took, preloaded_reclaimed = pre_took, direct_runs = d1.runs - d0.runs, cache_after_mb = pre.pages() * P / MIB);
    kassert!(d1.failed == d0.failed, "{} allocations failed although preloaded pages could go", d1.failed - d0.failed);
    // Preloaded first: a direct call that finds the preloaded cache's lock taken (kswapd is in it) goes on with
    // the clock over the other caches (MM-9: no waiting), so a few other pages may go; the first QEMU run took 32
    // of 76455 that way.
    let other = took - pre_took;
    kassert!(pre_took > 0 && other <= took / 100 + 256, "reclaimed {took} pages, only {pre_took} of them preloaded ones");
    kassert!(used.pages() + other >= used_n, "the read file lost {} pages, more than the {other} not preloaded ones reclaimed", used_n - used.pages());
    // The spike took half of the preloaded pages: the never-used ones suffice, so a used one may go only by the
    // clock fallback above (counted in `other`).
    let lost = touched.iter().filter(|&&i| pre.cached(i).is_none()).count() as u64;
    let untouched_left = pre.pages().saturating_sub(touched.len() as u64 - lost);
    klog::event!(Info, "vfs", "ktest.preload_reclaim_used", touched = touched.len() as u64, lost = lost, untouched_left = untouched_left);
    kassert!(lost <= other, "{lost} of {} used preloaded pages went, more than the {other} pages the clock took", touched.len());
    kassert!(pre.cached(0).is_some() || other > 0, "the used preloaded page 0 went before never-used ones");
    for i in 0..n {
        let got = a.read_u64(m + i * P).map_err(|e| format!("read back page {i}: {e:?}"))?;
        kassert!(got == MAGIC | i, "program page {i} reads {got:#x}");
    }
    drop(a);
    let mut late = before_spike;
    preload::prefetch_ranges(&pre, &all(big_n), &mut late).map_err(|e| format!("prefetch: {e}"))?;
    kassert!(late.stopped == Some(Stop::Reclaim) && late.pages == 0, "a pass after reclaim: {:?}, {} pages", late.stopped, late.pages);
    check(&pre, 4096.min(big_n))?;
    check(&used, used_n)
}
ktest!("fs.preload.reclaim", QemuOnly, preload_reclaim);
