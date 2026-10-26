//! What the ext4 code reads and writes: a volume as bytes in whole blocks of the file system. In the kernel a
//! `block::Disk` (`DiskDev`) or a file of another file system (`ImageDev`, the loop image `\BITSOS2\data.img`
//! on the boot stick's FAT); in host tests an image in memory that records every write in order (the cut-power
//! tests replay the first k of them) and fails on demand.
//!
//! ext4 keeps no block cache of its own: an image file has its own page cache below, and a disk mount gets one
//! by `block::bdev::DiskFile` (`CachedDiskDev`, FS-8c); `DiskDev` is the uncached path (each call one request).

use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use block::prio::Lift;
use block::Disk;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::SpinLock;
use crate::errno::*;
use vfs::inode::Inode;
use vfs::pagecache::SyncKind;

pub trait Dev: Send + Sync {
    /// Size in bytes.
    fn size(&self) -> u64;
    /// Reads `buf.len()` bytes at byte offset `at` (both multiples of 512).
    fn read(&self, at: u64, buf: &mut [u8]) -> Result<(), i64>;
    fn write(&self, at: u64, data: &[u8]) -> Result<(), i64>;
    /// Everything written so far on stable storage.
    fn flush(&self) -> Result<(), i64>;
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
}

/// Recorded writes: (byte offset, bytes), in order; a flush is recorded as an empty write at `u64::MAX`.
pub type WriteLog = Vec<(u64, Vec<u8>)>;

/// A volume image in memory (host tests, ktests).
pub struct MemDev {
    img: SpinLock<Vec<u8>>,
    log: SpinLock<Option<WriteLog>>,
    fail_writes: AtomicBool,
    fail_reads: AtomicBool,
    /// Writes after which every further write fails (a cut power: the device stops).
    stop_after: AtomicU64,
    writes: AtomicU64,
    settles: AtomicU64,
}

impl MemDev {
    pub fn new(img: Vec<u8>) -> MemDev {
        MemDev {
            img: SpinLock::named("ext4.memdev", img),
            log: SpinLock::named("ext4.memdev.log", None),
            fail_writes: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
            stop_after: AtomicU64::new(u64::MAX),
            writes: AtomicU64::new(0),
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
    pub fn fail_writes(&self, on: bool) {
        self.fail_writes.store(on, Ordering::Relaxed);
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
    /// How often `settle` was called (fsync, syncfs; never freeze).
    pub fn settles(&self) -> u64 {
        self.settles.load(Ordering::Relaxed)
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
        if let Some(l) = self.log.lock().as_mut() {
            l.push((at, data.to_vec()));
        }
        Ok(())
    }
    fn flush(&self) -> Result<(), i64> {
        if let Some(l) = self.log.lock().as_mut() {
            l.push((u64::MAX, Vec::new()));
        }
        Ok(())
    }
    fn settle(&self) -> Result<(), i64> {
        self.settles.fetch_add(1, Ordering::Relaxed);
        Ok(())
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

/// A partition (or a whole disk) of a `block::Disk` from block `start`: each call waits for its request, so
/// the order of the ext4 code's writes is the order they reach the device.
pub struct DiskDev {
    disk: Arc<Disk>,
    start: u64,
    bytes: u64,
    bs: u64,
    lift: Lift,
}

impl DiskDev {
    pub fn new(disk: Arc<Disk>, start: u64, blocks: u64) -> DiskDev {
        let bs = disk.block_size() as u64;
        DiskDev { disk, start, bytes: blocks * bs, bs, lift: Lift::new() }
    }
    fn lba(&self, at: u64, len: usize) -> Result<u64, i64> {
        if at % self.bs != 0 || len as u64 % self.bs != 0 || at.checked_add(len as u64).is_none_or(|e| e > self.bytes) {
            return Err(EIO);
        }
        Ok(self.start + at / self.bs)
    }
    fn read_whole(&self, at: u64, buf: &mut [u8]) -> Result<(), i64> {
        let lba = self.lba(at, buf.len())?;
        self.disk.read(lba, buf, self.lift.prio()).map_err(|e| e.errno())
    }
}

impl Dev for DiskDev {
    fn size(&self) -> u64 {
        self.bytes
    }
    fn read(&self, at: u64, buf: &mut [u8]) -> Result<(), i64> {
        read_part(self.bs, at, buf, |a, b| self.read_whole(a, b))
    }
    fn write(&self, at: u64, data: &[u8]) -> Result<(), i64> {
        write_part(self.bs, at, data, |a, b| self.read_whole(a, b), |a, d| {
            let lba = self.lba(a, d.len())?;
            self.disk.write(lba, d, false, self.lift.prio()).map_err(|e| e.errno())
        })
    }
    fn flush(&self) -> Result<(), i64> {
        self.disk.flush(self.lift.prio()).map_err(|e| e.errno())
    }
    fn settle(&self) -> Result<(), i64> {
        self.disk.settle().map_err(|e| e.errno())
    }
    fn lift(&self) -> Option<&Lift> {
        Some(&self.lift)
    }
}

/// A partition (or a whole disk) of a `block::Disk` through the disk's page cache (`block::bdev::DiskFile`,
/// Linux's block-device mapping that ext4's buffer heads live in): metadata reads hit memory after the first
/// time. Writes land in the cache; `flush` writes every dirty page back and flushes the device, the barrier the
/// journal puts before and after a commit block (the same contract as `ImageDev` on a file's cache).
pub struct CachedDiskDev {
    cache: vfs::pagecache::PageCache,
    disk: Arc<Disk>,
    start: u64,
    bytes: u64,
    lift: Lift,
}

impl CachedDiskDev {
    pub fn new(disk: Arc<Disk>, start: u64, blocks: u64) -> CachedDiskDev {
        let bs = disk.block_size() as u64;
        CachedDiskDev { cache: block::bdev::DiskFile::page_cache(disk.clone()), disk, start: start * bs, bytes: blocks * bs, lift: Lift::new() }
    }
    fn at(&self, at: u64, len: usize) -> Result<u64, i64> {
        if at.checked_add(len as u64).is_none_or(|e| e > self.bytes) {
            return Err(EIO);
        }
        Ok(self.start + at)
    }
}

impl Dev for CachedDiskDev {
    fn size(&self) -> u64 {
        self.bytes
    }
    fn read(&self, at: u64, buf: &mut [u8]) -> Result<(), i64> {
        let pos = self.at(at, buf.len())?;
        let _f = block::prio::floor(self.lift.prio());
        if self.cache.read_at(pos, buf)? != buf.len() {
            return Err(EIO);
        }
        Ok(())
    }
    fn write(&self, at: u64, data: &[u8]) -> Result<(), i64> {
        let pos = self.at(at, data.len())?;
        let _f = block::prio::floor(self.lift.prio());
        if self.cache.write_at(pos, data)? != data.len() {
            return Err(EIO);
        }
        Ok(())
    }
    fn flush(&self) -> Result<(), i64> {
        let _f = block::prio::floor(self.lift.prio());
        self.cache.sync_all(SyncKind::Fsync)
    }
    fn settle(&self) -> Result<(), i64> {
        self.disk.settle().map_err(|e| e.errno())
    }
    fn lift(&self) -> Option<&Lift> {
        Some(&self.lift)
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
