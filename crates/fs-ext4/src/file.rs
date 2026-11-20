//! A regular ext4 file behind the VFS: its page cache (FS-5) with the volume as the backend. A write that
//! reaches pages the disk does not map yet promises their blocks first (`Volume::reserve`): a full volume
//! answers ENOSPC at `write`, not at write-back (Linux's delayed-allocation reservation). The write-back
//! writes runs in steps (`flight`: blocks claimed, the data to the device, then blocks, extents and inode in one
//! transaction; FS-W2: on a disk the pages themselves go to the device, `Volume::write_pages`) and stores the
//! times the VFS keeps; fsync then commits and flushes the device, after the cache's lock is released.

use crate::vol::{Hold, Meta, Slot, Volume};
use alloc::vec::Vec;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use crate::errno::*;
use ksync::{Mutex, MutexGuard, SpinLock};
use vfs::inode::{Inode, InodeOps};
use vfs::pagecache::{Backend, CachePage, PageCache, SyncKind};

const PAGE: u64 = 4096;

pub struct Ext4File {
    pub vol: Arc<Volume>,
    pub ino: u32,
    pub cache: PageCache,
    /// The VFS inode (its times and mode are what the write-back stores).
    me: SpinLock<Option<Weak<Inode>>>,
    registered: AtomicBool,
    /// st_blocks as the volume keeps it (`Volume::hold`): stat takes no volume lock.
    blocks: Arc<AtomicU64>,
    /// This file's place on the volume's list of dropped inodes (`Volume::hold`), given back in the drop.
    slot: Option<Slot>,
}

struct Back {
    vol: Arc<Volume>,
    ino: u32,
    file: Weak<Ext4File>,
}

impl Ext4File {
    /// `hold`: from `Volume::hold`, which the caller called for this file (its `forget` is this file's drop).
    pub fn new(vol: Arc<Volume>, ino: u32, size: u64, hold: Hold) -> Arc<Ext4File> {
        let Hold { blocks, slot } = hold;
        Arc::new_cyclic(|me: &Weak<Ext4File>| {
            let back = Arc::new(Back { vol: vol.clone(), ino, file: me.clone() });
            Ext4File { vol, ino, cache: PageCache::with_backend(back, size), me: SpinLock::named("ext4.file.me", None), registered: AtomicBool::new(false), blocks, slot: Some(slot) }
        })
    }
    pub fn set_inode(&self, i: &Arc<Inode>) {
        *self.me.lock() = Some(Arc::downgrade(i));
    }
    fn register(self: &Arc<Self>) {
        if !self.registered.swap(true, Ordering::AcqRel) {
            let w: Weak<dyn vfs::writeback::Flush> = Arc::downgrade(self) as Weak<dyn vfs::writeback::Flush>;
            if vfs::writeback::register(w).is_err() {
                self.registered.store(false, Ordering::Release);
            }
        }
    }
    /// The times the VFS holds, for the disk.
    fn meta(&self) -> Option<Meta> {
        let w = self.me.lock().clone()?;
        let i = w.upgrade()?;
        let (a, m, c) = i.meta.times();
        Some(Meta { atime: Some(a), mtime: Some(m), ctime: Some(c), ..Meta::default() })
    }
    fn store_meta(&self) -> Result<(), i64> {
        match self.meta() {
            Some(m) if self.vol.writable() => self.vol.store_meta_wb(self.ino, m),
            _ => Ok(()),
        }
    }
    fn max_size(&self) -> u64 {
        ((1u64 << 32) - 1) * self.vol.geo.bs
    }
}

impl Drop for Ext4File {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            self.vol.forget(slot);
        }
    }
}

impl vfs::writeback::Flush for Ext4File {
    /// The background pass leaves a frozen volume's pages dirty for the pass after the thaw (it never waits).
    fn flush(&self, why: SyncKind) -> Result<(), i64> {
        if !why.durable() && self.vol.is_frozen() {
            return Ok(());
        }
        self.cache.sync_all(why)
    }
}

/// The InodeOps the VFS holds (`Data::External`).
pub struct Ops(pub Arc<Ext4File>);

impl InodeOps for Ops {
    fn page_cache(&self) -> Option<&PageCache> {
        Some(&self.0.cache)
    }
    fn size(&self) -> u64 {
        self.0.cache.size()
    }
    fn read_cache(&self) -> Option<&PageCache> {
        Some(&self.0.cache)
    }
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> Result<usize, i64> {
        self.0.cache.read_at(pos, buf)
    }
    fn write_at(&self, pos: u64, buf: &[u8]) -> Result<usize, i64> {
        let f = &self.0;
        if pos >= f.max_size() {
            return Err(EFBIG);
        }
        let n = buf.len().min((f.max_size() - pos) as usize);
        if n == 0 {
            return Ok(0);
        }
        f.vol.reserve(f.ino, pos / PAGE, (pos + n as u64 - 1) / PAGE)?;
        f.register();
        let w = f.cache.write_at(pos, &buf[..n])?;
        // FS-STALL rounds 4/5: dirty pages are bounded (a fifth of memory, Linux's dirty_ratio). Over it, a writer
        // whose file holds a large share writes it back itself; any other waits for the flusher, in proportion to
        // what it wrote (`writeback::balance_dirty`). No commit, no device flush here: the flusher's pass commits.
        // A failed write-back leaves the pages dirty for the flusher; the write itself went through.
        if vfs::pagecache::dirty_over_limit() {
            let dirtied = (w as u64).div_ceil(PAGE);
            if vfs::writeback::balance_dirty(f.cache.own_dirty(), dirtied) == vfs::writeback::Balance::WriteOwn {
                DIRTY_THROTTLED.inc();
                let _ = f.cache.sync_all(SyncKind::Background);
                vfs::writeback::kick();
            }
        }
        Ok(w)
    }
    /// `ext4_fallocate` for modes 0 and FALLOC_FL_KEEP_SIZE: unwritten extents on the disk now (ENOSPC now,
    /// and the space stays through eviction and a cut), mode 0 extends the size; other modes EOPNOTSUPP.
    fn fallocate(&self, mode: u32, offset: u64, len: u64) -> Result<bool, i64> {
        if mode & !vfs::abi::FALLOC_FL_KEEP_SIZE != 0 {
            return Err(EOPNOTSUPP);
        }
        let f = &self.0;
        let end = offset.checked_add(len).ok_or(EFBIG)?;
        if end > f.max_size() {
            return Err(EFBIG);
        }
        if len > 0 {
            f.vol.fallocate(f.ino, offset, len)?;
        }
        if mode & vfs::abi::FALLOC_FL_KEEP_SIZE != 0 || end <= self.size() {
            return Ok(false);
        }
        self.truncate(end)?;
        Ok(true)
    }
    fn truncate(&self, size: u64) -> Result<(), i64> {
        let f = &self.0;
        if size > f.max_size() {
            return Err(EFBIG);
        }
        if !f.vol.writable() {
            return Err(EROFS);
        }
        if size < f.cache.size() {
            f.cache.truncate(size)?;
            f.vol.set_size(f.ino, size)
        } else {
            f.vol.set_size(f.ino, size)?;
            f.cache.truncate(size)
        }
    }
    fn sync(&self, why: SyncKind) -> Result<(), i64> {
        if !why.durable() && self.0.vol.is_frozen() {
            return Ok(());
        }
        self.0.cache.sync_all(why)?;
        self.0.store_meta()?;
        if why.durable() { self.0.vol.flush() } else { Ok(()) }
    }
    fn blocks(&self) -> u64 {
        self.0.blocks.load(Ordering::Relaxed)
    }
}

/// Pages per step of a write-back. Each step is one `Volume::write`: its data, then its blocks, extents and size, so
/// the order holds per step. 1 MiB: one device request (`vol_data::IO_MAX`), and the volume lock is free again
/// after each, so other programs' file calls get in between a download's steps.
pub(crate) const WRITE_STEP_PAGES: usize = 256;
/// The buffers the steps copy their pages into where the device takes no pages (an image file, a volume with
/// blocks smaller than a page; a disk with 4 KiB blocks needs none since FS-W2), made once (`write_bufs_init`,
/// at the first writable mount of such a volume) and kept. Their number bounds those volumes' steps at the device;
/// such a device has no queue that could say how many it takes (its writes are copies into another cache). Write-back is what frees memory, so it must not ask the heap while memory is short. 11.11.2026, a Steam
/// download beside a full page cache on the Mini-PC: every run asked for 16 MiB, then for halves, each refused
/// only after the heap's reclaim rounds, 125 refusals a second on ten threads; sound and picture stalled.
const WRITE_BUFS: usize = 4;
static WRITE_BUF: [Mutex<Vec<u8>>; WRITE_BUFS] = [const { Mutex::named("ext4.wb_buf", Vec::new()) }; WRITE_BUFS];
static WRITE_BUF_NEXT: AtomicUsize = AtomicUsize::new(0);

/// Room for a whole step in `buf` if the heap gives it, else for as many pages as it gives (at least one).
fn write_buf_fill(buf: &mut Vec<u8>) -> Result<(), i64> {
    let mut pages = WRITE_STEP_PAGES;
    while buf.try_reserve_exact(pages * PAGE as usize).is_err() {
        if pages == 1 {
            return Err(ENOMEM);
        }
        pages /= 2;
    }
    Ok(())
}

/// Makes the kept buffers while memory is still free (a writable mount). A buffer the heap cannot give now is
/// made by its first user (`write_buf`).
pub(crate) fn write_bufs_init() {
    for m in &WRITE_BUF {
        let mut b = m.lock();
        if b.capacity() == 0 && write_buf_fill(&mut b).is_err() {
            klog::event!(Warn, "ext4", "write_buf.short"; "no memory for a write-back buffer at mount: its first user makes it");
        }
    }
}

/// One of the kept buffers, empty: a free one, else this thread sleeps until its turn at one.
fn write_buf() -> Result<MutexGuard<'static, Vec<u8>>, i64> {
    let mut b = match WRITE_BUF.iter().find_map(|m| m.try_lock()) {
        Some(b) => b,
        None => WRITE_BUF[WRITE_BUF_NEXT.fetch_add(1, Ordering::Relaxed) % WRITE_BUFS].lock(),
    };
    b.clear();
    if b.capacity() == 0 {
        write_buf_fill(&mut b)?;
    }
    Ok(b)
}

klog::counter!(pub DIRTY_THROTTLED, "ext4.dirty_throttled");
// BL-0: file pages read one by one through a buffer (since BL-3 only inline data), and the bytes ext4 copies from
// a buffer into cache pages (that path, a piece not on a block boundary, a device without `read_pages`), per CPU.
vfs::pcpu_counter!(pub READ_PAGE_CALLS, "ext4.read_page_calls", vfs::pcpu::EVENTS_BATCH);
vfs::pcpu_counter!(pub COPY_EXT4_PAGE, "block.copy_bytes.ext4_page", vfs::pcpu::BYTES_BATCH);
// BL-3: windows of pages read straight into the cache (`Volume::read_pages`).
vfs::pcpu_counter!(pub READ_WINDOWS, "ext4.read_windows", vfs::pcpu::EVENTS_BATCH);

/// BL-3: pages one missed page brings in, in one request per extent piece: Linux's default read-ahead size
/// (`VM_READAHEAD_PAGES`, 128 KiB, linux-7/include/linux/pagemap.h:1429), which `blk_apply_bdev_limits`
/// (linux-7/block/blk-settings.c:82) keeps for a device that names no optimal I/O size, as none here does. BL-4
/// owns read-ahead growth.
const WINDOW: u64 = 32;

impl Backend for Back {
    fn read_window(&self) -> u64 {
        WINDOW.min(vfs::pagecache::MAX_WINDOW)
    }
    /// BL-3: the window straight from the device into its pages (`Volume::read_pages`); inline data page by page
    /// as before.
    fn read_pages(&self, first: u64, pages: &[Arc<CachePage>]) -> Result<(), i64> {
        if self.vol.read_pages(self.ino, first * PAGE, pages)? {
            READ_WINDOWS.inc();
            return Ok(());
        }
        for (i, p) in pages.iter().enumerate() {
            self.read_page(first + i as u64, p)?;
        }
        Ok(())
    }
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), i64> {
        READ_PAGE_CALLS.inc();
        // Fallible: read-ahead runs on a full machine too (FS-STALL: this buffer was the QEMU repro's panic).
        let mut buf = Vec::new();
        buf.try_reserve_exact(PAGE as usize).map_err(|_| ENOMEM)?;
        buf.resize(PAGE as usize, 0);
        self.vol.read(self.ino, pgoff * PAGE, &mut buf)?;
        page.write(0, &buf);
        COPY_EXT4_PAGE.add(PAGE);
        Ok(())
    }
    fn write_run(&self, first: u64, pages: &[Arc<CachePage>], size: u64) -> Result<(), i64> {
        let off = first * PAGE;
        if off >= size {
            return Ok(());
        }
        let len = (pages.len() as u64 * PAGE).min(size - off) as usize;
        // Pages past `size` (a file cut after they were collected) hold nothing to write: the last step gives back
        // their promises too.
        let with_data = len.div_ceil(PAGE as usize);
        let mut at = 0;
        // FS-W2: a disk takes the pages themselves as its requests' memory (`Dev::write_pages`): no buffer, no copy,
        // and no count of buffers that bounds the steps. Each writing thread has one step at the device; what the
        // device does not take at once waits in its queue (`block::Disk`: `queue_depth` slots), and the thread
        // with it. With blocks smaller than a page the data shares the disk cache's pages with metadata and goes
        // through the buffer path below, as on a device that is a file.
        if self.vol.geo.bs == PAGE && self.vol.dev.takes_pages() {
            while at < with_data {
                let part = &pages[at..(at + WRITE_STEP_PAGES).min(with_data)];
                let bytes = (len - at * PAGE as usize).min(part.len() * PAGE as usize);
                let step = first + at as u64;
                let promised = step..if at + part.len() == with_data { first + pages.len() as u64 } else { step + part.len() as u64 };
                self.vol.write_pages(self.ino, step * PAGE, part, bytes, size, promised)?;
                at += part.len();
            }
            return Ok(());
        }
        while at < with_data {
            // A kept buffer, never the heap (`WRITE_BUF`); the step is what the buffer holds. Taken for one step
            // (FS-W): a long run held one of the four for all its steps (4.6 s on the Mini-PC) and the fifth
            // writer waited that long; now the writers take turns step by step.
            let mut data = write_buf()?;
            let step_pages = (data.capacity() / PAGE as usize).clamp(1, WRITE_STEP_PAGES);
            let part = &pages[at..(at + step_pages).min(with_data)];
            data.resize((len - at * PAGE as usize).min(part.len() * PAGE as usize), 0);
            for (p, b) in part.iter().zip(data.chunks_mut(PAGE as usize)) {
                p.read(0, b);
            }
            let step = first + at as u64;
            let promised = step..if at + part.len() == with_data { first + pages.len() as u64 } else { step + part.len() as u64 };
            self.vol.write(self.ino, step * PAGE, &data, size, promised)?;
            at += part.len();
        }
        Ok(())
    }
    fn commit(&self, _why: SyncKind) -> Result<(), i64> {
        match self.file.upgrade() {
            Some(f) => f.store_meta(),
            None => Ok(()),
        }
    }
    fn read_ahead_pages(&self) -> u64 {
        32
    }
    /// FS-W2: 4 MiB (four steps). A step's data now waits for the device, so the pages a write-back took and has
    /// not written are real memory in flight; one batch for all of a file's dirty pages took them out of the dirty
    /// count at once and the limit (`dirty_over_limit`) let every writer go on. Linux counts pages under write-back
    /// apart and holds writers back by both numbers; V2 has the one number, so what is outside it stays small:
    /// one batch per writing thread.
    fn batch_pages(&self) -> usize {
        4 * WRITE_STEP_PAGES
    }
    /// BL-4 CR 4: the disk's request size in pages (0 on an image or in memory: as `read_ahead_pages`).
    fn io_pages(&self) -> u64 {
        self.vol.dev.io_pages()
    }
}
