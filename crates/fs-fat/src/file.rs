//! A FAT file behind the VFS: its page cache (FS-5) with this file system as the backend, so reads, writes,
//! mmap and write-back work as for every cached file. `write_at`/`truncate` ask the stick rules first and,
//! for `\BITSOS2\CONFIG.TXT`, the guarded-keys rule on the content the call would leave (one change of the
//! file at a time, so two harmless fragments can never meet as a guarded line); the write-back asks them again
//! (a shared mapping changes pages without `write_at`). A write that makes the file longer promises its
//! clusters first (`Volume::reserve`): a full stick answers ENOSPC at `write`, not later at write-back.

use crate::protect::{self, Class};
use crate::vol::Volume;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use uapi::errno::*;
use vfs::inode::InodeOps;
use vfs::pagecache::{Backend, CachePage, PageCache, SyncKind};
use zeroize::Zeroize;

const PAGE: u64 = 4096;

/// A heap buffer that may hold CONFIG.TXT's text (RB-18 CR-2): when `secret`, zeroed on every way out, the `?`s
/// included. Never grown: a move would leave the old buffer unzeroed, so the capacity is reserved first.
pub(crate) struct Buf {
    pub(crate) bytes: Vec<u8>,
    secret: bool,
}

impl Buf {
    pub(crate) fn reserved(capacity: usize, secret: bool) -> Result<Self, i64> {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(capacity).map_err(|_| ENOMEM)?;
        Ok(Self { bytes, secret })
    }

    fn zeroed(len: usize, secret: bool) -> Result<Self, i64> {
        let mut b = Self::reserved(len, secret)?;
        b.bytes.resize(len, 0);
        Ok(b)
    }

    /// The whole capacity, also what a shrink left past the length: the tree builds zeroize without `alloc`, where
    /// `Vec::zeroize` is the slice's and stops at the length (a truncate to 0 would zero nothing).
    pub(crate) fn wipe(&mut self) {
        self.bytes.resize(self.bytes.capacity(), 0);
        self.bytes.as_mut_slice().zeroize();
    }
}

impl Drop for Buf {
    fn drop(&mut self) {
        if self.secret {
            self.wipe();
        }
    }
}

pub struct FatFile {
    pub vol: Arc<Volume>,
    pub ino: u64,
    pub cache: PageCache,
    /// A refused CONFIG.TXT write-back was logged (once per file).
    guard_logged: AtomicBool,
    /// Registered with the background write-back (at its first write).
    registered: AtomicBool,
    /// CONFIG.TXT: held from reading the old content to the cache change (`fat.config`, before `vfs.inode.data`).
    config: ksync::Mutex<()>,
}

/// The backend half (the page cache holds it; it reaches the file through a weak reference).
struct Data {
    vol: Arc<Volume>,
    ino: u64,
    file: Weak<FatFile>,
}

/// FS-14b: pages a missed page brings into the cache in the same request (USB sticks pay per command):
/// Linux's VM_READAHEAD_PAGES (128 KiB), which a fault reads around the missed page and a read() after it
/// (`vfs::pagecache::Reach`); a larger read() takes what it asks for, up to 1 MiB. Every page read stays in
/// the cache. Before FS-14b one buffer per file held them outside the cache and only the missed page went in:
/// mapped files faulted in a jumping order read each buffer again and again (Brave's cold start: 8 times).
const WINDOW: u64 = 32;

impl FatFile {
    pub fn new(vol: Arc<Volume>, ino: u64, size: u64) -> Arc<FatFile> {
        Arc::new_cyclic(|me: &Weak<FatFile>| {
            let backend = Arc::new(Data { vol: vol.clone(), ino, file: me.clone() });
            FatFile {
                vol,
                ino,
                cache: PageCache::with_backend(backend, size),
                guard_logged: AtomicBool::new(false),
                registered: AtomicBool::new(false),
                config: ksync::Mutex::named("fat.config", ()),
            }
        })
    }

    /// The whole cached content of CONFIG.TXT (EFBIG above `CONFIG_MAX`: never a huge copy).
    fn whole(&self) -> Result<Buf, i64> {
        if self.cache.size() > protect::CONFIG_MAX {
            return Err(self.vol.refused("config.size", self.ino, EFBIG));
        }
        let mut v = Buf::zeroed(self.cache.size() as usize, true)?;
        let n = self.cache.read_at(0, &mut v.bytes)?;
        v.bytes.truncate(n);
        Ok(v)
    }

    /// The background flusher writes this file's dirty pages from its first write on.
    fn register(self: &Arc<Self>) {
        if !self.registered.swap(true, Ordering::AcqRel) {
            let w: Weak<dyn vfs::writeback::Flush> = Arc::downgrade(self) as Weak<dyn vfs::writeback::Flush>;
            if vfs::writeback::register(w).is_err() {
                self.registered.store(false, Ordering::Release);
            }
        }
    }

    fn is_config(&self) -> bool {
        self.vol.class(self.ino) == Ok(Class::Config)
    }

    /// R8a/R8d for CONFIG.TXT: the content `new` may drop guarded lines, never add or change one.
    fn guard(&self, old: &[u8], new: &[u8]) -> Result<(), i64> {
        protect::config_guard(old, new).map_err(|key| {
            klog::event!(Warn, "fat", "config.refused", key = key; "CONFIG.TXT: a guarded line would be added or changed (only Georg sets it; R8a/R8d)");
            self.vol.refused("config", self.ino, EPERM)
        })
    }
}

impl Drop for FatFile {
    fn drop(&mut self) {
        self.vol.forget(self.ino);
    }
}

impl vfs::writeback::Flush for FatFile {
    fn flush(&self, why: SyncKind) -> Result<(), i64> {
        self.cache.sync_all(why)
    }
}

/// The InodeOps the VFS holds (`Data::External`).
pub struct Ops(pub Arc<FatFile>);

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
        f.vol.check_write(f.ino)?;
        if pos >= crate::vol_file::MAX_SIZE {
            return Err(EFBIG);
        }
        let n = buf.len().min((crate::vol_file::MAX_SIZE - pos) as usize);
        let end = pos + n as u64;
        let _one = if f.is_config() {
            let g = f.config.lock();
            if end > protect::CONFIG_MAX {
                return Err(f.vol.refused("config.size", f.ino, EFBIG));
            }
            let old = f.whole()?;
            let mut new = Buf::reserved(old.bytes.len().max(end as usize), true)?;
            new.bytes.extend_from_slice(&old.bytes);
            if new.bytes.len() < end as usize {
                new.bytes.resize(end as usize, 0);
            }
            new.bytes[pos as usize..end as usize].copy_from_slice(&buf[..n]);
            f.guard(&old.bytes, &new.bytes)?;
            Some(g)
        } else {
            None
        };
        if end > f.cache.size() {
            f.vol.reserve(f.ino, end)?;
        }
        f.register();
        f.cache.write_at(pos, &buf[..n])
    }
    /// `fat_fallocate` (linux-6.6/fs/fat/file.c:260): mode 0 is an expanding truncate (the new bytes read as
    /// zeros); FALLOC_FL_KEEP_SIZE promises the clusters up to the end (ENOSPC now, the size stays: V2 allocates
    /// at write-back, so the promise is what Linux's preallocated clusters give a program); other modes
    /// EOPNOTSUPP.
    fn fallocate(&self, mode: u32, offset: u64, len: u64) -> Result<bool, i64> {
        if mode & !vfs::abi::FALLOC_FL_KEEP_SIZE != 0 {
            return Err(EOPNOTSUPP);
        }
        let f = &self.0;
        let end = offset.checked_add(len).ok_or(EFBIG)?;
        if end > crate::vol_file::MAX_SIZE {
            return Err(EFBIG);
        }
        if mode & vfs::abi::FALLOC_FL_KEEP_SIZE != 0 {
            f.vol.check_write(f.ino)?;
            f.vol.reserve(f.ino, end)?;
            return Ok(false);
        }
        if end <= self.size() {
            return Ok(false);
        }
        self.truncate(end)?;
        Ok(true)
    }
    fn truncate(&self, size: u64) -> Result<(), i64> {
        let f = &self.0;
        f.vol.check_write(f.ino)?;
        if size > crate::vol_file::MAX_SIZE {
            return Err(EFBIG);
        }
        let _one = if f.is_config() {
            let g = f.config.lock();
            if size > protect::CONFIG_MAX {
                return Err(f.vol.refused("config.size", f.ino, EFBIG));
            }
            let old = f.whole()?;
            let mut new = Buf::reserved(size as usize, true)?;
            new.bytes.extend_from_slice(&old.bytes[..old.bytes.len().min(size as usize)]);
            new.bytes.resize(size as usize, 0);
            f.guard(&old.bytes, &new.bytes)?;
            Some(g)
        } else {
            None
        };
        if size < f.cache.size() {
            f.cache.truncate(size)?;
            f.vol.set_size(f.ino, size)
        } else {
            // Dirty pages first, so the disk's size never runs ahead of data that is only cached.
            f.cache.sync_all(SyncKind::Background)?;
            f.vol.set_size(f.ino, size)?;
            f.cache.truncate(size)
        }
    }
    fn sync(&self, why: SyncKind) -> Result<(), i64> {
        self.0.cache.sync_all(why)?;
        self.0.vol.commit(self.0.ino)?;
        self.0.vol.sync(why.durable())?;
        // RB-18: the file's text, secrets included, stays on the stick only.
        if self.0.is_config() {
            self.0.cache.forget_clean();
        }
        Ok(())
    }
    fn blocks(&self) -> u64 {
        self.0.vol.blocks(self.0.ino)
    }
}

impl Backend for Data {
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), i64> {
        self.read_pages(pgoff, core::slice::from_ref(page))
    }
    fn read_window(&self) -> u64 {
        WINDOW
    }
    /// One `read_data` for the whole run (one USB command per 1 MiB extent), copied into the pages.
    fn read_pages(&self, first: u64, pages: &[Arc<CachePage>]) -> Result<(), i64> {
        let len = pages.len() * PAGE as usize;
        let mut buf = Buf::zeroed(len, self.vol.class(self.ino) == Ok(Class::Config))?;
        self.vol.read_data(self.ino, first * PAGE, &mut buf.bytes)?;
        for (p, b) in pages.iter().zip(buf.bytes.chunks(PAGE as usize)) {
            p.write(0, b);
        }
        Ok(())
    }
    fn write_run(&self, first: u64, pages: &[Arc<CachePage>], size: u64) -> Result<(), i64> {
        let run_end = first + pages.len() as u64;
        for (k, part) in pages.chunks(WRITE_STEP_PAGES).enumerate() {
            let at = first + (k * WRITE_STEP_PAGES) as u64;
            let step_end = at + part.len() as u64;
            // `write_data` zero-fills every new cluster up to the length it is told, so a step that named the file's
            // final length would write zeros where the next steps write the data: twice the bytes on the stick.
            // Each step but the last grows the file only to its own end, and never below what the file has.
            let through = if step_end == run_end { size } else { (step_end * PAGE).max(self.vol.disk_size(self.ino)?).min(size) };
            self.write_step(at, part, through)?;
        }
        Ok(())
    }
    fn commit(&self, why: SyncKind) -> Result<(), i64> {
        self.vol.commit(self.ino)?;
        if why.durable() { self.vol.sync(true) } else { Ok(()) }
    }
}

/// The buffer's size in a write-back: a run can be the whole file, and a file of hundreds of MiB would then need one
/// block that long from the buddy allocator (INT-W31: a 388 MiB KERNEL at 4 GiB RAM, ENOMEM with 2 GiB free).
const WRITE_STEP_PAGES: usize = 4096;

impl Data {
    /// One step of `write_run` through one buffer; the file is `size` bytes long after it.
    fn write_step(&self, first: u64, pages: &[Arc<CachePage>], size: u64) -> Result<(), i64> {
        let off = first * PAGE;
        if off >= size {
            return Ok(());
        }
        let len = ((pages.len() as u64 * PAGE).min(size - off)) as usize;
        let mut data = Buf::zeroed(len, self.vol.class(self.ino) == Ok(Class::Config))?;
        for (i, p) in pages.iter().enumerate() {
            let at = i * PAGE as usize;
            if at >= len {
                break;
            }
            let n = (len - at).min(PAGE as usize);
            p.read(0, &mut data.bytes[at..at + n]);
        }
        if let Some(f) = self.file.upgrade()
            && f.is_config()
        {
            let new = f.whole()?;
            let disk_size = self.vol.disk_size(self.ino)?;
            if disk_size > protect::CONFIG_MAX {
                return Err(EFBIG);
            }
            let mut old = Buf::zeroed((disk_size as usize).div_ceil(PAGE as usize) * PAGE as usize, true)?;
            for (i, chunk) in old.bytes.chunks_mut(PAGE as usize).enumerate() {
                self.vol.read_data(self.ino, i as u64 * PAGE, chunk)?;
            }
            old.bytes.truncate(disk_size as usize);
            if let Err(e) = protect::config_guard(&old.bytes, &new.bytes) {
                if !f.guard_logged.swap(true, Ordering::Relaxed) {
                    klog::event!(Warn, "fat", "config.writeback_refused", key = e; "CONFIG.TXT pages changed through a mapping would add or change a guarded line: not written");
                }
                return Err(EPERM);
            }
        }
        self.vol.write_data(self.ino, off, &data.bytes, size)
    }
}
