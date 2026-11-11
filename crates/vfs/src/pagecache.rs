//! The page cache (FS-5): the pages of one file, by file page number, each a frame from the frame allocator
//! (`frame`), held in a fallible radix index (`index`; no fixed count, R1). Reads and writes of regular files go
//! through it, file mappings map its frames (`mmap`), and dirty pages go back to their file system through the
//! `Backend` hooks in the order the file system asks for (`sync`).
//!
//! tmpfs files are nothing but their cache: no backend, a missing page is a hole (reads as zeros). FS-14: a
//! tmpfs file or memfd is charged to its pool page by page when a page is allocated (Linux VM_NORESERVE,
//! `shmem_inode_acct_block` from `shmem_alloc_and_add_folio`), so a growing ftruncate costs nothing; only the
//! kernel's nameless objects (shared anonymous memory, Linux `shmem_kernel_file_setup` without VM_NORESERVE
//! under overcommit mode 2) are charged by size when it grows (`PageCache::tmpfs_accounted`). A block file system (FS-6..8) gives its
//! files a `Backend`: missing pages are read from it, written pages are marked dirty and written back by
//! `sync` (fsync, fdatasync, msync, the background flusher) as runs of consecutive pages, between the
//! backend's `begin` and `commit` hooks, so FAT can keep "data, then FAT, then directory entry, then FSInfo"
//! and ext4 its journal order.
//!
//! Pages are `Arc<CachePage>`: the cache holds one reference, every file-mapping pin another (`mmap`), so a
//! page cut off by truncate stays alive (and keeps its frame, and for tmpfs its one page of pool charge) until
//! the last mapping lets it go, and frames never go back to the allocator while a program may still reach them.
//!
//! Locks: `vfs.inode.data` (Mutex, one per file) around the index and size; never held while a user copy
//! runs (callers copy through bounce buffers) and never while `mm.as` is taken (the fault path takes `mm.as`
//! first, then this), and never while a backend reads or writes (`fill` reads a missing page with the lock
//! released and inserts it after, B4). `vfs.cache.sync` (Mutex per file) keeps one write-back of a file at a
//! time, taken before `vfs.inode.data`, which is released while the backend writes.

use crate::cache_view::{Gone, Pages, Shared, NO_MARK};
pub use crate::cache_view::{QuickSink, Stop};
use crate::frame::{Frame, PAGE};
use crate::memfd::{F_SEAL_EXEC, F_SEAL_FUTURE_WRITE, F_SEAL_GROW, F_SEAL_SEAL, F_SEAL_SHRINK, F_SEAL_WRITE};
use crate::pool::{pages_for, Pool};
use crate::readahead;
use crate::Errno;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use ksync::{Mutex, WaitQueue};
use uapi::errno::*;

const P: u64 = PAGE as u64;

/// Frames the page cache holds (all files, pages cut off but still mapped included).
static FRAMES_HELD: AtomicU64 = AtomicU64::new(0);
/// Of those, pages of pool-charged (tmpfs) files.
static TMPFS_FRAMES: AtomicU64 = AtomicU64::new(0);
static NO_FRAME: AtomicBool = AtomicBool::new(false);

klog::counter!(WRITEBACK_PAGES, "vfs.cache.writeback_pages");
klog::counter!(NO_FRAME_COUNT, "vfs.cache.no_frame");
klog::counter!(CARRIED, "vfs.cache.truncated_mapped_pages");
// PF-5: lockless steps that raced a truncate or a punch (copied again on the locked path). No counter on the
// lockless path itself: a shared counter line written by every read is what PF-5 removes.
klog::counter!(QUICK_RETRIES, "vfs.read.quick_retries");
// PF-4: pages a hole punch took out of a size-charged file while reads still copied from them.
klog::counter!(READ_HELD_PUNCHED, "vfs.cache.punched_read_pages");
klog::counter!(NO_CHARGE_FAULTS, "vfs.cache.fault_no_charge");
klog::counter!(FILL_READS, "vfs.cache.fill_reads");
klog::counter!(FILL_PAGES, "vfs.cache.fill_pages");
klog::counter!(FILL_WAITS, "vfs.cache.fill_waits");
klog::counter!(FILL_WAIT_TIMEOUTS, "vfs.cache.fill_wait_timeouts");
klog::counter!(FILL_WAIT_PAST_BG, "vfs.cache.fill_wait_past_preload");
klog::counter!(FILL_WINDOW_ERRORS, "vfs.cache.fill_window_errors");
// BL-0: POSIX_FADV_DONTNEED and drop_caches (not on the read path: shared counters).
klog::counter!(DONTNEED_DROPPED, "vfs.dontneed.dropped");
klog::counter!(DONTNEED_KEPT, "vfs.dontneed.kept");
klog::counter!(DONTNEED_WB_ERRORS, "vfs.dontneed.writeback_errors");
// BL-0: a reader's miss read from the backend in its own context (Linux `page_cache_sync_ra`), per CPU.
crate::pcpu_counter!(pub(crate) RA_SYNC, "vfs.readahead.sync", crate::pcpu::EVENTS_BATCH);
// BL-4: misses read as asked, without a window (a standalone random read, POSIX_FADV_RANDOM, MADV_RANDOM), and the
// mapping faults' windows (`fault_ra`).
crate::pcpu_counter!(RA_RANDOM, "vfs.readahead.random", crate::pcpu::EVENTS_BATCH);
crate::pcpu_counter!(RA_MMAP_SYNC, "vfs.readahead.mmap_sync", crate::pcpu::EVENTS_BATCH);
crate::pcpu_counter!(RA_MMAP_ASYNC, "vfs.readahead.mmap_async", crate::pcpu::EVENTS_BATCH);
klog::counter!(RA_LOTSAMISS, "vfs.readahead.mmap_lotsamiss");
/// Linux MMAP_LOTSAMISS (mm/filemap.c:3275): a file whose mapping faults miss this much more than they hit gets no
/// mmap read-around any more.
const MMAP_LOTSAMISS: u32 = 100;

/// What an invalidation did (`PageCache::dont_need`, `reclaim::drop_caches`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Invalidated {
    /// Pages that left the cache (their frames after a grace period).
    pub dropped: u64,
    /// Pages of the range that stayed: dirty, mapped, held by a read or a request.
    pub kept: u64,
}

/// Pages one hold of `vfs.inode.data` looks at in `invalidate_clean` (Linux takes a batch and reschedules).
const INVALIDATE_CHUNK: u32 = 512;
/// Pages a truncate takes out of the index per step (`PageCache::truncate`): the list of one step is 8 KiB.
const CUT_STEP: usize = 1024;

/// BL-0 (`invalidate_mapping_pages`, linux-7/mm/truncate.c:534 `mapping_try_invalidate` with
/// `mapping_evict_folio` :325): takes the clean pages of file pages `[first, last]` that only the index holds out
/// of the cache, `INVALIDATE_CHUNK` pages per hold of the data lock. A page that stays is marked COLD, so reclaim
/// takes it first once it is free (`deactivate_file_folio`). `take_alone` decides under the index's write side, so
/// a mapped page, one a locked read copies from or a block request holds stays; lockless readers hold no
/// reference, so a page that goes is dropped through PF-5's grace queue (`drop_nowait`), never wiped. Writers go
/// through this lock, so a page seen clean here stays clean until it is taken.
pub(crate) fn invalidate_clean(inner: &Mutex<Inner>, first: u64, last: u64) -> Invalidated {
    let mut out = Invalidated::default();
    let mut at = first;
    loop {
        let g = inner.lock();
        for _ in 0..INVALIDATE_CHUNK {
            let Some((i, dirty)) = g.pages.next_with(at, |i, p| {
                if i <= last {
                    p.set_state(COLD);
                }
                p.is_dirty()
            }) else {
                return out;
            };
            if i > last {
                return out;
            }
            match if dirty { None } else { g.pages.take_alone(i) } {
                Some(page) => {
                    page.drop_nowait();
                    out.dropped += 1;
                }
                None => out.kept += 1,
            }
            if i >= last {
                return out;
            }
            at = i + 1;
        }
        drop(g);
    }
}

/// FS-STALL round 4: dirty pages of block file systems and disks now (tmpfs pages are not written back). There was
/// no bound on them: a download filled memory with pages reclaim cannot take (`dirty_over_limit`).
static DIRTY_PAGES: AtomicU64 = AtomicU64::new(0);

/// Dirty pages of block file systems and disks now.
pub fn dirty_pages_total() -> u64 {
    DIRTY_PAGES.load(Ordering::Relaxed)
}

/// Whether dirty pages exceed a fifth of memory (Linux's `vm.dirty_ratio`, 20 %): a writer then writes back
/// before it dirties more (`balance_dirty_pages`; ext4's `write_at`).
pub fn dirty_over_limit() -> bool {
    let dirty = DIRTY_PAGES.load(Ordering::Relaxed);
    #[cfg(not(bitsos_no_ktests))]
    {
        let test = DIRTY_LIMIT_TEST.load(Ordering::Relaxed);
        if test != 0 {
            return dirty > test;
        }
    }
    // Cheap first: a fifth of memory is never below this many pages on a machine V2 runs on.
    if dirty < 16 * 1024 {
        return false;
    }
    let mut total = TOTAL_PAGES.load(Ordering::Relaxed);
    if total == 0 {
        total = mm_core::frames().map_or(0, |f| f.stats().total_bytes / P);
        TOTAL_PAGES.store(total, Ordering::Relaxed);
    }
    total > 0 && dirty > total / 5
}

static TOTAL_PAGES: AtomicU64 = AtomicU64::new(0);
#[cfg(not(bitsos_no_ktests))]
static DIRTY_LIMIT_TEST: AtomicU64 = AtomicU64::new(0);

/// ktests: the dirty limit is `pages` until the guard drops (a test machine's disk cannot hold a fifth of memory).
/// Not in the lean kernel.
#[cfg(not(bitsos_no_ktests))]
pub fn dirty_limit_for_test(pages: u64) -> DirtyLimitOverride {
    DIRTY_LIMIT_TEST.store(pages.max(1), Ordering::Relaxed);
    DirtyLimitOverride(())
}

#[cfg(not(bitsos_no_ktests))]
pub struct DirtyLimitOverride(());

#[cfg(not(bitsos_no_ktests))]
impl Drop for DirtyLimitOverride {
    fn drop(&mut self) {
        DIRTY_LIMIT_TEST.store(0, Ordering::Relaxed);
    }
}

/// Frames held by the page cache now.
pub fn frames_held() -> u64 {
    FRAMES_HELD.load(Ordering::Relaxed)
}

/// Frames held by tmpfs files now (`/proc/meminfo` Shmem).
pub fn tmpfs_frames() -> u64 {
    TMPFS_FRAMES.load(Ordering::Relaxed)
}

/// One cached page.
pub struct CachePage {
    frame: Frame,
    dirty: AtomicBool,
    /// The pool of a tmpfs file's page (None: a block file system's page).
    pool: Option<Arc<Pool>>,
    /// The page holds one page of its file's pool charge and gives it back when it is freed: every page of a
    /// lazily charged file (FS-14), and a page of a size-charged file that truncate cut off while a mapping
    /// still pinned it (its frame is still held, R2).
    carries_charge: AtomicBool,
    /// MM-9: the reclaim clock's view (`REFERENCED`, `COLD`, `PRELOADED`; `crate::reclaim`).
    state: AtomicU8,
    /// PF-4: references reads hold for their copy (`cache_view::ReadHold`), so a hole punch can tell them from a
    /// mapping's pin: it takes a page only reads hold out of the index (their copy keeps the frame), as Linux's
    /// punch drops a folio a reader still holds.
    pub(crate) reads: AtomicU32,
    /// PF-5: left the cache's accounting when it left the index alone (`account_out`); its frame waits for a
    /// grace period as pending memory, and its drop only frees it.
    out: AtomicBool,
    /// PF-5: left the index while something else still held it (a locked read, a pin): the last of those to
    /// let go before the grace queue's own drop accounts it out (`cache_view::ReadHold`), so its charge does not
    /// wait for the queue.
    pub(crate) retired: AtomicBool,
    /// RB-18: the page held a secret ([`PageCache::forget_clean`]): its frame is zeroed before it goes back.
    pub(crate) wipe: AtomicBool,
}

/// MM-9: used since the reclaim clock last passed (a new page starts so: one pass of protection).
pub(crate) const REFERENCED: u8 = 0;
/// MM-9: the clock passed it once unused: the next pass takes it.
pub(crate) const COLD: u8 = 1;
/// MM-9: brought in by the start-up preload (FS-13) and not used since: reclaim takes these first.
pub(crate) const PRELOADED: u8 = 2;

impl CachePage {
    pub(crate) fn new(pool: Option<&Arc<Pool>>) -> Option<Arc<CachePage>> {
        let Some(frame) = Frame::new_zeroed() else {
            NO_FRAME_COUNT.inc();
            if !NO_FRAME.swap(true, Ordering::AcqRel) {
                klog::event!(Warn, "vfs", "cache.no_frame"; "no frame for a page-cache page: the call answers ENOMEM");
            }
            return None;
        };
        NO_FRAME.store(false, Ordering::Relaxed);
        FRAMES_HELD.fetch_add(1, Ordering::Relaxed);
        if pool.is_some() {
            TMPFS_FRAMES.fetch_add(1, Ordering::Relaxed);
        }
        Some(Arc::new(CachePage { frame, dirty: AtomicBool::new(false), pool: pool.cloned(), carries_charge: AtomicBool::new(false), state: AtomicU8::new(REFERENCED), reads: AtomicU32::new(0), out: AtomicBool::new(false), retired: AtomicBool::new(false), wipe: AtomicBool::new(false) }))
    }
    /// The frame's physical address (None only for heap pages without a frame allocator: host tests).
    pub fn phys(&self) -> Option<u64> {
        self.frame.phys()
    }
    /// The frame's identity (its physical address; a heap address in host tests).
    pub fn ident(&self) -> u64 {
        self.frame.ident()
    }
    pub fn read(&self, off: usize, out: &mut [u8]) {
        self.frame.read(off, out)
    }
    /// PF-6: the page's frame for a copy straight to a program (`UserMem::write_page` of the kernel's user
    /// memory); None for a heap page (host tests).
    pub fn page_frame(&self) -> Option<&mm_core::PageFrame> {
        self.frame.page_frame()
    }
    pub fn write(&self, off: usize, data: &[u8]) {
        self.frame.write(off, data)
    }
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }
    pub fn set_dirty(&self) {
        self.make_dirty();
    }
    /// `set_dirty`; whether the page was clean before.
    fn make_dirty(&self) -> bool {
        let fresh = !self.dirty.swap(true, Ordering::AcqRel);
        if fresh && self.pool.is_none() {
            DIRTY_PAGES.fetch_add(1, Ordering::Relaxed);
        }
        fresh
    }
    /// Clears the dirty flag (a write-back took the page); whether it was set.
    fn take_dirty(&self) -> bool {
        let was = self.dirty.swap(false, Ordering::AcqRel);
        if was && self.pool.is_none() {
            DIRTY_PAGES.fetch_sub(1, Ordering::Relaxed);
        }
        was
    }
    /// MM-9: a read, write or mapping used the page (Linux `folio_mark_accessed`); a load first, so a hot page's
    /// cache line is not written on every access.
    pub(crate) fn touch(&self) {
        if self.state.load(Ordering::Relaxed) != REFERENCED {
            self.state.store(REFERENCED, Ordering::Relaxed);
        }
    }
    pub(crate) fn state(&self) -> u8 {
        self.state.load(Ordering::Relaxed)
    }
    pub(crate) fn set_state(&self, v: u8) {
        self.state.store(v, Ordering::Relaxed);
    }
}

impl CachePage {
    /// PF-5: the page left the index with no other reference (`cache_view::Gone`): it leaves the cache's numbers
    /// now (frames held, tmpfs frames, a carried pool charge, R2) instead of at its drop, which waits for a grace
    /// period. Returns the bytes that wait (once; 0 when it was accounted out already).
    pub(crate) fn account_out(&self) -> u64 {
        if self.out.swap(true, Ordering::AcqRel) {
            return 0;
        }
        self.leave_accounts();
        PAGE as u64
    }
    /// PF-5: drops a reference that is not the index's (a locked read's, a mapping's pin). When the page left
    /// the index while this one held it and only the grace queue's reference stays, the page leaves the
    /// cache's numbers now (its charge back at once, R2) instead of at the queue's drop. Two holders that let
    /// go together may both see three references: then the queue's drop does it.
    pub(crate) fn release(p: Arc<CachePage>) {
        if p.retired.load(Ordering::Acquire) && Arc::strong_count(&p) == 2 {
            p.account_out();
        }
        drop(p);
    }
    fn leave_accounts(&self) {
        FRAMES_HELD.fetch_sub(1, Ordering::Relaxed);
        if let Some(pool) = &self.pool {
            TMPFS_FRAMES.fetch_sub(1, Ordering::Relaxed);
            if self.carries_charge.swap(false, Ordering::AcqRel) {
                pool.uncharge(1);
            }
        }
    }
}

impl Drop for CachePage {
    fn drop(&mut self) {
        if *self.dirty.get_mut() && self.pool.is_none() {
            DIRTY_PAGES.fetch_sub(1, Ordering::Relaxed);
        }
        if *self.wipe.get_mut() {
            self.frame.zero_from(0);
        }
        if !*self.out.get_mut() {
            self.leave_accounts();
        }
    }
}

/// Why a write-back runs (the backend may order or flush differently).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncKind {
    /// fsync: data and metadata, durable (a cache flush of the device).
    Fsync,
    /// fdatasync: data and the metadata needed to read it back, durable.
    Fdatasync,
    /// msync(MS_SYNC) of a shared mapping's range (Linux: `vfs_fsync_range(.., datasync=1)`).
    Msync,
    /// The background flusher: not durable.
    Background,
}

impl SyncKind {
    /// Whether the call waits until the data is on stable storage.
    pub fn durable(self) -> bool {
        !matches!(self, SyncKind::Background)
    }
}

/// A file system's side of its cached pages (FS-6..8 implement it; tmpfs has none).
pub trait Backend: Send + Sync {
    /// Fills a page that is not cached yet (it is zeroed; a hole stays zero) from the file's storage. Called
    /// with no lock of the cache held (a slow device blocks only the caller, B4); two callers may read the
    /// same page at once, the first to insert it wins.
    /// FS-6: the page comes as its `Arc`, so a block request can own it while the device fills it.
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), Errno>;
    /// Before the first data of one write-back (FAT: mark FSInfo's free count unknown).
    fn begin(&self, _why: SyncKind) -> Result<(), Errno> {
        Ok(())
    }
    /// Writes one run of consecutive dirty pages starting at file page `first`; `size` is the file size (the
    /// last page's bytes past it are not file data). Data always goes before `commit`'s metadata.
    fn write_run(&self, first: u64, pages: &[Arc<CachePage>], size: u64) -> Result<(), Errno>;
    /// After every run of this write-back: metadata in the file system's order (FAT: FATs, then the directory
    /// entry, then FSInfo; ext4: the journal commit), and a device cache flush when `why.durable()`.
    fn commit(&self, _why: SyncKind) -> Result<(), Errno> {
        Ok(())
    }
    /// FS-12: the most pages `readahead` may read ahead of a sequential reader (0: none; the backend reads
    /// ahead itself or not at all).
    fn read_ahead_pages(&self) -> u64 {
        0
    }
    /// BL-4: the device's largest request in pages (Linux `bdi->io_pages`, max_sectors): a read asking for more
    /// than `read_ahead_pages` gets a window up to this (`readahead::max_pages`). 0: as `read_ahead_pages`.
    fn io_pages(&self) -> u64 {
        0
    }
    /// FS-14b: how many pages one missed page brings into the cache in one device request (the missed page
    /// and its neighbours, all of them kept: `PageCache::fill_with`); 1 = the missed page alone. A read() that
    /// wants more pages than this gets them in the same request, up to `MAX_WINDOW`.
    fn read_window(&self) -> u64 {
        1
    }
    /// FS-14b: fills `pages.len()` consecutive pages from file page `first` (all zeroed, all inside the file
    /// when the call began; holes stay zero), with no lock of the cache held, like `read_page`. The default
    /// reads them one by one.
    fn read_pages(&self, first: u64, pages: &[Arc<CachePage>]) -> Result<(), Errno> {
        for (i, p) in pages.iter().enumerate() {
            self.read_page(first + i as u64, p)?;
        }
        Ok(())
    }
}

/// FS-14b: the most pages one missed read brings in (1 MiB: the largest request the FAT code and a USB stick
/// on SuperSpeed take in one command).
pub const MAX_WINDOW: u64 = 256;

/// FS-14b: how long a missed page waits for another thread's read of it before reading it itself (B4: never
/// an unbounded wait on another reader; the block layer's deadline ends that read well before).
#[cfg(not(test))]
const FILL_WAIT_NS: u64 = 2_000_000_000;
/// Host tests: short, so the timeout path runs in the test's time.
#[cfg(test)]
pub(crate) const FILL_WAIT_NS: u64 = 300_000_000;
/// FS-13: how long a foreground miss waits for the preload's (Background) read of its page before it reads
/// the page's window itself at its own priority: that read may sit behind other foreground requests, and the
/// FS-12 bound for an Interactive read must hold.
pub(crate) const BG_FILL_WAIT_NS: u64 = 50_000_000;

/// FS-14b: how far the read of a missed page reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reach {
    /// The page alone (a partly written page).
    One,
    /// read(): the page and the pages after it that the call still wants, at least the backend's window
    /// (linux-6.6/mm/filemap.c `filemap_get_pages` -> `page_cache_sync_readahead`).
    Forward(u64),
    /// BL-3: read() of a page whose page before is not cached, on a backend with its own read-ahead and a window:
    /// only the pages the call wants (linux-7/mm/readahead.c:630 "Standalone, small random read. Read as is"), not
    /// the window.
    Request(u64),
    /// A mapping's fault: the backend's window around the page (linux-6.6/mm/filemap.c:3195
    /// `do_sync_mmap_readahead`, "mmap read-around": start = pgoff - ra_pages / 2, size = ra_pages).
    Around,
    /// BL-4: the read-ahead window [first, first + len) the missed page lies in (`readahead::Window`).
    Window(u64, u64),
}

/// Why a mapping could not get a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageFault {
    /// The page lies at or past the end of the file (Linux: SIGBUS).
    Bus,
    /// No frame for it.
    NoMemory,
    /// The backend could not read it (Linux: SIGBUS).
    Io,
}

pub(crate) struct Inner {
    /// The file's size; readers without this lock see it through `pages` (`set_size` keeps both).
    size: u64,
    /// Pages a size-charged file is charged to its pool for (= pages_for(size)); 0 for lazily charged files,
    /// whose pages carry their own charge.
    charged: u64,
    /// PF-4: the index (and the size again) behind `vfs.inode.index`, so reads look up without this lock.
    pub(crate) pages: Pages,
    /// FS-10: the file's seals (`shmem_inode_info.seals`; tmpfs files start with F_SEAL_SEAL, memfd_create
    /// may clear it). Kept under this lock with `writable_maps`, so F_ADD_SEALS, writes, truncates and new
    /// shared mappings see one consistent state (Linux: `inode_lock` and `i_mmap_writable`).
    seals: u32,
    /// FS-10: shared mappings that may write (Linux `i_mmap_writable`: VM_SHARED with VM_MAYWRITE), counted
    /// per mmap call (`mmap::FileMap`) while any part of it is mapped; F_SEAL_WRITE is EBUSY while it is not 0.
    writable_maps: u64,
    /// FS-12: bumped when pages leave the index by truncate or hole punch; a read that started before does not
    /// insert its page (`fill`, `readahead`).
    pub(crate) epoch: u64,
    pub(crate) ra: Ra,
    /// FS-14b: page ranges [first, end) a `fill_with` is reading now (Linux keeps such folios locked in the
    /// cache while they are read): a miss inside one waits for it instead of reading the same bytes again.
    /// One entry per running fill (no fixed limit, R1); true marks the preload's (Background) fills.
    reading: Vec<(u64, u64, bool)>,
    /// MM-9: the reclaim clock's hand (the next page it looks at).
    pub(crate) clock: u64,
    /// MM-9: the lowest page that may still be an unused preloaded one (None: none).
    pub(crate) preload_at: Option<u64>,
    /// FS-STALL round 5: pages `write` made dirty that no write-back took yet (a mapping's are not counted): this
    /// file's share of the dirty pages, for `writeback::balance_dirty`.
    own_dirty: u64,
}

impl Inner {
    pub(crate) fn size(&self) -> u64 {
        self.size
    }
    /// PF-4: sets the size for both views (this lock's and the readers').
    fn set_size(&mut self, size: u64) {
        self.size = size;
        self.pages.set_size(size);
    }
    fn sync_mark(&self) {
        self.pages.ra().ra_mark.store(self.ra.mark.unwrap_or(NO_MARK), Ordering::Release);
    }
    /// Whether a running fill reads page `idx`.
    fn in_flight(&self, idx: u64) -> bool {
        self.reading.iter().any(|&(s, e, _)| s <= idx && idx < e)
    }
    /// Whether only the preload's fills read page `idx` (it is in flight).
    fn in_flight_bg_only(&self, idx: u64) -> bool {
        self.reading.iter().all(|&(s, e, bg)| bg || !(s <= idx && idx < e))
    }
    /// Whether a fill may read page `idx` (not cached, nobody reading it).
    pub(crate) fn free(&self, idx: u64) -> bool {
        !self.pages.contains(idx) && !self.in_flight(idx)
    }
    /// BL-4: notes a read-ahead job's pages as being read (false: the list cannot grow).
    pub(crate) fn note_reading(&mut self, r: (u64, u64, bool)) -> bool {
        if self.reading.try_reserve(1).is_err() {
            return false;
        }
        self.reading.push(r);
        true
    }
    /// BL-4: the note of `note_reading` goes.
    pub(crate) fn end_reading(&mut self, r: (u64, u64, bool)) {
        if let Some(at) = self.reading.iter().position(|&x| x == r) {
            self.reading.swap_remove(at);
        }
    }
    /// BL-4: makes `w` the file's window and shows its mark (`Window::mark_in`) to readers without this lock.
    fn set_window(&mut self, w: readahead::Window, pages: u64) {
        self.ra.win = w;
        self.ra.mark = w.mark_in(pages);
        self.sync_mark();
    }
    /// BL-4: `page_cache_async_ra` at the mark: opens the window after the last (`Window::next`, at most `max`)
    /// for a file of `pages` pages and marks the file `queued`: (first, count), None at the end of the file.
    pub(crate) fn open_next(&mut self, pages: u64, max: u64) -> Option<(u64, u64)> {
        if self.ra.random || max == 0 {
            return None;
        }
        let w = self.ra.win.next(max);
        if w.start >= pages || w.size == 0 {
            self.ra.mark = None;
            self.sync_mark();
            return None;
        }
        self.set_window(w, pages);
        self.ra.queued = true;
        Some((w.start, w.size.min(pages - w.start)))
    }
}

/// FS-12, BL-4: the read-ahead state of one file (Linux `file_ra_state`; per file here, per open file there: see
/// plan/notes/bl-BL-4.md).
#[derive(Default)]
pub(crate) struct Ra {
    /// The last window (size 0 = none yet).
    win: readahead::Window,
    /// `win`'s PG_readahead page (None: none, or a reader reached it); mirrored to readers without the data
    /// lock (`sync_mark`).
    mark: Option<u64>,
    /// A job of this file is queued or running.
    pub(crate) queued: bool,
    /// The reader reached `mark` while the job was still queued or running: the job queues the next window
    /// when it ends (`readahead::run`), so a reader faster than the disk keeps the pipeline going.
    pub(crate) follow: bool,
    /// POSIX_FADV_RANDOM (Linux FMODE_RANDOM): misses read what they ask for, no window.
    random: bool,
    /// POSIX_FADV_SEQUENTIAL: twice the backend's window (Linux `f_ra.ra_pages = bdi->ra_pages * 2`).
    double: bool,
}

/// BL-4: posix_fadvise advice that changes read-ahead (linux-7/mm/fadvise.c:79-95).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Advice {
    /// POSIX_FADV_NORMAL: the backend's window, random off.
    Normal,
    /// POSIX_FADV_RANDOM: no window (the window size stays for a later NORMAL or SEQUENTIAL).
    Random,
    /// POSIX_FADV_SEQUENTIAL: twice the backend's window, random off.
    Sequential,
}

/// BL-4: what a mapping's fault tells read-ahead (Linux `vmf->vma->vm_flags` and the VMA's file range).
#[derive(Clone, Copy, Debug, Default)]
pub struct MapHint {
    /// MADV_RANDOM (VM_RAND_READ).
    pub random: bool,
    /// MADV_SEQUENTIAL (VM_SEQ_READ).
    pub sequential: bool,
    /// PROT_EXEC (VM_EXEC): code is read forward from the fault, inside the VMA, without a mark.
    pub exec: bool,
    /// The VMA's file pages [first, end).
    pub first: u64,
    pub end: u64,
}

/// FS-14b, BL-4: fills (and read-ahead jobs) that ended, and the threads that wait for one (`fill_with`). Shared
/// with the jobs (`readahead::Job`), which note their pages in `Inner::reading` like a fill.
pub(crate) struct FillDone {
    fills: AtomicU64,
    wq: WaitQueue,
}

impl FillDone {
    /// Under `vfs.inode.data`, when a fill's note leaves `Inner::reading`.
    pub(crate) fn ended(&self) {
        self.fills.fetch_add(1, Ordering::AcqRel);
    }
    /// After the data lock is dropped.
    pub(crate) fn wake(&self) {
        self.wq.wake_all();
    }
}

/// FS-14b: one running fill's note in `Inner::reading`; taken out (and the waiters woken) when the fill ends,
/// also on an error or a missing frame.
struct Reading<'a> {
    cache: &'a PageCache,
    range: Option<(u64, u64, bool)>,
}

impl Reading<'_> {
    /// Takes the note out under the caller's lock (the caller wakes the waiters after dropping it).
    fn end_locked(&mut self, g: &mut Inner) {
        if let Some(r) = self.range.take()
            && let Some(at) = g.reading.iter().position(|&x| x == r)
        {
            g.reading.swap_remove(at);
        }
        self.cache.done.ended();
    }
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        if self.range.is_some() {
            let mut g = self.cache.inner.lock();
            self.end_locked(&mut g);
            drop(g);
            self.cache.done.wake();
        }
    }
}

/// PF-4: `read_direct`'s consumer: (page or None for a hole, offset in the page, bytes, bytes of the read
/// before them) -> bytes it took.
pub type ReadSink<'a> = dyn FnMut(Option<&CachePage>, usize, usize, usize) -> usize + 'a;

/// Where a write goes, from the locked state: (position, length).
type WriteAt<'a> = dyn Fn(&Inner) -> Result<(u64, usize), Errno> + 'a;

/// One batch of a write-back: dirty pages (flags cleared) with their file page numbers.
struct Batch {
    idx: Vec<u64>,
    pages: Vec<Arc<CachePage>>,
    size: u64,
    next: Option<u64>,
    oom: bool,
}

/// The cached content of one file.
pub struct PageCache {
    /// Shared (Arc) only with a queued read-ahead job, which holds it weakly.
    inner: Arc<Mutex<Inner>>,
    /// PF-4: the index and size as reads see them without `inner` (`cache_view`).
    view: Arc<Shared>,
    pool: Option<Arc<Pool>>,
    /// FS-14: pool pages are charged one by one when allocated (Linux VM_NORESERVE); false: by size.
    lazy: bool,
    backend: Option<Arc<dyn Backend>>,
    /// FS-10: a tmpfs file (seals apply); false for block file systems (F_GET_SEALS is EINVAL there).
    sealable: bool,
    sync_lock: Mutex<()>,
    /// FS-12: the backend's read-ahead maximum (pages; 0 = none).
    ra_max: u64,
    /// BL-3: the byte where the last read(2) of the file ended (u64::MAX: none yet), Linux's `ra->prev_pos`
    /// (linux-7/mm/filemap.c:2897; per open file there, per file here: a hint only, two readers of one file may
    /// mislead it into one window more or less).
    prev_pos: AtomicU64,
    /// FS-14b: fills that ended (under `vfs.inode.data`), and the threads that wait for one (`fill_with`).
    done: Arc<FillDone>,
    /// BL-4: the backend's largest request (`Backend::io_pages`; 0: as `ra_max`).
    io_pages: u64,
    /// BL-4: Linux `ra->mmap_miss`: mapping faults that missed minus those that hit (at most 10 x MMAP_LOTSAMISS).
    mmap_miss: AtomicU32,
    /// FS-15 fixer F2: never given to another cache (an address is, once this one is freed); the preload recorder
    /// keys its files by it.
    ident: u64,
    /// FS-STALL: this cache's id for frame owners (`reclaim::register`; 0: none).
    owner_id: u64,
    /// Host tests: a write-back batch takes at most `batch_cap - 1` pages, as if memory ran out (0 = no cap).
    #[cfg(test)]
    pub(crate) batch_cap: core::sync::atomic::AtomicUsize,
}

static NEXT_IDENT: AtomicU64 = AtomicU64::new(1);

impl PageCache {
    /// This cache's number, unique while the kernel runs.
    pub(crate) fn ident(&self) -> u64 {
        self.ident
    }

    /// A tmpfs file or memfd: no backend, each page charged to `pool` when it is allocated (FS-14).
    pub fn tmpfs(pool: Arc<Pool>) -> PageCache {
        Self::build(Some(pool), None, 0, true)
    }
    /// A kernel-made nameless tmpfs object (shared anonymous memory): charged to `pool` at size change, so a
    /// fault inside the size never meets an empty pool (Linux `shmem_acct_size` under overcommit mode 2).
    pub fn tmpfs_accounted(pool: Arc<Pool>) -> PageCache {
        Self::build(Some(pool), None, 0, false)
    }
    /// A file of a block file system of `size` bytes (its pages come from `backend`; not pool-charged).
    pub fn with_backend(backend: Arc<dyn Backend>, size: u64) -> PageCache {
        Self::build(None, Some(backend), size, false)
    }
    /// A whole block device's cache (`block::bdev::DiskFile`): its write-back lock is the class
    /// `vfs.cache.sync.bdev`, because a file system mounted on the device writes it back under its own volume
    /// lock, which a file's write-back (`vfs.cache.sync`) takes inside (FS-8c: ext4 on a disk; Linux's
    /// bdev mapping is a separate lock class from a regular file's for the same reason).
    pub fn for_block_device(backend: Arc<dyn Backend>, size: u64) -> PageCache {
        let mut c = Self::build(None, Some(backend), size, false);
        c.sync_lock = Mutex::named("vfs.cache.sync.bdev", ());
        c
    }
    fn build(pool: Option<Arc<Pool>>, backend: Option<Arc<dyn Backend>>, size: u64, lazy: bool) -> PageCache {
        // `memfd_file_seals_ptr`: only shmem (tmpfs) files have seals; a block file system's files do not.
        let sealable = pool.is_some() && backend.is_none();
        let pages = Pages::new(size);
        let view = pages.shared();
        let inner = Arc::new(Mutex::named("vfs.inode.data", Inner { size, charged: 0, pages, seals: if sealable { F_SEAL_SEAL } else { 0 }, writable_maps: 0, epoch: 0, ra: Ra::default(), reading: Vec::new(), clock: 0, preload_at: None, own_dirty: 0 }));
        // MM-9: a block file system's clean pages can be read again, so reclaim may take them; tmpfs pages
        // (the pool's) never go (no swap).
        let owner_id = if Self::may_reclaim(&pool, &backend) { crate::reclaim::register(&inner) } else { 0 };
        view.owner_id.store(owner_id, Ordering::Relaxed);
        PageCache {
            inner,
            view,
            ra_max: backend.as_ref().map_or(0, |b| b.read_ahead_pages()),
            io_pages: backend.as_ref().map_or(0, |b| b.io_pages()),
            prev_pos: AtomicU64::new(u64::MAX),
            lazy,
            sealable,
            pool,
            backend,
            sync_lock: Mutex::named("vfs.cache.sync", ()),
            done: Arc::new(FillDone { fills: AtomicU64::new(0), wq: WaitQueue::new() }),
            mmap_miss: AtomicU32::new(0),
            ident: NEXT_IDENT.fetch_add(1, Ordering::Relaxed),
            owner_id,
            #[cfg(test)]
            batch_cap: core::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Host tests: whether `vfs.inode.data` is free now (a backend checks it is not called under it).
    #[cfg(test)]
    pub(crate) fn data_lock_free(&self) -> bool {
        self.inner.try_lock().is_some()
    }

    /// Host tests (PF-4): holds `vfs.inode.data` (a read of cached pages must not wait for it).
    #[cfg(test)]
    pub(crate) fn hold_data_lock(&self) -> ksync::MutexGuard<'_, Inner> {
        self.inner.lock()
    }

    /// The locked state, for a reclaim ring (`reclaim::Ring`).
    pub(crate) fn inner_arc(&self) -> &Arc<Mutex<Inner>> {
        &self.inner
    }

    pub fn size(&self) -> u64 {
        self.inner.lock().size
    }
    /// Pages cached now.
    pub fn pages(&self) -> u64 {
        self.inner.lock().pages.len() as u64
    }
    /// Pages charged to the pool now (a lazily charged file: its cached pages; cut-off pages still mapped
    /// carry theirs outside the cache).
    pub fn charged(&self) -> u64 {
        let g = self.inner.lock();
        if self.lazy { g.pages.len() as u64 } else { g.charged }
    }
    /// FS-14: whether pages are charged one by one when allocated.
    pub fn lazy(&self) -> bool {
        self.lazy
    }
    pub fn has_backend(&self) -> bool {
        self.backend.is_some()
    }

    /// Pages one fill reads at least (the backend's window, 1 without a backend).
    pub fn read_window(&self) -> u64 {
        self.backend.as_ref().map_or(1, |b| b.read_window().clamp(1, MAX_WINDOW))
    }

    /// FS-15: the end of the run of cached pages that starts at `first`, at most `max` pages long (`first`
    /// when that page is not cached).
    pub fn cached_run(&self, first: u64, max: u64) -> u64 {
        let g = self.inner.lock();
        let stop = first.saturating_add(max).min(pages_for(g.size));
        let mut i = first;
        while i < stop && g.pages.contains(i) {
            i += 1;
        }
        i
    }

    fn may_reclaim(pool: &Option<Arc<Pool>>, backend: &Option<Arc<dyn Backend>>) -> bool {
        backend.is_some() && pool.is_none()
    }
    /// MM-9: whether reclaim may take this cache's clean pages (listed in `reclaim`).
    pub fn reclaimable(&self) -> bool {
        Self::may_reclaim(&self.pool, &self.backend)
    }

    /// FS-10: the seals (`memfd_get_seals`); None for a file without them (a block file system's: EINVAL).
    pub fn seals(&self) -> Option<u32> {
        self.sealable.then(|| self.inner.lock().seals)
    }

    /// FS-10: memfd_create's start seals (F_SEAL_SEAL cleared for MFD_ALLOW_SEALING, F_SEAL_EXEC set for
    /// MFD_NOEXEC_SEAL), before the file is reachable by anyone.
    pub(crate) fn set_initial_seals(&self, seals: u32) {
        if self.sealable {
            self.inner.lock().seals = seals;
        }
    }

    /// FS-10: F_ADD_SEALS after the fd checks (`memfd_add_seals`, linux-6.6/mm/memfd.c:178-208): EINVAL for a
    /// file without seals, EPERM once F_SEAL_SEAL is set, EBUSY for F_SEAL_WRITE while a shared mapping may
    /// write; F_SEAL_EXEC on a file with an exec bit adds the write seals too (W^X). `exec_bits`: the mode has an
    /// x bit. Returns the seals now in place.
    pub fn add_seals(&self, seals: u32, exec_bits: bool) -> Result<u32, Errno> {
        if !self.sealable {
            return Err(EINVAL);
        }
        let mut g = self.inner.lock();
        if g.seals & F_SEAL_SEAL != 0 {
            return Err(EPERM);
        }
        // `mapping_deny_writable`. Linux then waits up to ~150 ms for pages pinned by get_user_pages
        // (`memfd_wait_for_pins`); V2 has no such pins (direct I/O, io_uring), only mapping pins.
        if seals & F_SEAL_WRITE != 0 && g.seals & F_SEAL_WRITE == 0 && g.writable_maps > 0 {
            return Err(EBUSY);
        }
        let mut add = seals;
        if add & F_SEAL_EXEC != 0 && exec_bits {
            add |= F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE;
        }
        g.seals |= add;
        Ok(g.seals)
    }

    /// FS-10: a new MAP_SHARED mapping (`seal_check_write` + `mapping_map_writable` in Linux's mmap_region, as
    /// CachyOS's kernel does it; linux-6.6/include/linux/mm.h:4010 has the FUTURE_WRITE half). With a write seal
    /// a writable mapping of a writable fd is EPERM and a read-only one is mapped without VM_MAYWRITE (mprotect
    /// cannot add PROT_WRITE later); otherwise a writable fd's mapping may write and is counted until
    /// `unmap_writable`. Returns whether the mapping may write (and was counted).
    pub(crate) fn map_shared(&self, prot_write: bool, fd_write: bool) -> Result<bool, Errno> {
        let mut g = self.inner.lock();
        if g.seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0 {
            if fd_write && prot_write {
                return Err(EPERM);
            }
            return Ok(false);
        }
        if !fd_write {
            return Ok(false);
        }
        g.writable_maps += 1;
        Ok(true)
    }

    /// FS-10: a counted shared mapping is gone (its `FileMap` ended).
    pub(crate) fn unmap_writable(&self) {
        let mut g = self.inner.lock();
        g.writable_maps = g.writable_maps.saturating_sub(1);
    }

    /// FS-10: how much of a write of `len` bytes at `pos` (None: at the end) the seals let in (see
    /// `seal_write_limit`); for a whole request before it is copied in chunks.
    pub fn write_limit(&self, pos: Option<u64>, len: usize) -> Result<usize, Errno> {
        let g = self.inner.lock();
        seal_write_limit(g.seals, pos.unwrap_or(g.size), len, g.size)
    }

    /// FS-10: shared mappings that may write now (`mapping_writably_mapped`).
    pub fn writable_maps(&self) -> u64 {
        self.inner.lock().writable_maps
    }

    /// The page at `idx`, a new zeroed one when missing (never read from the backend: callers that need the
    /// stored content made sure it is cached first, `fill`).
    fn page_locked(&self, g: &mut Inner, idx: u64) -> Result<Arc<CachePage>, Errno> {
        if let Some(p) = g.pages.get(idx) {
            return Ok(p.clone());
        }
        self.insert_new(g, idx, false)
    }

    /// Inserts a new zeroed page at `idx` (missing there); `charged`: its page of charge is taken already.
    fn insert_new(&self, g: &mut Inner, idx: u64, charged: bool) -> Result<Arc<CachePage>, Errno> {
        let give_back = |c: bool| {
            if let Some(pool) = self.pool.as_ref().filter(|_| c) {
                pool.uncharge(1);
            }
        };
        if g.pages.prepare(idx).is_err() {
            give_back(charged);
            return Err(ENOMEM);
        }
        let p = self.new_page(charged)?;
        g.pages.insert(idx, p.clone()).map_err(|_| ENOMEM)?;
        Ok(p)
    }

    /// A new zeroed page. A lazily charged file's page takes its page of charge first and carries it until it
    /// is freed (`shmem_inode_acct_block`, linux-6.6/mm/shmem.c:210: ENOSPC whichever level refuses, the
    /// memory account or the mount's size).
    /// `charged`: the caller took that page of charge already (fallocate).
    fn new_page(&self, charged: bool) -> Result<Arc<CachePage>, Errno> {
        let pool = self.pool.as_ref().filter(|_| self.lazy);
        if let Some(pool) = pool.filter(|_| !charged) {
            pool.charge(1).map_err(|_| ENOSPC)?;
        }
        match CachePage::new(self.pool.as_ref()) {
            Some(p) => {
                if pool.is_some() {
                    p.carries_charge.store(true, Ordering::Release);
                }
                Ok(p)
            }
            None => {
                if let Some(pool) = pool {
                    pool.uncharge(1);
                }
                Err(ENOMEM)
            }
        }
    }

    /// Reads file page `idx` alone from the backend into the cache (`fill_with`).
    fn fill(&self, idx: u64) -> Result<(), Errno> {
        self.fill_with(idx, Reach::One).map(|_| ())
    }

    /// The pages [first, end) a fill of the missed page `idx` reads: inside the file, not cached, not being
    /// read by another fill, contiguous with `idx` (one device request), at most `MAX_WINDOW`.
    /// `over_bg`: pages only the preload reads count as free (a foreground miss that stopped waiting for it).
    fn window(g: &Inner, idx: u64, reach: Reach, win: u64, over_bg: bool) -> (u64, u64) {
        let n = pages_for(g.size);
        let (back, len, cap) = match reach {
            Reach::One => return (idx, idx + 1),
            Reach::Forward(want) => (0, want.max(win), u64::MAX),
            Reach::Request(want) => (0, want, u64::MAX),
            Reach::Around => (win / 2, win, u64::MAX),
            // BL-4: exactly the window's pages (the ones of it that are free and contiguous with `idx`).
            Reach::Window(first, len) => (idx - first.min(idx), len, first.min(idx).saturating_add(len).max(idx + 1)),
        };
        let len = len.clamp(1, MAX_WINDOW);
        let lowest = idx.saturating_sub(back);
        let mut first = idx;
        let free = |i: u64| if over_bg { !g.pages.contains(i) && g.in_flight_bg_only(i) } else { g.free(i) };
        while first > lowest && free(first - 1) {
            first -= 1;
        }
        let mut end = idx + 1;
        while end < n && end < cap && end - first < len && free(end) {
            end += 1;
        }
        (first, end)
    }

    /// FS-14b: reads the missed file page `idx` and, as `reach` says, its neighbours from the backend in one
    /// call, and keeps every page read in the cache (before FS-14b FAT read 32 pages per miss but kept only
    /// the missed one: Brave's cold start read its folder ~8 times over). **Without** `vfs.inode.data` held
    /// while the backend reads (B4: a slow device blocks only this caller). A page is inserted only if nobody
    /// cached it meanwhile and it still lies inside the file (a truncate while the read ran wins: `epoch`).
    /// The pages being read are noted in `Inner::reading`; a miss on one of them waits for that read (at most
    /// `FILL_WAIT_NS`, then reads its page itself), so two processes faulting the same code at once read it
    /// once. Callers take the lock again and look once more.
    /// Ok: the pages this call inserted (FS-13 counts them).
    pub(crate) fn fill_with(&self, idx: u64, reach: Reach) -> Result<u64, Errno> {
        self.fill_as(idx, reach, false)
    }

    /// `fill_with`; `bg` marks the preload's fill: a foreground miss of a page it reads waits only
    /// `BG_FILL_WAIT_NS` for it, then reads the page's window itself (the first insert wins).
    fn fill_as(&self, idx: u64, reach: Reach, bg: bool) -> Result<u64, Errno> {
        let Some(b) = &self.backend else { return Ok(0) };
        let win = b.read_window().clamp(1, MAX_WINDOW);
        let mut alone = false;
        let mut past_bg = false;
        let (first, end, epoch) = loop {
            let mut g = self.inner.lock();
            if idx >= pages_for(g.size) || g.pages.contains(idx) {
                return Ok(0);
            }
            if !alone && !past_bg && g.in_flight(idx) {
                let short = !bg && g.in_flight_bg_only(idx);
                let seen = self.done.fills.load(Ordering::Acquire);
                drop(g);
                FILL_WAITS.inc();
                let limit = if short { BG_FILL_WAIT_NS } else { FILL_WAIT_NS };
                if !self.done.wq.wait_until_timeout(|| self.done.fills.load(Ordering::Acquire) != seen, limit) {
                    if short {
                        FILL_WAIT_PAST_BG.inc();
                        past_bg = true;
                    } else {
                        FILL_WAIT_TIMEOUTS.inc();
                        klog::event!(Warn, "vfs", "cache.fill_wait_timeout", page = idx; "another read of this page took too long: read it here");
                        alone = true;
                    }
                }
                continue;
            }
            let (first, end) = if alone { (idx, idx + 1) } else { Self::window(&g, idx, reach, win, past_bg) };
            // Noted only if the list can grow; without the note a second reader may read the same pages.
            if g.reading.try_reserve(1).is_ok() {
                g.reading.push((first, end, bg));
            }
            break (first, end, g.epoch);
        };
        let mut done = Reading { cache: self, range: Some((first, end, bg)) };
        let (mut start, mut pages) = self.alloc_run(first, end, idx)?;
        FILL_READS.inc();
        if !bg {
            RA_SYNC.inc();
        }
        if let Err(e) = b.read_pages(start, &pages) {
            if pages.len() <= 1 {
                return Err(e);
            }
            // A bad neighbour must not fail a good page (filemap_fault reads the needed page alone after a
            // failed read-ahead, linux-6.6/mm/filemap.c:3368): the window is dropped, the page read by itself.
            FILL_WINDOW_ERRORS.inc();
            klog::event!(Warn, "vfs", "cache.fill_window_failed", page = idx, first = start, pages = pages.len() as u64, errno = e; "reading the page alone");
            drop(pages);
            (start, pages) = (idx, self.alloc_run(idx, idx + 1, idx)?.1);
            FILL_READS.inc();
            b.read_pages(start, &pages)?;
        }
        let read = (start, start + pages.len() as u64);
        let mut g = self.inner.lock();
        let mut res = Ok(());
        let mut inserted = 0u64;
        if g.epoch == epoch {
            let n = pages_for(g.size);
            for (k, p) in pages.into_iter().enumerate() {
                let i = start + k as u64;
                if i >= n || g.pages.contains(i) {
                    continue;
                }
                if bg {
                    // MM-9: unused preloaded pages are the first reclaim takes.
                    p.set_state(PRELOADED);
                    if g.preload_at.is_none() {
                        crate::reclaim::note_preloaded(&self.inner);
                    }
                    g.preload_at = Some(g.preload_at.map_or(i, |a| a.min(i)));
                }
                if g.pages.insert(i, p).is_err() {
                    if i <= idx {
                        res = Err(ENOMEM);
                    }
                    break;
                }
                FILL_PAGES.inc();
                inserted += 1;
            }
        }
        done.end_locked(&mut g);
        drop(g);
        self.done.wake();
        // FS-13: what this caller read from the device, for its program's preload record (the preload's own
        // reads are not a program's).
        if !bg {
            crate::preload::note_fill(self, read.0, read.1);
        }
        res.map(|()| inserted)
    }

    /// Zeroed pages for file pages [first, end): (first page, pages). When memory runs out part way, the run
    /// is cut after what was allocated if that holds `idx`, else it is `idx` alone (ENOMEM: not even that).
    fn alloc_run(&self, first: u64, end: u64, idx: u64) -> Result<(u64, Vec<Arc<CachePage>>), Errno> {
        let mut pages: Vec<Arc<CachePage>> = Vec::new();
        if pages.try_reserve_exact((end - first) as usize).is_ok() {
            for _ in first..end {
                match CachePage::new(self.pool.as_ref()) {
                    Some(p) => pages.push(p),
                    None => break,
                }
            }
            if first + pages.len() as u64 > idx {
                return Ok((first, pages));
            }
        }
        drop(pages);
        let p = CachePage::new(self.pool.as_ref()).ok_or(ENOMEM)?;
        let mut one = Vec::new();
        one.try_reserve_exact(1).map_err(|_| ENOMEM)?;
        one.push(p);
        Ok((idx, one))
    }

    /// The first page a write of `len` bytes at `pos` only partly covers whose stored content is still to be
    /// read (backend files; pages at or past the old end are zeros and need no read). At most two: the first
    /// and the last page of the range.
    fn needs_read(&self, g: &Inner, pos: u64, len: usize) -> Option<u64> {
        self.backend.as_ref()?;
        if len == 0 {
            return None;
        }
        let end = pos.saturating_add(len as u64);
        [pos / P, (end - 1) / P].into_iter().find(|&idx| {
            let start = idx * P;
            let whole = pos <= start && end >= start.saturating_add(P);
            !whole && start < g.size && !g.pages.contains(idx)
        })
    }

    /// `vfs.inode.data` held with every page a write needs to read already cached; `at` gives the write's
    /// (position, length) from the locked state (an append's position is the size).
    fn lock_for_write(&self, at: &WriteAt<'_>) -> Result<ksync::MutexGuard<'_, Inner>, Errno> {
        loop {
            let g = self.inner.lock();
            let (pos, len) = at(&g)?;
            let Some(idx) = self.needs_read(&g, pos, len) else { return Ok(g) };
            drop(g);
            self.fill(idx)?;
        }
    }

    /// FS-12, BL-4: a read or a fault that touched pages `[first, end)` reached the mark: opens the next window
    /// (`Inner::open_next`) and returns its job; while the file's job still runs, that job opens it when it ends
    /// (`Ra::follow`).
    fn ra_note(&self, g: &mut Inner, first: u64, end: u64) -> Option<readahead::Job> {
        if !g.ra.mark.is_some_and(|m| m >= first && m < end) {
            return None;
        }
        g.ra.mark = None;
        if g.ra.queued {
            g.ra.follow = true;
            g.sync_mark();
            return None;
        }
        let pages = pages_for(g.size);
        let max = self.ra_pages(g);
        let opened = g.open_next(pages, max);
        g.sync_mark();
        let w = opened?;
        Some(readahead::Job::new(Arc::downgrade(&self.inner), self.backend.clone()?, self.done.clone(), w, g.epoch, max))
    }

    /// The window limit for this file now: the backend's, twice that after POSIX_FADV_SEQUENTIAL.
    fn ra_pages(&self, g: &Inner) -> u64 {
        let ra = if g.ra.double { self.ra_max.saturating_mul(2) } else { self.ra_max };
        ra.min(MAX_WINDOW)
    }

    /// FS-12 for one step of a read that touched pages `[first, end)`: a step that does not reach the mark only
    /// notes where it ended (no data lock); the mark takes the lock for `ra_note` and queues its job.
    fn ra_step(&self, first: u64, end: u64) {
        // A step that touched no page (a miss right at its start) says nothing about where the reader is.
        if self.ra_max == 0 || end <= first {
            return;
        }
        let mark = self.view.mark();
        if !(mark != NO_MARK && mark >= first && mark < end) || !readahead::active() {
            // PF-7: readers of one place write nothing here; this word sits with what every reader loads.
            if self.view.ra_next.load(Ordering::Relaxed) != end {
                self.view.ra_next.store(end, Ordering::Relaxed);
            }
            return;
        }
        let mut g = self.inner.lock();
        let job = self.ra_note(&mut g, first, end);
        self.view.ra_next.store(end, Ordering::Relaxed);
        drop(g);
        if let Some(j) = job {
            self.queue_job(j);
        }
    }

    /// Queues a read-ahead job of this file (FS-13: noted for the reader's program's preload record).
    fn queue_job(&self, j: readahead::Job) {
        let range = (j.first, j.first + j.count);
        if readahead::queue(j) {
            crate::preload::note_fill(self, range.0, range.1);
        } else {
            self.inner.lock().ra.queued = false;
        }
    }

    /// BL-4: `page_cache_prev_miss` (linux-7/mm/filemap.c): the cached pages right before `idx`, at most `max`, and
    /// whether they reach back to page 0.
    fn history(g: &Inner, idx: u64, max: u64) -> (u64, bool) {
        let mut n = 0;
        let mut i = idx;
        while n < max {
            if i == 0 {
                return (n, true);
            }
            i -= 1;
            if !g.pages.contains(i) {
                return (n, false);
            }
            n += 1;
        }
        (n, false)
    }

    /// BL-4: `page_cache_sync_ra` (linux-7/mm/readahead.c:577) for a miss at `idx` of a read that wants `req` pages
    /// from there (`cont`: the miss lies inside the read). Sets the file's window and returns what the fill reads:
    /// the window, or the request alone for a standalone random read and after POSIX_FADV_RANDOM. The window is
    /// at least the backend's (`Backend::read_window`, FS-14b) and its mark stays where Linux puts it.
    /// Sequential: the file start, an oversized read, inside the read, where the last read(2) ended (or in its
    /// last page) or where the last read step ended (BL-3: the end of the last read keeps a stream sequential
    /// whose history pages went: fadvise DONTNEED after each piece, drop_caches, reclaim).
    fn sync_window(&self, g: &mut Inner, idx: u64, req: u64, cont: bool) -> Option<readahead::Window> {
        if g.ra.random {
            return None;
        }
        let max = readahead::max_pages(self.ra_pages(g), self.io_pages, req);
        let prev = self.prev_pos.load(Ordering::Relaxed);
        let after_last = prev != u64::MAX && idx.wrapping_sub(prev / P) <= 1;
        let w = if idx == 0 || cont || req > max || after_last || idx == self.view.ra_next.load(Ordering::Relaxed) {
            readahead::Window::sync_seq(idx, req, max)
        } else {
            let (contig, from_start) = Self::history(g, idx, max);
            readahead::Window::sync_history(idx, req, contig, from_start, max)?
        };
        let w = w.at_least(self.read_window().min(max.max(1)));
        g.set_window(w, pages_for(g.size));
        Some(w)
    }

    /// The reach of the fill for a read(2) miss at `idx` wanting `req` pages (`sync_window`; without read-ahead
    /// in the backend the request and the backend's window, as before BL-4).
    fn miss_reach(&self, idx: u64, req: u64, cont: bool) -> Reach {
        if self.ra_max == 0 {
            return Reach::Forward(req);
        }
        let mut g = self.inner.lock();
        // Being read already (a job's window or another reader's fill: Linux finds the locked folio in the cache,
        // filemap.c `filemap_get_pages`): no new window, the fill waits for that read; reaching the mark there
        // starts the next window as a cached page would.
        if g.in_flight(idx) {
            let job = if readahead::active() { self.ra_note(&mut g, idx, idx + 1) } else { None };
            drop(g);
            if let Some(j) = job {
                self.queue_job(j);
            }
            return Reach::Request(req);
        }
        match self.sync_window(&mut g, idx, req, cont) {
            Some(w) => Reach::Window(w.start, w.size),
            None => {
                RA_RANDOM.inc();
                Reach::Request(req)
            }
        }
    }

    /// BL-4: read-ahead for a mapping's fault on file page `pgoff` (linux-7/mm/filemap.c:3555 `filemap_fault`),
    /// before MM takes the page (`page_for_map`, which then finds it cached). A cached page: `do_async_mmap_readahead`
    /// (:3452): the mark starts the next window, a hit counts against `mmap_miss`. A missing page:
    /// `do_sync_mmap_readahead` (:3329): MADV_RANDOM reads the page alone; MADV_SEQUENTIAL the read path's window;
    /// code (PROT_EXEC) `ra_pages` forward from the page inside the VMA, without a mark; else the read-around
    /// window (`ra_pages` centred on the page, a quarter of it async), unless the file's faults miss far more
    /// than they hit (`MMAP_LOTSAMISS`: the page alone). MM runs a missing page's call without the faulting
    /// space's lock (`reads_for_fault`; Linux `maybe_unlock_mmap_for_io`); the async window never waits (a job).
    /// A backend without read-ahead reads the page's window as `page_for_map` would (`Reach::Around`).
    pub fn fault_ra(&self, pgoff: u64, h: &MapHint) {
        if self.backend.is_none() {
            return;
        }
        if self.ra_max == 0 {
            if self.view.get(pgoff).is_none() {
                // An error here is the fault's: `page_for_map` reads the page again and answers it.
                let _ = self.fill_with(pgoff, Reach::Around);
            }
            return;
        }
        if self.view.get(pgoff).is_some() {
            if h.random {
                return;
            }
            if !h.sequential && !h.exec {
                let m = self.mmap_miss.load(Ordering::Relaxed);
                if m > 0 {
                    self.mmap_miss.store(m - 1, Ordering::Relaxed);
                }
            }
            if self.view.mark() == pgoff && readahead::active() {
                let mut g = self.inner.lock();
                let job = self.ra_note(&mut g, pgoff, pgoff + 1);
                drop(g);
                if let Some(j) = job {
                    RA_MMAP_ASYNC.inc();
                    self.queue_job(j);
                }
            }
            return;
        }
        if h.random && !h.exec {
            RA_RANDOM.inc();
            let _ = self.fill_with(pgoff, Reach::One);
            return;
        }
        let mut g = self.inner.lock();
        let pages = pages_for(g.size);
        if pgoff >= pages {
            return;
        }
        // Being read (see `miss_reach`): its mark starts the next window; the fault waits for that read here,
        // without the space's lock (`fill_with` waits for a running fill), not in `page_for_map`.
        if g.in_flight(pgoff) {
            let job = if h.random || !readahead::active() { None } else { self.ra_note(&mut g, pgoff, pgoff + 1) };
            drop(g);
            if let Some(j) = job {
                RA_MMAP_ASYNC.inc();
                self.queue_job(j);
            }
            let _ = self.fill_with(pgoff, Reach::One);
            return;
        }
        let ra = self.ra_pages(&g);
        // MADV_SEQUENTIAL is `page_cache_sync_ra(ra_pages)`: where it finds no stream (a standalone read, or
        // POSIX_FADV_RANDOM) it still reads `ra_pages` forward, without a window (readahead.c:603, :640).
        let mut alone = Reach::One;
        let w = if h.sequential {
            alone = Reach::Request(ra);
            self.sync_window(&mut g, pgoff, ra, false)
        } else if h.exec {
            let start = pgoff.max(h.first);
            let end = start.saturating_add(ra).min(h.end.max(start + 1));
            let w = readahead::Window { start, size: end - start, async_size: 0 };
            g.set_window(w, pages);
            Some(w)
        } else {
            let m = self.mmap_miss.load(Ordering::Relaxed);
            if m < MMAP_LOTSAMISS * 10 {
                self.mmap_miss.store(m + 1, Ordering::Relaxed);
            }
            if m + 1 > MMAP_LOTSAMISS {
                RA_LOTSAMISS.add(1);
                if m == MMAP_LOTSAMISS {
                    klog::event!(Debug, "vfs", "readahead.mmap_lotsamiss", cache = self.ident, page = pgoff; "this file's mapping faults miss far more than they hit: no read-around");
                }
                None
            } else {
                let w = readahead::Window { start: pgoff.saturating_sub(ra / 2), size: ra, async_size: ra / 4 };
                g.set_window(w, pages);
                Some(w)
            }
        };
        drop(g);
        let reach = match w {
            Some(w) => {
                RA_MMAP_SYNC.inc();
                Reach::Window(w.start, w.size)
            }
            None => alone,
        };
        // An error here is the fault's: `page_for_map` reads the page again and answers it.
        let _ = self.fill_with(pgoff, reach);
    }

    /// BL-4 (fixer): whether a mapping's fault on file page `pgoff` reads the file (`fault_ra` then waits for the
    /// device): the file has a backend and the page is not cached. MM then calls `fault_ra` without its lock.
    pub fn reads_for_fault(&self, pgoff: u64) -> bool {
        self.backend.is_some() && self.view.get(pgoff).is_none()
    }

    /// BL-4 (fault-around, linux-7/mm/filemap.c:3899 `filemap_map_pages`): whether a mapping may map file page
    /// `pgoff` without a fault: cached now and not the read-ahead mark (whose fault starts the next window).
    pub fn ready_for_map(&self, pgoff: u64) -> bool {
        self.view.mark() != pgoff && self.view.get(pgoff).is_some()
    }

    /// BL-4: posix_fadvise NORMAL, RANDOM and SEQUENTIAL (linux-7/mm/fadvise.c:79-95), for every reader of the
    /// file (per open file in Linux). RANDOM also drops the mark, so no window starts from an old one.
    pub fn advise(&self, a: Advice) {
        if self.ra_max == 0 {
            return;
        }
        let mut g = self.inner.lock();
        match a {
            Advice::Normal => (g.ra.random, g.ra.double) = (false, false),
            Advice::Random => {
                g.ra.random = true;
                g.ra.mark = None;
                g.ra.follow = false;
                g.sync_mark();
            }
            Advice::Sequential => (g.ra.random, g.ra.double) = (false, true),
        }
        drop(g);
        klog::event!(Debug, "vfs", "readahead.advice", cache = self.ident, advice = a as u8);
    }

    /// Reads into `buf` at `pos` (kernel callers, the bounce path of the read syscalls): cached pages lockless
    /// (`read_quick`), the rest through `read_direct`.
    pub fn read_at(&self, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        let (got, stop) = self.read_quick(pos, buf.len(), &mut |page, off, n, done| {
            let out = &mut buf[done..done + n];
            match page {
                Some(p) => p.read(off, out),
                None => out.fill(0),
            }
            n
        });
        if got == buf.len() || stop == Stop::Eof {
            return Ok(got);
        }
        let (pos, buf) = (pos + got as u64, &mut buf[got..]);
        let r = self.read_direct(pos, buf.len(), &mut |page, off, n, done| {
            let out = &mut buf[done..done + n];
            match page {
                Some(p) => p.read(off, out),
                None => out.fill(0),
            }
            n
        });
        match r {
            Ok((n, _)) => Ok(got + n),
            Err(_) if got > 0 => Ok(got),
            Err(e) => Err(e),
        }
    }

    /// PF-5: reads up to `len` bytes at `pos` from cached pages with no lock and no reference, step by step
    /// (`Shared::quick`: one read section per `BATCH` pages), through `sink`, which must not sleep, fault or
    /// take a lock (it runs inside the section). Ends at the end of the file, at a page not cached (a
    /// backend's: `read_direct` reads it), when the sink takes less, or when a step raced a truncate or a punch
    /// (`Stop::Retry`: that step's bytes do not count; the caller goes on with `read_direct` from the bytes
    /// returned, which copies them again). Returns (bytes taken, why it stopped); `Stop::More` only when `len`
    /// bytes were taken.
    pub fn read_quick(&self, pos: u64, len: usize, sink: &mut QuickSink<'_>) -> (usize, Stop) {
        let mut done = 0usize;
        let backend = self.backend.is_some();
        while done < len {
            let here = pos + done as u64;
            let (n, stop) = self.view.quick(here, len - done, backend, &mut |p, off, k, d| sink(p, off, k, done + d));
            if n > 0 {
                self.ra_step(here / P, (here + n as u64).div_ceil(P));
            }
            done += n;
            match stop {
                Stop::More => {}
                Stop::Retry => {
                    QUICK_RETRIES.inc();
                    return (done, stop);
                }
                _ => return (done, stop),
            }
        }
        (done, Stop::More)
    }

    /// PF-4: reads up to `len` bytes at `pos` through `sink(page, off, n, done)`, which takes `n` bytes of `page`
    /// from `off` (None: a hole, `n` zeros) as the bytes `done..done + n` of the read and answers how many it
    /// took. Without `vfs.inode.data`: each step looks up to `BATCH` pages up under `vfs.inode.index` (the size
    /// of the same moment clips it), keeps them by their `Arc` and copies with no lock held, so readers of one
    /// file run in parallel and a page truncate cuts off meanwhile stays alive until its copy is done (its bytes
    /// may be stale then, as a racing read's are on Linux). A missing page of a file with a backend is read
    /// (`fill_with`, as before) and the step looked up again. Ok((bytes, short)): `short` when the sink took
    /// fewer bytes than offered (a user fault): the read ends there. Err only when nothing was read.
    pub fn read_direct(&self, pos: u64, len: usize, sink: &mut ReadSink<'_>) -> Result<(usize, bool), Errno> {
        let r = self.read_direct_steps(pos, len, sink);
        // Only for a backend with its own read-ahead (the rule below), and only when the page changes: parallel
        // readers of one file (PF-4) do not write the line for every read.
        if let Ok((done, _)) = r
            && done > 0
            && self.ra_max > 0
        {
            let end = pos + done as u64;
            if self.prev_pos.load(Ordering::Relaxed) / P != end / P {
                self.prev_pos.store(end, Ordering::Relaxed);
            }
        }
        r
    }

    fn read_direct_steps(&self, pos: u64, len: usize, sink: &mut ReadSink<'_>) -> Result<(usize, bool), Errno> {
        let end = pos.saturating_add(len as u64);
        let mut done = 0usize;
        loop {
            let here = pos + done as u64;
            if done == len {
                return Ok((done, false));
            }
            let first = here / P;
            let snap = self.view.snap(first, end, self.backend.is_some());
            if here >= snap.size {
                return Ok((done, false));
            }
            let n = len.min((snap.size - pos).min(usize::MAX as u64) as usize);
            for page in snap.pages[..snap.len].iter() {
                if done >= n {
                    break;
                }
                let off = ((pos + done as u64) % P) as usize;
                let k = (PAGE - off).min(n - done);
                if let Some(p) = page {
                    p.touch();
                }
                let got = sink(page.as_deref(), off, k, done);
                done += got.min(k);
                if got < k {
                    return Ok((done, true));
                }
            }
            let missing = snap.missing.filter(|_| done < n);
            drop(snap);
            let stop = missing.unwrap_or_else(|| (pos + done as u64).div_ceil(P));
            self.ra_step(first, stop);
            let Some(idx) = missing else {
                if done >= n {
                    return Ok((done, false));
                }
                continue;
            };
            // BL-4: the miss reads its window here (`miss_reach`, Linux `page_cache_sync_ra`); a backend without
            // read-ahead (FAT) reads the pages the call wants and its window (FS-14b).
            let req = (pos + n as u64).div_ceil(P).saturating_sub(idx).max(1);
            let reach = self.miss_reach(idx, req, done > 0);
            if let Err(e) = self.fill_with(idx, reach) {
                return if done == 0 { Err(e) } else { Ok((done, false)) };
            }
        }
    }

    /// Writes `buf` at `pos` (the caller checked MAX_FILE_SIZE and the freeze state). A size-charged pool file
    /// grows its charge first, a lazily charged one charges each new page; what the pool refuses is a short
    /// write, or the error when nothing fits (ENOSPC for a lazy file, as shmem's write_begin answers).
    pub fn write_at(&self, pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        let mut g = self.lock_for_write(&|_| Ok((pos, buf.len())))?;
        self.write_locked(&mut g, pos, buf)
    }

    /// Appends at the end taken under the lock; returns (bytes, new end).
    pub fn append(&self, buf: &[u8], max_size: u64) -> Result<(usize, u64), Errno> {
        let len = |end: u64| buf.len().min((max_size.saturating_sub(end)).min(usize::MAX as u64) as usize);
        let mut g = self.lock_for_write(&|g| {
            crate::inode::check_limit(g.size, buf.len())?;
            Ok((g.size, len(g.size)))
        })?;
        let end = g.size;
        let w = self.write_locked(&mut g, end, &buf[..len(end)])?;
        Ok((w, end + w as u64))
    }

    fn write_locked(&self, g: &mut Inner, pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        if buf.is_empty() {
            return Ok(0);
        }
        // FS-10 (`shmem_write_begin`, linux-6.6/mm/shmem.c:2680): seals before any space is charged.
        let buf = &buf[..seal_write_limit(g.seals, pos, buf.len(), g.size)?];
        let want_end = pos.checked_add(buf.len() as u64).ok_or(EFBIG)?;
        let mut limit = want_end;
        let mut first_err = None;
        if let Some(pool) = self.pool.as_ref().filter(|_| !self.lazy) {
            let need = pages_for(want_end).saturating_sub(g.charged);
            if need > 0 {
                // The pages up to and including the one `pos` lies in are the least that lets any byte in.
                let least = pages_for(pos + 1).saturating_sub(g.charged);
                let (got, err) = pool.charge_upto(least, need);
                g.charged += got;
                first_err = err;
                limit = want_end.min(g.charged * P);
                if limit <= pos {
                    self.settle_charge(g);
                    return Err(err.unwrap_or(ENOSPC));
                }
            }
        }
        let n = (limit - pos) as usize;
        let mut prepaid = self.prepay(g, pos, limit);
        let mut done = 0;
        while done < n {
            let p = pos + done as u64;
            let (idx, off) = (p / P, (p % P) as usize);
            let k = (PAGE - off).min(n - done);
            let page = if prepaid > 0 && !g.pages.contains(idx) {
                prepaid -= 1;
                self.insert_new(g, idx, true)
            } else {
                self.page_locked(g, idx)
            };
            match page {
                Ok(page) => {
                    page.touch();
                    page.write(off, &buf[done..done + k]);
                    if self.backend.is_some() {
                        if page.make_dirty() {
                            g.own_dirty += 1;
                        }
                    }
                }
                Err(e) => {
                    first_err = Some(e);
                    break;
                }
            }
            done += k;
        }
        if let Some(pool) = &self.pool {
            pool.uncharge(prepaid);
        }
        if pos + done as u64 > g.size {
            g.set_size(pos + done as u64);
        }
        self.settle_charge(g);
        if done == 0 {
            return Err(first_err.unwrap_or(ENOSPC));
        }
        Ok(done)
    }

    /// A lazy file's write of `[pos, end)`: the charge of its missing pages taken in one go (one commit question
    /// instead of one per page, e.g. the initramfs's 11 000 pages at boot); 0 when the pool refuses the whole,
    /// and the pages are then charged one by one, which finds the exact end of a short write.
    fn prepay(&self, g: &Inner, pos: u64, end: u64) -> u64 {
        let Some(pool) = self.pool.as_ref().filter(|_| self.lazy) else { return 0 };
        if end <= pos {
            return 0;
        }
        let (first, last) = (pos / P, pages_for(end));
        let mut present = 0u64;
        let mut at = first;
        while let Some((i, _)) = g.pages.next_from(at).filter(|&(i, _)| i < last) {
            present += 1;
            at = i + 1;
        }
        let missing = (last - first).saturating_sub(present);
        if missing > 1 && pool.charge(missing).is_ok() { missing } else { 0 }
    }

    /// Gives back what a pool file holds beyond the pages its size covers (after a short write).
    fn settle_charge(&self, g: &mut Inner) {
        if let Some(pool) = self.pool.as_ref().filter(|_| !self.lazy) {
            let keep = pages_for(g.size);
            if g.charged > keep {
                pool.uncharge(g.charged - keep);
                g.charged = keep;
            }
        }
    }

    /// ftruncate/truncate/O_TRUNC: growing only sets the size for a lazily charged file (`shmem_setattr`,
    /// linux-6.6/mm/shmem.c:1153: i_size only; pages are charged when allocated) and charges the new pages
    /// first for a size-charged one (all or nothing: ENOMEM from the system pool, ENOSPC from the mount);
    /// shrinking drops the pages past the end (`Inode::truncate` has taken them out of every mapping first,
    /// MM-rmap; one a fault mapped again meanwhile stays alive in its mapping until the second unmap after the cut),
    /// zeroes the tail of the new last page and gives the charge back.
    ///
    /// With a backend it first waits for a write-back of this file in flight (`vfs.cache.sync`, as Linux's
    /// truncate_inode_pages waits on pages under writeback): a batch collected before the truncate carries the
    /// old size and would otherwise write the cut-off pages back after the file system shrank the file (FS-7).
    pub fn truncate(&self, size: u64) -> Result<(), Errno> {
        let _wb = self.backend.as_ref().map(|_| self.sync_lock.lock());
        let mut g = self.inner.lock();
        // FS-10 (`shmem_setattr`, linux-6.6/mm/shmem.c:1177): F_SEAL_SHRINK/F_SEAL_GROW before the charge.
        if (size < g.size && g.seals & F_SEAL_SHRINK != 0) || (size > g.size && g.seals & F_SEAL_GROW != 0) {
            return Err(EPERM);
        }
        if let Some(pool) = self.pool.as_ref().filter(|_| !self.lazy) {
            let want = pages_for(size);
            if want > g.charged {
                pool.charge(want - g.charged)?;
                g.charged = want;
            }
        }
        // `shmem_setattr` (newsize <= oldsize): also a truncate to the same size drops fallocate(KEEP_SIZE)'s
        // pages past the end.
        if size < g.size || (size == g.size && g.pages.next_from(pages_for(size)).is_some()) {
            g.epoch = g.epoch.wrapping_add(1);
            // PF-4: the size first, then the pages (`truncate_setsize` before `truncate_pagecache`): a read
            // without this lock never finds a cut-off page missing inside the old size (it would read zeros).
            g.set_size(size);
            let keep = pages_for(size);
            // The pages leave the index in steps, and each step's pages are freed before the next. The room for a
            // step is made here, once: the drain's callback runs under the index's write side (a spin lock), where
            // the heap can neither reclaim nor use single frames. 11.11.2026, Mini-PC, a full page cache: one list
            // for all pages grew in the callback, and past 2 MiB every further page was one refused request
            // (70 000 a second for seconds, under this lock) while Steam cut its download's files.
            let mut cut: Vec<Gone> = Vec::new();
            let step = if cut.try_reserve_exact(CUT_STEP).is_ok() { CUT_STEP } else { 1 };
            let mut carried = 0u64;
            let (pool_file, lazy) = (self.pool.is_some(), self.lazy);
            loop {
                let more = g.pages.drain_some(keep, step, &mut |_, p| {
                    // A pool page some mapping still pins keeps its frame: it keeps one page of the charge too and
                    // gives it back when its last pin goes, so the pool bound holds for frames really held (R2).
                    // No new reference can appear once it left the index, so a count of 1 stays 1.
                    // A lazy file's page carries its charge already.
                    if pool_file && p.strong_count() > 1 {
                        if !lazy {
                            p.carries_charge.store(true, Ordering::Release);
                        }
                        carried += 1;
                    }
                    // Without room (no memory for the step's list) the page goes to the grace queue from here.
                    if cut.len() < cut.capacity() {
                        cut.push(p);
                    }
                });
                // Outside the write side: a page's last reference frees its frame after a grace period, and above
                // the queue's bound this thread waits for it (as `empty_dropped` does under this lock).
                cut.clear();
                if !more {
                    break;
                }
            }
            if carried > 0 {
                if !lazy {
                    g.charged = g.charged.saturating_sub(carried);
                }
                CARRIED.add(carried);
                klog::event!(Debug, "vfs", "cache.truncate_mapped", pages = carried;
                    "truncate cut off pages still mapped: they keep their pool charge until unmapped");
            }
            if size % P != 0
                && let Some(page) = g.pages.get(size / P)
            {
                page.frame.zero_from((size % P) as usize);
            }
            g.set_size(size);
            self.settle_charge(&mut g);
            drop(g);
            return Ok(());
        }
        g.set_size(size);
        Ok(())
    }

    /// RB-18: a file that holds a secret (CONFIG.TXT): every page in the cache is marked to be zeroed when its frame
    /// goes back, and the clean ones no one else holds leave the cache now (their frames after a grace period: a
    /// lockless reader may still copy, as with reclaim). Dirty and mapped pages stay, marked: they hold the file's only
    /// current content or are in use. The file reads back from its backend. Returns the pages that stayed.
    pub fn forget_clean(&self) -> u64 {
        if self.backend.is_none() {
            // A tmpfs file: the cache is its content.
            return self.pages();
        }
        let _wb = self.sync_lock.lock();
        let g = self.inner.lock();
        let (mut at, mut stayed) = (0, 0);
        while let Some((i, dirty)) = g.pages.next_with(at, |_, p| {
            p.wipe.store(true, Ordering::Release);
            p.is_dirty()
        }) {
            at = i + 1;
            match if dirty { None } else { g.pages.take_alone(i) } {
                Some(page) => page.drop_nowait(),
                None => stayed += 1,
            }
        }
        stayed
    }

    /// fallocate on a tmpfs file after `vfs_fallocate`'s checks (`shmem_fallocate`, linux-6.6/mm/shmem.c:2995-3130;
    /// `offset + len` fits MAX_FILE_SIZE). Block file systems' files have no fallocate (EOPNOTSUPP; glibc's
    /// posix_fallocate then writes zeros itself), nor does any mode but KEEP_SIZE and PUNCH_HOLE.
    ///
    /// - PUNCH_HOLE (always with KEEP_SIZE): EPERM under F_SEAL_WRITE or F_SEAL_FUTURE_WRITE (F_SEAL_SHRINK does
    ///   not stop it, as in Linux); the range reads as zeros afterwards (`punch_locked`).
    /// - Allocation (mode 0, KEEP_SIZE): EPERM under F_SEAL_GROW when the range reaches past the end; ENOSPC when
    ///   the range alone is larger than the mount (`max_blocks`). FS-14, a lazily charged file: every missing
    ///   page of the range is allocated and charged now (SGP_FALLOC), past the end too with KEEP_SIZE (they
    ///   count in st_blocks, as on Linux); when one is refused the pages this call added go again
    ///   (`shmem_undo_range`) and the call answers ENOSPC (ENOMEM: no frame). A size-charged file (R2): a
    ///   charged page can never fail to come at a fault, so the charge *is* the preallocation: mode 0 grows the
    ///   size and charges the new pages (all or nothing, ENOSPC as shmem's accounting answers), the pages
    ///   inside the old size are charged already, and KEEP_SIZE past the end only asks whether the pages would
    ///   fit now (ENOSPC if not) without holding them (Linux holds them beyond i_size; V2 charges at growth).
    pub fn fallocate(&self, mode: u32, offset: u64, len: u64) -> Result<(), Errno> {
        use crate::abi::{FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE};
        let Some(pool) = self.pool.as_ref().filter(|_| self.sealable) else { return Err(EOPNOTSUPP) };
        if mode & !(FALLOC_FL_KEEP_SIZE | FALLOC_FL_PUNCH_HOLE) != 0 {
            return Err(EOPNOTSUPP);
        }
        let end = offset.checked_add(len).ok_or(EFBIG)?;
        let mut g = self.inner.lock();
        if mode & FALLOC_FL_PUNCH_HOLE != 0 {
            if g.seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0 {
                return Err(EPERM);
            }
            self.punch_locked(&mut g, offset, end);
            return Ok(());
        }
        if g.seals & F_SEAL_GROW != 0 && end > g.size {
            return Err(EPERM);
        }
        if let Some(limit) = pool.size()
            && pages_for(end) - offset / P > limit
        {
            return Err(ENOSPC);
        }
        if self.lazy {
            return self.falloc_lazy(&mut g, mode, offset, end);
        }
        if end <= g.size {
            return Ok(());
        }
        // shmem's accounting refusal (`shmem_inode_acct_blocks`) is ENOSPC, whichever level said no.
        let nospc = |e: Errno| if e == ENOMEM { ENOSPC } else { e };
        let need = pages_for(end).saturating_sub(g.charged);
        pool.charge(need).map_err(nospc)?;
        if mode & FALLOC_FL_KEEP_SIZE != 0 {
            pool.uncharge(need);
            return Ok(());
        }
        g.charged += need;
        g.set_size(end);
        Ok(())
    }

    /// FS-14: fallocate's allocation on a lazily charged file (see `fallocate`), under `vfs.inode.data`. The
    /// missing pages are charged in one go first (all or nothing, so a hostile range is refused before any
    /// frame is taken), then made; the pages added are remembered as runs of indices (one run for an empty
    /// range), so an undo after a missing frame needs no list per page.
    fn falloc_lazy(&self, g: &mut Inner, mode: u32, offset: u64, end: u64) -> Result<(), Errno> {
        use crate::abi::FALLOC_FL_KEEP_SIZE;
        let Some(pool) = &self.pool else { return Err(EOPNOTSUPP) };
        let (first, last) = (offset / P, pages_for(end));
        let mut present = 0u64;
        let mut at = first;
        while let Some((i, _)) = g.pages.next_from(at).filter(|&(i, _)| i < last) {
            present += 1;
            at = i + 1;
        }
        let mut missing = (last - first).saturating_sub(present);
        pool.charge(missing).map_err(|_| ENOSPC)?;
        let mut runs: Vec<(u64, u64)> = Vec::new();
        let mut failed = None;
        for idx in first..last {
            if missing == 0 {
                break;
            }
            if g.pages.contains(idx) {
                continue;
            }
            missing -= 1;
            if runs.try_reserve(1).is_err() {
                failed = Some(ENOMEM);
                pool.uncharge(1);
                break;
            }
            if let Err(e) = self.insert_new(g, idx, true) {
                failed = Some(e);
                break;
            }
            match runs.last_mut() {
                Some(r) if r.1 == idx => r.1 = idx + 1,
                _ => runs.push((idx, idx + 1)),
            }
        }
        if let Some(e) = failed {
            pool.uncharge(missing);
            for &(a, b) in &runs {
                for i in a..b {
                    g.pages.remove(i);
                }
            }
            return Err(e);
        }
        if mode & FALLOC_FL_KEEP_SIZE == 0 && end > g.size {
            g.set_size(end);
        }
        Ok(())
    }

    /// The hole punch of [start, end) (`shmem_truncate_range`): a page wholly inside it leaves the cache (a hole
    /// again, its frame freed) unless a mapping still pins it; then, as a page only partly inside, its bytes
    /// in the range are zeroed in place. `Inode::fallocate` has taken the whole pages out of every mapping first
    /// (MM-rmap, as Linux's `unmap_mapping_range` in `shmem_fallocate`), so only a page a fault mapped again
    /// between that unmap and this punch is still pinned (Linux closes the gap with `shmem_falloc` waiters):
    /// it stays at its index, zeroed, until the next unmap or punch, so at most one page per index of the hole
    /// (a private copy made before the punch keeps its data in both). A size-charged file keeps its charge (it
    /// follows the size); a lazily charged file's freed page gives its own back (FS-14).
    ///
    /// PF-4: a page only reads hold for their copy (no data lock) is no pin: it leaves the index as a free one
    /// does (Linux's punch drops a folio a reader holds; the read copies the old bytes, as any racing read may),
    /// so SEEK_HOLE and the charge see the hole at once. Its frame lives until the last read ends; a
    /// size-charged file's page then carries one page of charge of its own meanwhile (R2: frames really
    /// held), or, when the pool refuses that page, it stays zeroed in place as a pinned one.
    fn punch_locked(&self, g: &mut Inner, start: u64, end: u64) {
        if end <= start {
            return;
        }
        g.epoch = g.epoch.wrapping_add(1);
        let (first, last) = (start / P, (end - 1) / P);
        let mut at = first;
        while let Some((i, ())) = g.pages.next_with(at, |_, _| ()) {
            if i > last {
                break;
            }
            let lo = if i == first { (start % P) as usize } else { 0 };
            let hi = if i == last { ((end - 1) % P) as usize + 1 } else { PAGE };
            let taken = lo == 0 && hi == PAGE && self.take_punched(g, i);
            if !taken && let Some(p) = g.pages.get(i) {
                p.frame.zero_range(lo, hi - lo);
            }
            match i.checked_add(1) {
                Some(n) => at = n,
                None => break,
            }
        }
    }

    /// `punch_locked`'s whole page `i`: true when it left the index. A read-held page of a size-charged file
    /// takes its own page of charge first (the file's charge follows the size; the frame outlives the cut).
    fn take_punched(&self, g: &mut Inner, i: u64) -> bool {
        let size_pool = self.pool.as_ref().filter(|_| !self.lazy);
        if size_pool.is_none() {
            return g.pages.take_unpinned(i).is_some();
        }
        if let Some(p) = g.pages.take_alone(i) {
            drop(p);
            return true;
        }
        let Some(pool) = size_pool else { return false };
        if pool.charge(1).is_err() {
            return false;
        }
        match g.pages.take_unpinned(i) {
            Some((p, _)) => {
                // Taken under the write side, so no read can hold it anew; a read that ended meanwhile left
                // it alone, and its drop here gives the page back at once.
                p.carries_charge.store(true, Ordering::Release);
                READ_HELD_PUNCHED.inc();
                true
            }
            None => {
                pool.uncharge(1);
                false
            }
        }
    }

    /// SEEK_DATA / SEEK_HOLE: holes are whole missing pages (tmpfs). With a backend every byte is data (the
    /// file system knows its holes; FS-8 may refine this).
    pub fn seek_data(&self, pos: u64, hole: bool) -> Option<u64> {
        let g = self.inner.lock();
        if pos >= g.size {
            return None;
        }
        if self.backend.is_some() {
            return Some(if hole { g.size } else { pos });
        }
        let mut p = pos;
        loop {
            if p >= g.size {
                return if hole { Some(g.size) } else { None };
            }
            let present = g.pages.contains(p / P);
            if present != hole {
                return Some(p);
            }
            p = (p / P + 1) * P;
            if !hole {
                match g.pages.next_from(p / P) {
                    Some((i, _)) => p = (i * P).max(p),
                    None => return None,
                }
            }
        }
    }

    /// The page a mapping maps at file page `pgoff` (made when missing). Past the end of the file: `Bus`; a
    /// lazily charged file's new page the pool refuses: `Bus` too (`shmem_fault`: ENOSPC is VM_FAULT_SIGBUS).
    /// `write`: a write through a shared mapping (the page becomes dirty for write-back).
    pub fn page_for_map(&self, pgoff: u64, write: bool) -> Result<Arc<CachePage>, PageFault> {
        let as_fault = |e| match e {
            ENOMEM => PageFault::NoMemory,
            ENOSPC => {
                NO_CHARGE_FAULTS.inc();
                PageFault::Bus
            }
            _ => PageFault::Io,
        };
        loop {
            let mut g = self.inner.lock();
            if pgoff >= pages_for(g.size) {
                return Err(PageFault::Bus);
            }
            if self.backend.is_some() && !g.pages.contains(pgoff) {
                drop(g);
                self.fill_with(pgoff, Reach::Around).map_err(as_fault)?;
                continue;
            }
            let p = self.page_locked(&mut g, pgoff).map_err(as_fault)?;
            p.touch();
            if write && self.backend.is_some() {
                p.set_dirty();
            }
            return Ok(p);
        }
    }

    /// FS-12: posix_fadvise(POSIX_FADV_WILLNEED) (linux-6.6/mm/fadvise.c -> `force_page_cache_readahead`): the
    /// pages of `[pos, pos + len)` (len 0: to the end of the file) are queued for the Background read-ahead
    /// thread; the call does not wait. Only for backends that take read-ahead, and not while this file's job runs.
    pub fn will_need(&self, pos: u64, len: u64) {
        if self.ra_max == 0 || !readahead::active() {
            return;
        }
        let Some(backend) = self.backend.clone() else { return };
        let mut g = self.inner.lock();
        let pages = pages_for(g.size);
        let first = pos / P;
        let end = if len == 0 { pages } else { pos.saturating_add(len).div_ceil(P).min(pages) };
        if g.ra.queued || first >= end {
            return;
        }
        g.ra.queued = true;
        let job = readahead::Job::new(Arc::downgrade(&self.inner), backend, self.done.clone(), (first, end - first), g.epoch, self.ra_pages(&g));
        drop(g);
        if !readahead::queue(job) {
            self.inner.lock().ra.queued = false;
        }
    }

    /// BL-0: posix_fadvise(POSIX_FADV_DONTNEED) (linux-7/mm/fadvise.c:113-167 `generic_fadvise`) of
    /// `[offset, offset + len)`, len 0 to the end; both at most `i64::MAX` (the caller refuses negative ones).
    /// 1. The dirty pages of the range are written back (`filemap_flush_range`). V2's backends write synchronously,
    ///    so the start of the write-back is the write itself, at Background priority; its error is not the call's
    ///    (Linux ignores it too) and the page stays dirty.
    /// 2. The clean pages among the *whole* pages of the range leave the cache (`mapping_try_invalidate`,
    ///    linux-7/mm/truncate.c:534): a partial first or last page stays, unless the range ends at a page end or at
    ///    the file's end (fadvise.c:120-145).
    ///
    /// What stays (`invalidate_clean`): tmpfs files' pages (their cache is their content; Linux's shmem has the
    /// no-op backing device, whose advice is ignored), dirty pages, mapped ones, pages a locked read or a block
    /// request holds, and pages being read in (not in the index yet).
    pub fn dont_need(&self, offset: u64, len: u64) -> Invalidated {
        if self.backend.is_none() {
            return Invalidated::default();
        }
        let max = i64::MAX as u64;
        let sum = offset.saturating_add(len);
        let endbyte = if len == 0 || sum > max { max } else { sum - 1 };
        let size = self.size();
        let pages = pages_for(size);
        if pages == 0 || offset / P >= pages {
            return Invalidated::default();
        }
        let last_page = (endbyte / P).min(pages - 1);
        if let Err(e) = self.sync_range(offset / P, last_page, SyncKind::Background) {
            DONTNEED_WB_ERRORS.inc();
            klog::event!(Info, "vfs", "fadvise.writeback_failed", errno = e; "POSIX_FADV_DONTNEED: the dirty pages stay");
        }
        let first = offset.div_ceil(P);
        let mut end = endbyte / P;
        if endbyte % P != P - 1 && endbyte != size.wrapping_sub(1) {
            if end == 0 {
                return Invalidated::default();
            }
            end -= 1;
        }
        let end = end.min(pages - 1);
        if end < first {
            return Invalidated::default();
        }
        let out = invalidate_clean(&self.inner, first, end);
        if out.dropped > 0 {
            // The frames are free when the call returns, as Linux's (bounded wait for the grace period, no lock held).
            mm_core::grace::expedite();
        }
        DONTNEED_DROPPED.add(out.dropped);
        DONTNEED_KEPT.add(out.kept);
        out
    }

    /// FS-13: the preload's read of file pages `[first, end)`: each missing page is filled through the normal
    /// fill path (the backend's windows, first insert wins, pages in flight waited for), so what it brings in
    /// are ordinary clean cache pages. `go(inserted so far)` runs before every device read and stops the pass
    /// with false. Ok: (pages this call inserted, device reads); a read error ends this range.
    pub fn prefetch(&self, first: u64, end: u64, go: &mut dyn FnMut(u64) -> bool) -> Result<(u64, u64), Errno> {
        if self.backend.is_none() {
            return Ok((0, 0));
        }
        let (mut inserted, mut reads) = (0, 0);
        let mut idx = first;
        loop {
            let next = {
                let g = self.inner.lock();
                let end = end.min(pages_for(g.size));
                let mut i = idx;
                while i < end && g.pages.contains(i) {
                    i += 1;
                }
                (i < end).then_some((i, end))
            };
            let Some((i, end)) = next else { return Ok((inserted, reads)) };
            if !go(inserted) {
                return Ok((inserted, reads));
            }
            inserted += self.fill_as(i, Reach::Forward(end - i), true)?;
            reads += 1;
            idx = i + 1;
        }
    }

    /// The cached page at `pgoff`, if any (no read, no allocation).
    pub fn cached(&self, pgoff: u64) -> Option<Arc<CachePage>> {
        // A probe (mincore, tests): not a use, so the reclaim clock is not told (`touch`).
        self.view.get(pgoff)
    }

    /// Pages `write` dirtied that no write-back took yet (cheap; `dirty_pages` counts exactly).
    pub fn own_dirty(&self) -> u64 {
        self.inner.lock().own_dirty
    }

    /// Dirty pages now.
    pub fn dirty_pages(&self) -> u64 {
        let g = self.inner.lock();
        let mut n = 0;
        g.pages.for_each(&mut |_, p| {
            n += p.is_dirty() as u64;
            true
        });
        n
    }

    /// Writes the dirty pages of file pages [first, last] back through the backend: runs of consecutive pages
    /// in file order between `begin` and `commit`. Without a backend (tmpfs) there is nothing to write.
    ///
    /// Ok means every page of the range that was dirty when the call began reached the backend. A page whose
    /// write did not happen is always dirty again afterwards:
    /// - `begin` fails: nothing is written, every collected page is dirty again, no `commit`, its error.
    /// - a run fails: it and every run after it stay dirty; `commit` still runs for what did reach the
    ///   device; the run's error.
    /// - no memory to collect the dirty pages: they are collected and written in batches as memory allows;
    ///   when not even one page fits, ENOMEM after `commit` of what was written (never a silent Ok).
    pub fn sync_range(&self, first: u64, last: u64, why: SyncKind) -> Result<(), Errno> {
        if self.backend.is_none() {
            return Ok(());
        }
        let one = self.sync_lock.lock();
        self.sync_ranges_locked(&one, &[(first, last)], why)
    }

    /// FS-STALL: this cache's write-back lock, for an owner that picks the pages to write while no other write-back
    /// runs (`sync_ranges_locked`).
    pub fn lock_sync(&self) -> ksync::MutexGuard<'_, ()> {
        self.sync_lock.lock()
    }

    /// `sync_range` over several [first, last] ranges in one write-back (one `begin`, one `commit`), under
    /// `lock_sync`'s guard: ext4's device knows the pages it dirtied and writes them back without a walk over every
    /// page of the disk's cache, which held `vfs.inode.data` (and so every metadata write of the volume) for as long.
    pub fn sync_ranges_locked(&self, _one: &ksync::MutexGuard<'_, ()>, ranges: &[(u64, u64)], why: SyncKind) -> Result<(), Errno> {
        let Some(b) = &self.backend else { return Ok(()) };
        let mut begun = false;
        let mut err = None;
        'ranges: for &(first, last) in ranges {
            let mut at = Some(first);
            while let Some(start) = at.filter(|&a| a <= last) {
                let batch = self.collect_dirty(start, last);
                if batch.pages.is_empty() {
                    if batch.oom {
                        err = Some(ENOMEM);
                        klog::event!(Warn, "vfs", "cache.writeback_nomem", first = start;
                            "no memory to collect dirty pages for write-back: they stay dirty, the call answers ENOMEM");
                        break 'ranges;
                    }
                    break;
                }
                if !begun {
                    if let Err(e) = b.begin(why) {
                        for p in &batch.pages {
                            p.set_dirty();
                        }
                        klog::event!(Warn, "vfs", "cache.writeback_begin_failed", pages = batch.pages.len(), errno = e;
                            "the file system refused to begin a write-back: the pages stay dirty");
                        return Err(e);
                    }
                    begun = true;
                }
                if let Err(e) = Self::write_runs(&**b, &batch) {
                    err = Some(e);
                    break 'ranges;
                }
                at = batch.next;
            }
        }
        if !begun {
            return err.map_or(Ok(()), Err);
        }
        let c = b.commit(why);
        match err {
            Some(e) => Err(e),
            None => c,
        }
    }

    /// The dirty pages of [start, last] in file order, their dirty flags cleared (Linux's
    /// clear_page_dirty_for_io: a write through a mapping while the run is written sets it again and is not
    /// lost). Room is reserved before a flag is cleared, so a page that does not fit stays dirty; `next` is
    /// where the next batch starts then (None: the range is done).
    fn collect_dirty(&self, start: u64, last: u64) -> Batch {
        let mut g = self.inner.lock();
        let mut batch = Batch { idx: Vec::new(), pages: Vec::new(), size: g.size, next: None, oom: false };
        let mut at = start;
        while at <= last {
            let Some((i, p)) = g.pages.next_from(at) else { break };
            if i > last {
                break;
            }
            if p.is_dirty() {
                #[cfg(test)]
                let capped = matches!(self.batch_cap.load(Ordering::Relaxed), c if c != 0 && batch.idx.len() + 1 >= c);
                #[cfg(not(test))]
                let capped = false;
                if capped || batch.idx.try_reserve(1).is_err() || batch.pages.try_reserve(1).is_err() {
                    batch.oom = true;
                    batch.next = Some(i);
                    break;
                }
                if p.take_dirty() {
                    g.own_dirty = g.own_dirty.saturating_sub(1);
                    batch.idx.push(i);
                    batch.pages.push(p.clone());
                }
            }
            match i.checked_add(1) {
                Some(n) => at = n,
                None => break,
            }
        }
        batch
    }

    /// Writes a batch as runs of consecutive pages; on a failure the failed run and all after it are dirty
    /// again.
    fn write_runs(b: &dyn Backend, batch: &Batch) -> Result<(), Errno> {
        let n = batch.idx.len();
        let mut i = 0;
        while i < n {
            let mut j = i + 1;
            while j < n && batch.idx[j] == batch.idx[j - 1] + 1 {
                j += 1;
            }
            if let Err(e) = b.write_run(batch.idx[i], &batch.pages[i..j], batch.size) {
                for p in &batch.pages[i..] {
                    p.set_dirty();
                }
                klog::event!(Warn, "vfs", "cache.writeback_failed", first = batch.idx[i], pages = n - i, errno = e;
                    "dirty pages kept dirty after a failed write-back");
                return Err(e);
            }
            WRITEBACK_PAGES.add((j - i) as u64);
            i = j;
        }
        Ok(())
    }

    /// The whole file (fsync, fdatasync).
    pub fn sync_all(&self, why: SyncKind) -> Result<(), Errno> {
        self.sync_range(0, u64::MAX, why)
    }
}

/// FS-10: the step of `generic_perform_write` a tmpfs write goes in. Linux 6.6 stepped by pages
/// (linux-6.6/mm/filemap.c:3948); CachyOS's kernel steps by the mapping's largest folio, 2 MiB for tmpfs
/// (measured: under F_SEAL_GROW a write across the end is refused whole unless it crosses a 2 MiB boundary).
/// The oracle's step is taken.
const WRITE_STEP: u64 = 2 << 20;

/// FS-10: how much of a write of `len` bytes at `pos` the seals let in (Linux checks them per step of
/// `generic_perform_write` in `shmem_write_begin`, linux-6.6/mm/shmem.c:2680): EPERM for any write under
/// F_SEAL_WRITE or F_SEAL_FUTURE_WRITE; under F_SEAL_GROW the steps that end inside the file, EPERM when not
/// even the first does. A short count makes the caller's next call answer EPERM, so the program sees the bytes
/// written.
fn seal_write_limit(seals: u32, pos: u64, len: usize, size: u64) -> Result<usize, Errno> {
    if seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0 {
        return Err(EPERM);
    }
    if seals & F_SEAL_GROW == 0 {
        return Ok(len);
    }
    let mut done = 0usize;
    while done < len {
        let p = pos.saturating_add(done as u64);
        let k = ((WRITE_STEP - p % WRITE_STEP) as usize).min(len - done);
        if p.saturating_add(k as u64) > size {
            break;
        }
        done += k;
    }
    if done == 0 { Err(EPERM) } else { Ok(done) }
}

/// The index the state of every dropped file keeps in place of its own (`PageCache`'s drop). It stays empty:
/// no file reaches it, and a read-ahead job that still holds such a state finds another epoch and no size.
static NO_PAGES: ksync::Once<Pages> = ksync::Once::new();

impl Drop for PageCache {
    fn drop(&mut self) {
        crate::reclaim::unregister(self.owner_id);
        let mut g = self.inner.lock();
        if let Some(pool) = &self.pool {
            pool.uncharge(g.charged);
        }
        // The pages go with the file (Linux `evict` -> `truncate_inode_pages_final`), not with the state: a
        // reclaimer's visit holds the state (`reclaim::Ring::walk`; kswapd's scan of a big file takes many
        // milliseconds) and leaves a reference in the grace queue (`reclaim::release`). A file dropped meanwhile
        // kept all its memory until both were gone, and whoever drained that queue then tore it down. So the
        // state gets the empty index, and the file's own one drops with `view` right after this, at once: no
        // reader is left (they borrow this cache) and everyone else reaches the pages through the state, under
        // this lock. Only where reclaim lists the cache: no other state has a holder that lasts.
        if self.reclaimable() {
            g.epoch = g.epoch.wrapping_add(1);
            g.size = 0;
            g.pages = NO_PAGES.get_or_init(|| Pages::new(0)).handle();
        }
    }
}
