//! What the ext4 code reads and writes: a volume as bytes in whole blocks of the file system. In the kernel a
//! `block::Disk` through its page cache (`CachedDiskDev`) or a file of another file system (`ImageDev`, the loop image `\BITSOS2\data.img`
//! on the boot stick's FAT); in host tests an image in memory that records every write in order (the cut-power
//! tests replay the first k of them) and fails on demand.
//!
//! ext4 keeps no block cache of its own: an image file has its own page cache below, and a disk mount gets one
//! by `block::bdev::DiskFile` (`CachedDiskDev`, FS-8c). C1 removed the uncached `DiskDev` (no caller).
//! FS-W2: that cache is for metadata and the log; a file's data goes to the disk beside it (`Dev::write_pages`).

use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use block::prio::Lift;
use block::{Disk, PageFrames, PageSeg};
use devmodel::block::{FrameIo, IoBuf};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::SpinLock;
use crate::errno::*;
use vfs::inode::Inode;
use vfs::pagecache::SyncKind;

const PAGE: u64 = 4096;

// BL-0: bytes ext4 copies out of a disk's page cache (`CachedDiskDev::read`, `read_cached`), per CPU.
vfs::pcpu_counter!(pub COPY_BDEV_CACHE, "block.copy_bytes.bdev_cache", vfs::pcpu::BYTES_BATCH);
// FS-W2: bytes ext4 copies into a disk's page cache (`CachedDiskDev::write`: metadata, the log; file data only
// where it cannot go straight to the disk).
vfs::pcpu_counter!(pub COPY_BDEV_WRITE, "block.copy_bytes.bdev_cache_write", vfs::pcpu::BYTES_BATCH);
// FS-W2: file bytes a write-back sent straight to the disk, beside its cache (`CachedDiskDev::write_pages`).
vfs::pcpu_counter!(pub DIRECT_BYTES, "ext4.write_direct_bytes", vfs::pcpu::BYTES_BATCH);
// BL-3: file bytes a disk read straight into the file's cache pages, and the reads that went through the disk's
// cache instead because it held newer bytes of their blocks (written, not yet flushed) or were not in whole disk
// blocks.
vfs::pcpu_counter!(pub FRAME_BYTES, "ext4.read_frames_bytes", vfs::pcpu::BYTES_BATCH);
vfs::pcpu_counter!(pub FRAME_VIA_CACHE, "ext4.read_frames_via_cache", vfs::pcpu::EVENTS_BATCH);

/// BL-3: one read of `len` bytes of whole volume blocks at byte `at`, straight into page-cache pages: `segs` in
/// order, `len` bytes in all (Linux's bio of `ext4_mpage_readpages`).
pub struct PageRead {
    pub at: u64,
    pub len: usize,
    pub segs: Vec<PageSeg>,
}

/// Copies `src` into the pages of `segs`, in order (`src.len()` is their total).
fn scatter(segs: &[PageSeg], src: &[u8]) {
    let mut done = 0;
    for s in segs {
        s.page.write(s.off, &src[done..done + s.len]);
        done += s.len;
    }
}

/// Takes the first `n` bytes of `segs` off as segments of their own (a segment across the cut is split).
fn cut_front(segs: &mut Vec<PageSeg>, n: usize) -> Result<Vec<PageSeg>, i64> {
    let (mut k, mut sum) = (0, 0);
    while k < segs.len() && sum + segs[k].len <= n {
        sum += segs[k].len;
        k += 1;
    }
    let mut front = Vec::new();
    front.try_reserve_exact(k + 1).map_err(|_| ENOMEM)?;
    front.extend(segs.drain(..k));
    if sum < n {
        let s = segs.first_mut().ok_or(EIO)?;
        let take = n - sum;
        front.push(PageSeg { page: s.page.clone(), off: s.off, len: take });
        s.off += take;
        s.len -= take;
    }
    Ok(front)
}

/// `r` through the device's `read` and one copy into the pages (`block.copy_bytes.ext4_page`).
pub(crate) fn bounce_pages<D: Dev + ?Sized>(dev: &D, r: &PageRead) -> Result<(), i64> {
    let mut tmp = Vec::new();
    tmp.try_reserve_exact(r.len).map_err(|_| ENOMEM)?;
    tmp.resize(r.len, 0);
    dev.read(r.at, &mut tmp)?;
    scatter(&r.segs, &tmp);
    crate::file::COPY_EXT4_PAGE.add(r.len as u64);
    Ok(())
}

pub trait Dev: Send + Sync {
    /// Size in bytes.
    fn size(&self) -> u64;
    /// Reads `buf.len()` bytes at byte offset `at` (both multiples of 512).
    fn read(&self, at: u64, buf: &mut [u8]) -> Result<(), i64>;
    fn write(&self, at: u64, data: &[u8]) -> Result<(), i64>;
    /// Everything written so far on stable storage.
    fn flush(&self) -> Result<(), i64>;
    /// `write`, then these bytes on stable storage: the journal's commit block (FS-W). Other bytes written since the
    /// last `flush` may stay where they are: Linux sends the commit record alone with a flush before it and FUA
    /// (`journal_submit_commit_record`), it writes out nobody's dirty data for it. Default: `write` and `flush`.
    fn write_through(&self, at: u64, data: &[u8]) -> Result<(), i64> {
        self.write(at, data)?;
        self.flush()
    }
    /// `read` if the bytes are in memory, else false and nothing read (FS-STALL: the volume lock is never held over a
    /// device read of a read-only call). Default: every read counts as in memory.
    fn read_cached(&self, at: u64, buf: &mut [u8]) -> Result<bool, i64> {
        self.read(at, buf).map(|()| true)
    }
    /// After an fsync's flush, with no volume lock held: every flush answered before is durable
    /// (`BlockDevice::settle`, INT-W12 VS-2b CR 3). Never called by freeze, `make_clean` or the journal's
    /// barriers: a snapshot's freeze would wait for its own commit.
    fn settle(&self) -> Result<(), i64> {
        Ok(())
    }
    /// Where the volume lock's holder and waiters are noted for the request priorities (`block::prio::Lift`).
    fn lift(&self) -> Option<&Lift> {
        None
    }
    /// Whether the last failure was the device's timeout, not a media error; forgets it (VS-H CR 5).
    fn take_timed_out(&self) -> bool {
        false
    }
    /// BL-3: file data into its cache pages. Default: each through `read` and one copy; a disk reads into the pages
    /// themselves (`CachedDiskDev`). Called with no volume lock held.
    fn read_pages(&self, reads: Vec<PageRead>) -> Result<(), i64> {
        for r in &reads {
            bounce_pages(self, r)?;
        }
        Ok(())
    }
    /// BL-4 CR 4: the device's largest request in 4 KiB pages (`Backend::io_pages`); 0: not known.
    fn io_pages(&self) -> u64 {
        0
    }
    /// FS-W2: file data of whole volume blocks, straight to the device (a write-back step's part 2, `flight`).
    /// When it returns the device has taken the bytes: the next `flush` makes them durable, `read` gives them,
    /// and nothing written to the same blocks through `write` before can land on top of them. The caller makes
    /// sure nobody writes these blocks through `write` meanwhile (they belong to the step's file). Default:
    /// `write` (a device with no cache of its own in front of it, or one that is a file).
    fn write_data(&self, at: u64, data: &[u8]) -> Result<(), i64> {
        self.write(at, data)
    }
    /// FSR: file data of whole volume blocks that the caller will write back with `write_data`. A disk reads it
    /// beside its cache when the cache does not hold it, so that write finds nothing of the range cached. Default:
    /// `read`.
    fn read_data(&self, at: u64, buf: &mut [u8]) -> Result<(), i64> {
        self.read(at, buf)
    }
    /// FS-W2: whether `write_pages` sends the pages themselves to the device (a disk). False: it would copy them
    /// into one buffer from the heap, so a write-back copies into its kept buffer and calls `write_data`.
    fn takes_pages(&self) -> bool {
        false
    }
    /// FS-W2: `write_data` from page-cache pages: `segs` in order, whole volume blocks in all. Default: gathered
    /// into one buffer (host tests; the kernel asks `takes_pages` first).
    fn write_pages(&self, at: u64, segs: Vec<PageSeg>) -> Result<(), i64> {
        let len: usize = segs.iter().map(|s| s.len).sum();
        let mut tmp = Vec::new();
        tmp.try_reserve_exact(len).map_err(|_| ENOMEM)?;
        tmp.resize(len, 0);
        let mut done = 0;
        for s in &segs {
            s.page.read(s.off, &mut tmp[done..done + s.len]);
            done += s.len;
        }
        self.write_data(at, &tmp)
    }
}

/// Recorded writes: (byte offset, bytes), in order, and two marks, each an empty write: at `FLUSHED` a flush
/// (every write before it is stable), at `THROUGH` a `write_through` (the write right before it is stable, and
/// nothing else is). A power cut may lose any write that neither mark covers.
pub type WriteLog = Vec<(u64, Vec<u8>)>;
pub const FLUSHED: u64 = u64::MAX;
pub const THROUGH: u64 = u64::MAX - 1;

/// Whether a log entry at `at` is a mark, not a write.
pub fn is_mark(at: u64) -> bool {
    at >= THROUGH
}

/// A volume image in memory (host tests, ktests).
pub struct MemDev {
    img: SpinLock<Vec<u8>>,
    log: SpinLock<Option<WriteLog>>,
    fail_writes: AtomicBool,
    fail_reads: AtomicBool,
    /// Writes and flushes answer as a device that timed out (`take_timed_out` says so).
    time_out: AtomicBool,
    timed_out: AtomicBool,
    /// Writes after which every further write fails (a cut power: the device stops).
    stop_after: AtomicU64,
    writes: AtomicU64,
    written_bytes: AtomicU64,
    settles: AtomicU64,
}

impl MemDev {
    pub fn new(img: Vec<u8>) -> MemDev {
        MemDev {
            img: SpinLock::named("ext4.memdev", img),
            log: SpinLock::named("ext4.memdev.log", None),
            fail_writes: AtomicBool::new(false),
            time_out: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
            stop_after: AtomicU64::new(u64::MAX),
            writes: AtomicU64::new(0),
            written_bytes: AtomicU64::new(0),
            settles: AtomicU64::new(0),
        }
    }
    pub fn record(&self) {
        *self.log.lock() = Some(Vec::new());
    }
    pub fn take_log(&self) -> WriteLog {
        self.log.lock().take().unwrap_or_default()
    }
    pub fn image(&self) -> Vec<u8> {
        self.img.lock().clone()
    }
    /// The image, without a copy.
    pub fn into_image(self) -> Vec<u8> {
        self.img.into_inner()
    }
    pub fn fail_writes(&self, on: bool) {
        self.fail_writes.store(on, Ordering::Relaxed);
    }
    /// From now on writes and flushes time out (EIO, and `take_timed_out` answers true), until `on` is false.
    pub fn time_out(&self, on: bool) {
        self.time_out.store(on, Ordering::Relaxed);
    }
    fn timing_out(&self) -> bool {
        let on = self.time_out.load(Ordering::Relaxed);
        if on {
            self.timed_out.store(true, Ordering::Relaxed);
        }
        on
    }
    pub fn fail_reads(&self, on: bool) {
        self.fail_reads.store(on, Ordering::Relaxed);
    }
    /// Lets `n` more writes through, then fails every write (EIO).
    pub fn stop_after(&self, n: u64) {
        self.stop_after.store(self.writes.load(Ordering::Relaxed) + n, Ordering::Relaxed);
    }
    pub fn writes(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }
    /// Bytes that reached the image.
    pub fn written_bytes(&self) -> u64 {
        self.written_bytes.load(Ordering::Relaxed)
    }
    /// How often `settle` was called (fsync, syncfs; never freeze).
    pub fn settles(&self) -> u64 {
        self.settles.load(Ordering::Relaxed)
    }
}

impl MemDev {
    /// A write; `through`: with the mark that it alone is stable, right behind it in the log (one hold of the
    /// log, so no other thread's write gets between the two).
    fn put(&self, at: u64, data: &[u8], through: bool) -> Result<(), i64> {
        if self.timing_out() {
            return Err(EIO);
        }
        let n = self.writes.load(Ordering::Relaxed);
        if self.fail_writes.load(Ordering::Relaxed) || n >= self.stop_after.load(Ordering::Relaxed) {
            return Err(EIO);
        }
        self.writes.store(n + 1, Ordering::Relaxed);
        {
            let mut img = self.img.lock();
            let a = usize::try_from(at).map_err(|_| EIO)?;
            let dst = img.get_mut(a..a.checked_add(data.len()).ok_or(EIO)?).ok_or(EIO)?;
            dst.copy_from_slice(data);
        }
        self.written_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
        if let Some(l) = self.log.lock().as_mut() {
            l.push((at, data.to_vec()));
            if through {
                l.push((THROUGH, Vec::new()));
            }
        }
        Ok(())
    }
}

impl Dev for MemDev {
    fn size(&self) -> u64 {
        self.img.lock().len() as u64
    }
    fn read(&self, at: u64, buf: &mut [u8]) -> Result<(), i64> {
        if self.fail_reads.load(Ordering::Relaxed) {
            return Err(EIO);
        }
        let img = self.img.lock();
        let at = usize::try_from(at).map_err(|_| EIO)?;
        let src = img.get(at..at.checked_add(buf.len()).ok_or(EIO)?).ok_or(EIO)?;
        buf.copy_from_slice(src);
        Ok(())
    }
    fn write(&self, at: u64, data: &[u8]) -> Result<(), i64> {
        self.put(at, data, false)
    }
    fn flush(&self) -> Result<(), i64> {
        if self.timing_out() {
            return Err(EIO);
        }
        if let Some(l) = self.log.lock().as_mut() {
            l.push((FLUSHED, Vec::new()));
        }
        Ok(())
    }
    /// As a disk with a cache does it (`CachedDiskDev::write_through`): only this write becomes stable. The log
    /// says so with its own mark, so a cut test can lose what a real disk could lose behind the commit block
    /// (with the default, a flush, every cut test would still model the second barrier FS-W removed).
    fn write_through(&self, at: u64, data: &[u8]) -> Result<(), i64> {
        self.put(at, data, true)
    }
    fn settle(&self) -> Result<(), i64> {
        self.settles.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn take_timed_out(&self) -> bool {
        self.timed_out.swap(false, Ordering::Relaxed)
    }
}

/// Whether [at, at + len) covers whole `bs`-byte blocks.
pub(crate) fn aligned(bs: u64, at: u64, len: usize) -> bool {
    at % bs == 0 && len as u64 % bs == 0
}

/// The whole blocks around [at, at + len): (first byte, length).
pub(crate) fn around(bs: u64, at: u64, len: usize) -> Result<(u64, usize), i64> {
    let first = at / bs * bs;
    let end = at.checked_add(len as u64).ok_or(EIO)?.div_ceil(bs) * bs;
    Ok((first, usize::try_from(end - first).map_err(|_| EIO)?))
}

/// A read of 512-aligned bytes inside larger blocks (the superblock at byte 1024 of a 4 KiB-block disk):
/// the blocks around it, then the part asked for.
pub(crate) fn read_part(bs: u64, at: u64, buf: &mut [u8], whole: impl Fn(u64, &mut [u8]) -> Result<(), i64>) -> Result<(), i64> {
    if aligned(bs, at, buf.len()) {
        return whole(at, buf);
    }
    let (first, n) = around(bs, at, buf.len())?;
    let mut tmp = alloc::vec![0u8; n];
    whole(first, &mut tmp)?;
    let o = (at - first) as usize;
    buf.copy_from_slice(&tmp[o..o + buf.len()]);
    Ok(())
}

/// A write of 512-aligned bytes inside larger blocks: read, patch, write the blocks around it (the volume
/// lock is held, so nothing else changes those blocks in between).
pub(crate) fn write_part(bs: u64, at: u64, data: &[u8], read: impl Fn(u64, &mut [u8]) -> Result<(), i64>, write: impl Fn(u64, &[u8]) -> Result<(), i64>) -> Result<(), i64> {
    if aligned(bs, at, data.len()) {
        return write(at, data);
    }
    let (first, n) = around(bs, at, data.len())?;
    let mut tmp = alloc::vec![0u8; n];
    read(first, &mut tmp)?;
    let o = (at - first) as usize;
    tmp[o..o + data.len()].copy_from_slice(data);
    write(first, &tmp)
}

/// A partition (or a whole disk) of a `block::Disk` through the disk's page cache (`block::bdev::DiskFile`,
/// Linux's block-device mapping that ext4's buffer heads live in): metadata reads hit memory after the first
/// time. `write` lands in the cache; `flush` writes every dirty page back and flushes the device, the barrier the
/// journal puts before a commit block (the same contract as `ImageDev` on a file's cache).
///
/// FS-W2: file data does not pass through this cache. `write_pages` (and `write_data`) send it to the disk and
/// keep the cache right about those blocks (`vfs::pagecache::PageCache::write_direct`: an older page of them is
/// written out or waited for first and then dropped, a fill that read them meanwhile inserts nothing). So a
/// `flush` writes only what `write` dirtied (metadata, the log) and then flushes the device **once**, itself:
/// the cache's backend does not (`DiskFile::page_cache_owner_flushes`), because with data beside the cache "no
/// dirty page" no longer means "nothing in the device's cache". That one device flush is what makes every data
/// write that ended before the call durable: the journal's commit relies on it (ordered mode, `flight`).
pub struct CachedDiskDev {
    cache: vfs::pagecache::PageCache,
    disk: Arc<Disk>,
    start: u64,
    bytes: u64,
    lift: Lift,
    /// The cache pages this device dirtied since its last flush, so `flush` writes those instead of walking the
    /// whole disk's cache (FS-STALL: 5 M pages on the Mini-PC, under `vfs.inode.data`, twice per commit).
    dirty: SpinLock<Dirtied>,
    /// BL-3: file reads this device took through its cache (`read_pages`' fallback; tests).
    via_cache: AtomicU64,
}

#[derive(Default)]
struct Dirtied {
    /// Page ranges [first, last], in write order (merged with the one before when they touch).
    runs: Vec<(u64, u64)>,
    /// A range could not be noted (no memory): the next flush walks the whole cache.
    lost: bool,
    /// BL-3: the lowest and highest page of `runs` (a file read outside them skips the walk).
    span: Option<(u64, u64)>,
    /// BL-3: what the running flush took from `runs`: the disk may not hold those bytes yet (their pages are clean
    /// while their write is in flight, `collect_dirty`), so file reads of them still go through the cache.
    flushing: Vec<(u64, u64)>,
    flushing_span: Option<(u64, u64)>,
    /// The running flush's ranges could not be listed (no memory, or `lost`): every file read goes through the cache
    /// until it ends.
    flushing_all: bool,
}

fn widen(span: &mut Option<(u64, u64)>, first: u64, last: u64) {
    *span = Some(span.map_or((first, last), |(a, b)| (a.min(first), b.max(last))));
}


impl Dirtied {
    fn note(&mut self, first: u64, last: u64) {
        widen(&mut self.span, first, last);
        if let Some(r) = self.runs.last_mut()
            && first <= r.1.saturating_add(1)
            && last.saturating_add(1) >= r.0
        {
            *r = (r.0.min(first), r.1.max(last));
            return;
        }
        if self.runs.try_reserve(1).is_err() {
            self.lost = true;
            return;
        }
        self.runs.push((first, last));
    }
    /// A flush takes the noted ranges; they stay listed as `flushing` until `end_flush`.
    fn start_flush(&mut self) -> (Vec<(u64, u64)>, bool) {
        let runs = core::mem::take(&mut self.runs);
        let lost = core::mem::take(&mut self.lost);
        self.flushing_span = self.span.take();
        self.flushing.clear();
        self.flushing_all = lost || self.flushing.try_reserve(runs.len()).is_err();
        if !self.flushing_all {
            self.flushing.extend_from_slice(&runs);
        }
        (runs, lost)
    }
    fn end_flush(&mut self) {
        self.flushing.clear();
        self.flushing_span = None;
        self.flushing_all = false;
    }
    /// BL-3: the pages from `first` (one entry of `ends` each, all 0) whose bytes the disk's cache may hold and the
    /// disk not yet: each range of `runs` and `flushing` inside them sets, at its first page, how far it reaches
    /// (`held` turns that into one flag per page, after the lock). One walk over the lists, nothing per page under
    /// the spin lock (fixer: a read inside a long dirty list cost pages times ranges here). False: none.
    fn mark_pending(&self, first: u64, ends: &mut [u32]) -> bool {
        let Some(last) = (ends.len() as u64).checked_sub(1).map(|n| first + n) else { return false };
        if self.lost || self.flushing_all {
            ends[0] = ends.len() as u32;
            return true;
        }
        let mut any = false;
        for (runs, span) in [(&self.runs, self.span), (&self.flushing, self.flushing_span)] {
            if !span.is_some_and(|(a, b)| first <= b && last >= a) {
                continue;
            }
            for &(a, b) in runs.iter().filter(|&&(a, b)| first <= b && last >= a) {
                let s = (a.max(first) - first) as usize;
                ends[s] = ends[s].max((b.min(last) - first + 1) as u32);
                any = true;
            }
        }
        any
    }
}

/// A request's failure as the write-back reports it (ETIMEDOUT for a timeout, as `DiskFile::page_cache_owner_flushes`).
fn flush_errno(e: block::BlockError) -> i64 {
    if e == block::BlockError::Timeout { ETIMEDOUT } else { e.errno() }
}

/// `Dirtied::mark_pending`'s reaches turned into one flag per page (non-zero: held back), outside the lock.
fn held_flags(ends: &mut [u32]) {
    let mut reach = 0u32;
    for (i, e) in ends.iter_mut().enumerate() {
        reach = reach.max(*e);
        *e = u32::from((i as u32) < reach);
    }
}

/// `runs` sorted, touching ones merged (in place: the flush allocates nothing).
fn merged(mut runs: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    runs.sort_unstable();
    let mut w = 0;
    for i in 0..runs.len() {
        if w > 0 && runs[i].0 <= runs[w - 1].1.saturating_add(1) {
            runs[w - 1].1 = runs[w - 1].1.max(runs[i].1);
        } else {
            runs[w] = runs[i];
            w += 1;
        }
    }
    runs.truncate(w);
    runs
}

impl CachedDiskDev {
    pub fn new(disk: Arc<Disk>, start: u64, blocks: u64) -> CachedDiskDev {
        let bs = disk.block_size() as u64;
        CachedDiskDev { cache: block::bdev::DiskFile::page_cache_owner_flushes(disk.clone()), disk, start: start * bs, bytes: blocks * bs, lift: Lift::new(), dirty: SpinLock::named("ext4.dev.dirtied", Dirtied::default()), via_cache: AtomicU64::new(0) }
    }
    /// BL-0: pages in the disk's cache now (ktests: drop_caches empties it of clean pages).
    pub fn cached_pages(&self) -> u64 {
        self.cache.pages()
    }
    /// BL-3: file reads (`read_pages`) that went through this device's cache instead of straight to the disk.
    pub fn file_reads_via_cache(&self) -> u64 {
        self.via_cache.load(Ordering::Relaxed)
    }
    fn at(&self, at: u64, len: usize) -> Result<u64, i64> {
        if at.checked_add(len as u64).is_none_or(|e| e > self.bytes) {
            return Err(EIO);
        }
        Ok(self.start + at)
    }
    /// The device's own cache made durable: every write it answered before (nothing to do without such a cache).
    fn device_flush(&self) -> Result<(), i64> {
        if !self.disk.dev_info().has_volatile_cache {
            return Ok(());
        }
        self.disk.flush(block::prio::higher(devmodel::block::IoPriority::Normal, block::prio::current())).map_err(flush_errno)
    }
    /// The pages [first, last] of the disk's cache that the disk bytes [pos, pos + len) are, when they are whole
    /// pages (a partition that begins inside a page has none: its data keeps going through the cache).
    fn page_run(pos: u64, len: usize) -> Option<(u64, u64)> {
        (len > 0 && pos % PAGE == 0 && len as u64 % PAGE == 0).then(|| (pos / PAGE, (pos + len as u64 - 1) / PAGE))
    }
    /// The priority of a data write: never below Normal, as the commit's flush sent it before FS-W2.
    fn data_prio() -> devmodel::block::IoPriority {
        block::prio::higher(devmodel::block::IoPriority::Normal, block::prio::current())
    }
}

impl Dev for CachedDiskDev {
    fn size(&self) -> u64 {
        self.bytes
    }
    fn io_pages(&self) -> u64 {
        self.disk.request_pages()
    }
    fn read(&self, at: u64, buf: &mut [u8]) -> Result<(), i64> {
        let pos = self.at(at, buf.len())?;
        let _f = block::prio::floor(self.lift.prio());
        if self.cache.read_at(pos, buf)? != buf.len() {
            return Err(EIO);
        }
        COPY_BDEV_CACHE.add(buf.len() as u64);
        Ok(())
    }
    fn read_cached(&self, at: u64, buf: &mut [u8]) -> Result<bool, i64> {
        let pos = self.at(at, buf.len())?;
        let (got, _) = self.cache.read_quick(pos, buf.len(), &mut |page, off, n, done| {
            let out = &mut buf[done..done + n];
            match page {
                Some(p) => p.read(off, out),
                None => out.fill(0),
            }
            n
        });
        COPY_BDEV_CACHE.add(got as u64);
        Ok(got == buf.len())
    }
    /// BL-3: file data from the disk into the file's pages, all requests queued before the first is waited for
    /// (`Disk::read_frames`); this cache is not touched. The disk-cache pages this device wrote and has not flushed
    /// yet (`Dirtied::pending`) are read through the cache as before (the disk does not hold those bytes), the rest
    /// of the same read straight from the disk; so is a read not in whole disk blocks. A write landing after the
    /// check belongs to a page the file's cache holds (its write-back), so the cache keeps that page and drops what
    /// this read brought (`fill_with`).
    fn read_pages(&self, reads: Vec<PageRead>) -> Result<(), i64> {
        let dbs = u64::from(self.disk.block_size()).max(1);
        let mut reqs: Vec<FrameIo> = Vec::new();
        reqs.try_reserve_exact(reads.len()).map_err(|_| ENOMEM)?;
        let mut slow: Vec<PageRead> = Vec::new();
        for r in reads {
            if r.len == 0 {
                continue;
            }
            let pos = self.at(r.at, r.len)?;
            if pos % dbs != 0 || r.len as u64 % dbs != 0 {
                slow.try_reserve(1).map_err(|_| ENOMEM)?;
                slow.push(r);
                continue;
            }
            let (first, last) = (pos / PAGE, (pos + r.len as u64 - 1) / PAGE);
            let mut held: Vec<u32> = Vec::new();
            held.try_reserve_exact((last - first + 1) as usize).map_err(|_| ENOMEM)?;
            held.resize((last - first + 1) as usize, 0);
            if self.dirty.lock().mark_pending(first, &mut held) {
                held_flags(&mut held);
            } else {
                held.clear();
            }
            let (mut segs, mut at, mut cur, end) = (r.segs, r.at, pos, pos + r.len as u64);
            // Runs of pages that are all held back or all on the disk, each one read (page boundaries are whole
            // disk blocks).
            while cur < end {
                let state = |a: u64| held.get((a / PAGE - first) as usize).is_some_and(|&h| h != 0);
                let here = state(cur);
                let mut stop = ((cur / PAGE + 1) * PAGE).min(end);
                while stop < end && state(stop) == here {
                    stop = ((stop / PAGE + 1) * PAGE).min(end);
                }
                let n = (stop - cur) as usize;
                let part = PageRead { at, len: n, segs: cut_front(&mut segs, n)? };
                if here {
                    slow.try_reserve(1).map_err(|_| ENOMEM)?;
                    slow.push(part);
                } else {
                    let frames = PageFrames::new(part.segs).ok_or(ENOMEM)?;
                    reqs.try_reserve(1).map_err(|_| ENOMEM)?;
                    reqs.push(FrameIo { lba: cur / dbs, buf: IoBuf::new(Arc::new(frames)) });
                }
                at += n as u64;
                cur = stop;
            }
        }
        let _f = block::prio::floor(self.lift.prio());
        if !reqs.is_empty() {
            self.disk.read_frames(&reqs, block::prio::current()).map_err(|e| e.errno())?;
            FRAME_BYTES.add(reqs.iter().map(|q| q.buf.len() as u64).sum());
        }
        if !slow.is_empty() {
            FRAME_VIA_CACHE.add(slow.len() as u64);
            self.via_cache.fetch_add(slow.len() as u64, Ordering::Relaxed);
            klog::event!(Debug, "ext4", "read.via_cache", reads = slow.len() as u64; "file blocks not yet flushed to the disk (or not in whole disk blocks): read through its cache");
        }
        for r in &slow {
            bounce_pages(self, r)?;
        }
        Ok(())
    }
    fn write(&self, at: u64, data: &[u8]) -> Result<(), i64> {
        let pos = self.at(at, data.len())?;
        let _f = block::prio::floor(self.lift.prio());
        if data.is_empty() {
            return Ok(());
        }
        let n = self.cache.write_at(pos, data)?;
        COPY_BDEV_WRITE.add(n as u64);
        // Noted after the bytes are in the cache: a flush that took the list earlier did not owe them.
        if n > 0 {
            self.dirty.lock().note(pos / PAGE, (pos + n as u64 - 1) / PAGE);
        }
        if n != data.len() {
            return Err(EIO);
        }
        Ok(())
    }
    fn flush(&self) -> Result<(), i64> {
        let _f = block::prio::floor(self.lift.prio());
        // The list is taken under the cache's write-back lock: a flush that finds it empty while another one writes
        // its pages waits for those writes first. The lock goes before the device flush (FS-W2): the flush covers
        // every write the device answered, whoever sent it, and a data write that has to wait for this lock (it
        // met an old page of its blocks) does not wait for the device's flush too.
        let wrote = {
            let one = self.cache.lock_sync();
            let (runs, lost) = self.dirty.lock().start_flush();
            let r = if runs.is_empty() && !lost {
                Ok(())
            } else {
                let runs = if lost { alloc::vec![(0, u64::MAX)] } else { merged(runs) };
                let r = self.cache.sync_ranges_locked(&one, &runs, SyncKind::Fsync);
                if r.is_err() {
                    // Noted again before `flushing` goes: a file read never sees the failed ranges unlisted.
                    let mut d = self.dirty.lock();
                    d.lost |= lost;
                    for (a, b) in runs {
                        d.note(a, b);
                    }
                    d.end_flush();
                }
                r
            };
            if r.is_ok() {
                self.dirty.lock().end_flush();
            }
            r
        };
        wrote?;
        // Always, also with nothing dirty: a flush is an ordering point (the journal's barrier, fsync), and file
        // data went to the disk beside the cache.
        self.device_flush()
    }
    /// Only these pages are written back, then the disk's own cache is flushed: what else the cache holds dirty
    /// waits for the next `flush`. The cache's write-back lock is taken before the bytes go in, so no flush running
    /// beside this call still has the pages at the device when this call flushes it. The pages stay noted as
    /// dirtied; the next `flush` finds them clean.
    fn write_through(&self, at: u64, data: &[u8]) -> Result<(), i64> {
        let pos = self.at(at, data.len())?;
        if data.is_empty() {
            return self.flush();
        }
        let _f = block::prio::floor(self.lift.prio());
        {
            let one = self.cache.lock_sync();
            let n = self.cache.write_at(pos, data)?;
            COPY_BDEV_WRITE.add(n as u64);
            let (first, last) = (pos / PAGE, (pos + data.len() as u64 - 1) / PAGE);
            if n > 0 {
                self.dirty.lock().note(first, (pos + n as u64 - 1) / PAGE);
            }
            if n != data.len() {
                return Err(EIO);
            }
            self.cache.sync_ranges_locked(&one, &[(first, last)], SyncKind::Fsync)?;
        }
        self.device_flush()
    }
    /// FSR: from the disk's cache when it holds the pages (they may be newer than the disk), else straight from the
    /// disk without bringing them in: the `write_data` after it then takes `write_direct`'s way with nothing cached,
    /// and never waits for the cache's write-back lock, which a commit's flush holds while it writes the log and
    /// the metadata (the caller holds `ext4.vol`). Nobody writes these blocks through the cache meanwhile (file data).
    fn read_data(&self, at: u64, buf: &mut [u8]) -> Result<(), i64> {
        let pos = self.at(at, buf.len())?;
        let dbs = u64::from(self.disk.block_size()).max(1);
        if Self::page_run(pos, buf.len()).is_none() || pos % dbs != 0 {
            return self.read(at, buf);
        }
        if self.read_cached(at, buf)? {
            return Ok(());
        }
        let _f = block::prio::floor(self.lift.prio());
        self.disk.read(pos / dbs, buf, Self::data_prio()).map_err(|e| e.errno())?;
        FRAME_BYTES.add(buf.len() as u64);
        Ok(())
    }
    /// FS-W2: to the disk, beside the cache (`PageCache::write_direct` keeps the cache right about the blocks).
    /// The request owns a copy of `data` (`Disk::write`): for the one padded block at a file's end; a write-back's
    /// megabytes come through `write_pages`.
    fn write_data(&self, at: u64, data: &[u8]) -> Result<(), i64> {
        let pos = self.at(at, data.len())?;
        let Some((first, last)) = Self::page_run(pos, data.len()) else { return self.write(at, data) };
        let lba = pos / u64::from(self.disk.block_size()).max(1);
        self.cache.write_direct(first, last, || self.disk.write(lba, data, false, Self::data_prio()).map_err(flush_errno))?;
        DIRECT_BYTES.add(data.len() as u64);
        Ok(())
    }
    fn takes_pages(&self) -> bool {
        self.start % PAGE == 0
    }
    /// FS-W2: the file's pages are the request's memory (no copy, as the cache's own write-back sends its pages).
    fn write_pages(&self, at: u64, segs: Vec<PageSeg>) -> Result<(), i64> {
        let len: usize = segs.iter().map(|s| s.len).sum();
        let pos = self.at(at, len)?;
        if len == 0 {
            return Ok(());
        }
        let Some((first, last)) = Self::page_run(pos, len) else {
            // Not whole pages of the disk's cache: through it, page by page (one page from the heap, not the run).
            let mut buf = Vec::new();
            buf.try_reserve_exact(PAGE as usize).map_err(|_| ENOMEM)?;
            buf.resize(PAGE as usize, 0);
            let mut done = 0u64;
            for s in &segs {
                let b = buf.get_mut(..s.len).ok_or(EIO)?;
                s.page.read(s.off, b);
                self.write(at + done, b)?;
                done += s.len as u64;
            }
            return Ok(());
        };
        let lba = pos / u64::from(self.disk.block_size()).max(1);
        self.cache.write_direct(first, last, move || self.disk.write_pages(lba, segs, false, Self::data_prio()).map_err(flush_errno))?;
        DIRECT_BYTES.add(len as u64);
        Ok(())
    }
    fn settle(&self) -> Result<(), i64> {
        self.disk.settle().map_err(|e| e.errno())
    }
    fn lift(&self) -> Option<&Lift> {
        Some(&self.lift)
    }
    /// Its write-back answers ETIMEDOUT per request (`DiskFile::page_cache_telling_timeouts`): the disk-wide flag
    /// is not asked (another request's timeout would excuse a media error of a commit).
    fn take_timed_out(&self) -> bool {
        false
    }
}

/// A loop image: a regular file of another mounted file system (the FAT stick's `\BITSOS2\data.img`), read
/// and written through its page cache. Its size is fixed at mount (ext4 never grows its device).
/// `flush` is the image's fsync: its dirty pages, its file system's metadata and the device cache.
pub struct ImageDev {
    file: Arc<Inode>,
    bytes: u64,
    /// Held like an open for writing (Linux's loop device keeps its file open).
    _write: Option<vfs::inode::WriteAccess>,
    /// The ext4 volume lock's lift: the image file system's requests made for its holder get an I/O floor.
    lift: Lift,
}

impl ImageDev {
    /// `writable`: the image will be written (it takes write access, ETXTBSY while a program runs it).
    pub fn new(file: Arc<Inode>, writable: bool) -> Result<ImageDev, i64> {
        if !file.is_reg() {
            return Err(EINVAL);
        }
        let bytes = file.size() & !4095;
        let w = if writable { Some(file.get_write_access()?) } else { None };
        Ok(ImageDev { file, bytes, _write: w, lift: Lift::new() })
    }
    pub fn inode(&self) -> &Arc<Inode> {
        &self.file
    }
}

impl Dev for ImageDev {
    fn size(&self) -> u64 {
        self.bytes
    }
    fn read(&self, at: u64, buf: &mut [u8]) -> Result<(), i64> {
        if at.checked_add(buf.len() as u64).is_none_or(|e| e > self.bytes) {
            return Err(EIO);
        }
        let _f = block::prio::floor(self.lift.prio());
        let n = self.file.read_at(at, buf)?;
        if n != buf.len() {
            return Err(EIO);
        }
        Ok(())
    }
    fn write(&self, at: u64, data: &[u8]) -> Result<(), i64> {
        if at.checked_add(data.len() as u64).is_none_or(|e| e > self.bytes) {
            return Err(EIO);
        }
        let _f = block::prio::floor(self.lift.prio());
        let n = self.file.write_at(at, data)?;
        if n != data.len() {
            return Err(EIO);
        }
        Ok(())
    }
    fn flush(&self) -> Result<(), i64> {
        let _f = block::prio::floor(self.lift.prio());
        self.file.sync(SyncKind::Fsync)
    }
    fn lift(&self) -> Option<&Lift> {
        Some(&self.lift)
    }
}

/// A read-only view of a device with blocks laid over it in memory: a read-only mount of a volume whose
/// journal needs recovery sees the replayed state without writing anything (`Volume::recover`).
pub struct OverlayDev {
    inner: Arc<dyn Dev>,
    bs: u64,
    blocks: SpinLock<BTreeMap<u64, Vec<u8>>>,
}

impl OverlayDev {
    pub fn new(inner: Arc<dyn Dev>, bs: u64) -> OverlayDev {
        OverlayDev { inner, bs, blocks: SpinLock::named("ext4.overlay", BTreeMap::new()) }
    }
    pub fn put(&self, blk: u64, data: Vec<u8>) -> Result<(), i64> {
        if data.len() as u64 != self.bs {
            return Err(EINVAL);
        }
        self.blocks.lock().insert(blk, data);
        Ok(())
    }
}

impl Dev for OverlayDev {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn read(&self, at: u64, buf: &mut [u8]) -> Result<(), i64> {
        self.inner.read(at, buf)?;
        if buf.is_empty() {
            return Ok(());
        }
        let end = at + buf.len() as u64;
        let m = self.blocks.lock();
        for (b, d) in m.range(at / self.bs..=(end - 1) / self.bs) {
            let (bstart, bend) = (b * self.bs, (b + 1) * self.bs);
            let (s, e) = (bstart.max(at), bend.min(end));
            buf[(s - at) as usize..(e - at) as usize].copy_from_slice(&d[(s - bstart) as usize..(e - bstart) as usize]);
        }
        Ok(())
    }
    fn write(&self, _at: u64, _data: &[u8]) -> Result<(), i64> {
        Err(EROFS)
    }
    fn flush(&self) -> Result<(), i64> {
        Ok(())
    }
    fn lift(&self) -> Option<&Lift> {
        self.inner.lift()
    }
}

#[cfg(test)]
mod dirtied_tests {
    use super::{held_flags, Dirtied};
    use std::vec;
    use std::vec::Vec;

    fn held(d: &Dirtied, first: u64, n: usize) -> Vec<bool> {
        let mut e = vec![0u32; n];
        if !d.mark_pending(first, &mut e) {
            return vec![false; n];
        }
        held_flags(&mut e);
        e.iter().map(|&h| h != 0).collect()
    }

    #[test]
    fn pending_pages_are_marked_in_one_walk() {
        let mut d = Dirtied::default();
        assert_eq!(held(&d, 0, 4), [false; 4]);
        d.note(10, 12);
        d.note(20, 20);
        d.note(11, 25); // overlaps both, not merged with the one before it (written out of order)
        d.note(40, 40);
        let h = held(&d, 8, 22);
        let want: Vec<bool> = (8..30).map(|p| (10..=25).contains(&p)).collect();
        assert_eq!(h, want);
        assert_eq!(held(&d, 26, 14), [false; 14], "outside every range");
        assert_eq!(held(&d, 0, 0), Vec::<bool>::new());
    }

    #[test]
    fn a_running_flush_keeps_its_ranges_pending_and_a_failed_one_notes_them_again() {
        let mut d = Dirtied::default();
        d.note(5, 6);
        let (runs, lost) = d.start_flush();
        assert!(!lost);
        assert_eq!(held(&d, 4, 4), [false, true, true, false], "taken by the flush, not on the disk yet");
        d.note(7, 7); // a write while the flush runs
        assert_eq!(held(&d, 4, 4), [false, true, true, true]);
        // The flush failed: noted again before `flushing` goes (CachedDiskDev::flush).
        for (a, b) in runs {
            d.note(a, b);
        }
        d.end_flush();
        assert_eq!(held(&d, 4, 4), [false, true, true, true], "a failed flush leaves them pending");
        let _ = d.start_flush();
        d.end_flush();
        assert_eq!(held(&d, 4, 4), [false; 4], "flushed");
        d.lost = true;
        assert_eq!(held(&d, 100, 3), [true; 3], "a range not noted: everything is pending");
    }
}
