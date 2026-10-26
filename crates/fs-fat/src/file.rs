//! A FAT file behind the VFS: its page cache (FS-5) with this file system as the backend, so reads, writes,
//! mmap and write-back work as for every cached file. `write_at`/`truncate` ask the stick rules first and,
//! for `\BITSOS2\CONFIG.TXT`, the guarded-keys rule on the content the call would leave (one change of the
//! file at a time, so two harmless fragments can never meet as a guarded line); the write-back asks them again
//! (a shared mapping changes pages without `write_at`). A write that makes the file longer promises its
//! clusters first (`Volume::reserve`): a full stick answers ENOSPC at `write`, not later at write-back.

use crate::protect::{self, Class};
use crate::vol::Volume;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use uapi::errno::*;
use vfs::inode::InodeOps;
use vfs::pagecache::{Backend, CachePage, PageCache, SyncKind};

const PAGE: u64 = 4096;

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
    fn whole(&self) -> Result<Vec<u8>, i64> {
        if self.cache.size() > protect::CONFIG_MAX {
            return Err(self.vol.refused("config.size", self.ino, EFBIG));
        }
        let size = self.cache.size() as usize;
        let mut v = Vec::new();
        v.try_reserve_exact(size).map_err(|_| ENOMEM)?;
        v.resize(size, 0);
        let n = self.cache.read_at(0, &mut v)?;
        v.truncate(n);
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
            let mut new = Vec::new();
            new.try_reserve_exact(old.len().max(end as usize)).map_err(|_| ENOMEM)?;
            new.extend_from_slice(&old);
            if new.len() < end as usize {
                new.resize(end as usize, 0);
            }
            new[pos as usize..end as usize].copy_from_slice(&buf[..n]);
            f.guard(&old, &new)?;
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
            let mut new = f.whole()?;
            let old = new.clone();
            if (size as usize) > new.len() {
                new.try_reserve_exact(size as usize - new.len()).map_err(|_| ENOMEM)?;
            }
            new.resize(size as usize, 0);
            f.guard(&old, &new)?;
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
        self.0.vol.sync(why.durable())
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
        let mut buf = Vec::new();
        buf.try_reserve_exact(len).map_err(|_| ENOMEM)?;
        buf.resize(len, 0);
        self.vol.read_data(self.ino, first * PAGE, &mut buf)?;
        for (p, b) in pages.iter().zip(buf.chunks(PAGE as usize)) {
            p.write(0, b);
        }
        Ok(())
    }
    fn write_run(&self, first: u64, pages: &[Arc<CachePage>], size: u64) -> Result<(), i64> {
        let off = first * PAGE;
        if off >= size {
            return Ok(());
        }
        let len = ((pages.len() as u64 * PAGE).min(size - off)) as usize;
        let mut data = Vec::new();
        data.try_reserve_exact(len).map_err(|_| ENOMEM)?;
        data.resize(len, 0);
        for (i, p) in pages.iter().enumerate() {
            let at = i * PAGE as usize;
            if at >= len {
                break;
            }
            let n = (len - at).min(PAGE as usize);
            p.read(0, &mut data[at..at + n]);
        }
        if let Some(f) = self.file.upgrade()
            && f.is_config()
        {
            let new = f.whole()?;
            let disk_size = self.vol.disk_size(self.ino)?;
            if disk_size > protect::CONFIG_MAX {
                return Err(EFBIG);
            }
            let mut old = vec![0u8; (disk_size as usize).div_ceil(PAGE as usize) * PAGE as usize];
            for (i, chunk) in old.chunks_mut(PAGE as usize).enumerate() {
                self.vol.read_data(self.ino, i as u64 * PAGE, chunk)?;
            }
            old.truncate(disk_size as usize);
            if let Err(e) = protect::config_guard(&old, &new) {
                if !f.guard_logged.swap(true, Ordering::Relaxed) {
                    klog::event!(Warn, "fat", "config.writeback_refused", key = e; "CONFIG.TXT pages changed through a mapping would add or change a guarded line: not written");
                }
                return Err(EPERM);
            }
        }
        self.vol.write_data(self.ino, off, &data, size)
    }
    fn commit(&self, why: SyncKind) -> Result<(), i64> {
        self.vol.commit(self.ino)?;
        if why.durable() { self.vol.sync(true) } else { Ok(()) }
    }
}
