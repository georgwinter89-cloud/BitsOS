//! A regular ext4 file behind the VFS: its page cache (FS-5) with the volume as the backend. A write that
//! reaches pages the disk does not map yet promises their blocks first (`Volume::reserve`): a full volume
//! answers ENOSPC at `write`, not at write-back (Linux's delayed-allocation reservation). The write-back
//! writes runs (`Volume::write`: blocks marked used, data, then extents and inode) and stores the times the
//! VFS keeps; fsync then flushes the device, after the cache's lock is released.

use crate::vol::{Meta, Volume};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use crate::errno::*;
use ksync::SpinLock;
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
}

struct Back {
    vol: Arc<Volume>,
    ino: u32,
    file: Weak<Ext4File>,
}

impl Ext4File {
    pub fn new(vol: Arc<Volume>, ino: u32, size: u64) -> Arc<Ext4File> {
        Arc::new_cyclic(|me: &Weak<Ext4File>| {
            let back = Arc::new(Back { vol: vol.clone(), ino, file: me.clone() });
            Ext4File { vol, ino, cache: PageCache::with_backend(back, size), me: SpinLock::named("ext4.file.me", None), registered: AtomicBool::new(false) }
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
        self.vol.forget(self.ino);
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
        f.cache.write_at(pos, &buf[..n])
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
        self.0.vol.stat(self.0.ino).map(|d| d.blocks512(&self.0.vol.geo)).unwrap_or(0)
    }
}

impl Backend for Back {
    fn read_page(&self, pgoff: u64, page: &Arc<CachePage>) -> Result<(), i64> {
        let mut buf = alloc::vec![0u8; PAGE as usize];
        self.vol.read(self.ino, pgoff * PAGE, &mut buf)?;
        page.write(0, &buf);
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
        self.vol.write(self.ino, off, &data, size, first..first + pages.len() as u64)
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
}
