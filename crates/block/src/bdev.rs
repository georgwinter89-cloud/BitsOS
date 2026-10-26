//! A whole disk as a page-cache file (Linux's block-device mapping, `fs/block_dev.c`/`block/fops.c`): FS-5's
//! `PageCache` with this `Backend` reads missing pages from the disk and writes dirty runs back as one
//! scatter-gather request each, straight from the cached pages. FAT (FS-7) and ext4 (FS-8) give their files
//! backends of the same shape (file page -> disk blocks).
//!
//! Page `n` is the disk's bytes `[n x 4096, (n + 1) x 4096)`; a last page past the disk end holds only the
//! blocks inside (the rest reads zero and is never written). Write-back runs as Background unless a caller
//! waits for it (fsync, msync: Normal); a durable sync ends with a device flush (`commit`). The priority comes
//! from counting the write-backs under way (not one shared flag): while any durable one runs, every run goes
//! as Normal, so a background write-back that begins in the middle of an fsync never demotes the fsync's
//! writes (FS-5's `sync_lock` serialises one cache's write-backs, but a `DiskFile` may back several).
//! FS-12: a page a thread misses is read at that thread's priority (`prio::current`); FS-5's read-ahead runs in
//! the Background reader thread (`vfs::readahead`), so its requests go as Background by the same rule. A
//! durable write-back an Interactive thread waits for goes as Interactive (never below Normal).

use crate::disk::{Disk, PageSeg};
use crate::error::BlockError;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use devmodel::block::IoPriority;
use vfs::pagecache::{Backend, CachePage, PageCache, SyncKind};

const PAGE: u64 = 4096;

pub struct DiskFile {
    disk: Arc<Disk>,
    /// Background write-backs under way (between `begin` and `commit`).
    background: AtomicU32,
    /// Durable write-backs under way (fsync, fdatasync, msync): their runs go as Normal.
    durable: AtomicU32,
}

impl DiskFile {
    pub fn new(disk: Arc<Disk>) -> Arc<DiskFile> {
        Arc::new(DiskFile { disk, background: AtomicU32::new(0), durable: AtomicU32::new(0) })
    }

    /// The disk's size in bytes.
    pub fn size(&self) -> u64 {
        self.disk.dev_info().blocks.saturating_mul(self.disk.block_size() as u64)
    }

    /// A page cache over the whole disk.
    pub fn page_cache(disk: Arc<Disk>) -> PageCache {
        let f = DiskFile::new(disk);
        let size = f.size();
        PageCache::for_block_device(f, size)
    }

    fn count(&self, why: SyncKind) -> &AtomicU32 {
        if why.durable() { &self.durable } else { &self.background }
    }

    /// The priority of a write-back run now: Background only while background write-backs alone are under
    /// way; Normal while any durable one is (and outside begin/commit), Interactive when the calling thread's
    /// own priority is (FS-12: an fsync of the program in front).
    pub fn run_priority(&self) -> IoPriority {
        if self.durable.load(Ordering::Acquire) == 0 && self.background.load(Ordering::Acquire) > 0 {
            IoPriority::Background
        } else {
            crate::prio::higher(IoPriority::Normal, crate::prio::current())
        }
    }

    /// The bytes of page `pgoff` that lie on the disk (whole blocks), from its first block.
    fn span(&self, pgoff: u64) -> Option<(u64, usize)> {
        let bs = self.disk.block_size() as u64;
        let start = pgoff.checked_mul(PAGE)?;
        let size = self.size();
        if start >= size {
            return None;
        }
        let len = (size - start).min(PAGE) / bs * bs;
        (len > 0).then_some((start / bs, len as usize))
    }
}

fn errno(e: BlockError) -> i64 {
    e.errno()
}

impl Backend for DiskFile {
    fn read_ahead_pages(&self) -> u64 {
        vfs::readahead::DEFAULT_PAGES
    }

    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), i64> {
        let Some((lba, len)) = self.span(pgoff) else { return Ok(()) };
        self.disk.read_pages(lba, alloc::vec![PageSeg { page: page.clone(), off: 0, len }], crate::prio::current()).map_err(errno)
    }

    /// FS-14b: a run of pages in one request (the read-ahead job and large reads), scattered into the pages.
    fn read_pages(&self, first: u64, pages: &[Arc<CachePage>]) -> Result<(), i64> {
        let Some((lba, _)) = self.span(first) else { return Ok(()) };
        let mut segs: Vec<PageSeg> = Vec::new();
        segs.try_reserve_exact(pages.len()).map_err(|_| uapi::errno::ENOMEM)?;
        for (i, p) in pages.iter().enumerate() {
            match self.span(first + i as u64) {
                Some((_, len)) => segs.push(PageSeg { page: p.clone(), off: 0, len }),
                None => break,
            }
        }
        if segs.is_empty() {
            return Ok(());
        }
        self.disk.read_pages(lba, segs, crate::prio::current()).map_err(errno)
    }

    fn begin(&self, why: SyncKind) -> Result<(), i64> {
        self.count(why).fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn write_run(&self, first: u64, pages: &[Arc<CachePage>], _size: u64) -> Result<(), i64> {
        let Some((lba, _)) = self.span(first) else { return Ok(()) };
        let mut segs: Vec<PageSeg> = Vec::new();
        segs.try_reserve_exact(pages.len()).map_err(|_| uapi::errno::ENOMEM)?;
        for (i, p) in pages.iter().enumerate() {
            match self.span(first + i as u64) {
                Some((_, len)) => segs.push(PageSeg { page: p.clone(), off: 0, len }),
                None => break,
            }
        }
        if segs.is_empty() {
            return Ok(());
        }
        let prio = self.run_priority();
        self.disk.write_pages(lba, segs, false, prio).map_err(errno)
    }

    fn commit(&self, why: SyncKind) -> Result<(), i64> {
        // Saturating: a commit without its begin never wraps the count.
        let c = self.count(why);
        let mut n = c.load(Ordering::Acquire);
        while n > 0 {
            match c.compare_exchange_weak(n, n - 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(now) => n = now,
            }
        }
        if why.durable() && self.disk.dev_info().has_volatile_cache {
            self.disk.flush(crate::prio::higher(IoPriority::Normal, crate::prio::current())).map_err(errno)?;
        }
        Ok(())
    }
}
