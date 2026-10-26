//! File mappings (FS-5): the file side of MM's file-backed VMAs.
//!
//! `check` is the part of Linux's `do_mmap` that looks at the file (linux-6.6/mm/mmap.c:1279-1337, after the
//! address checks MM does), in Linux's order, and makes a `FileMap`: one per `mmap` call, shared by the VMAs
//! a later split makes of it (and by a fork's copy). MM reaches it through the `MapBacking` trait:
//!
//! - `get_page(pgoff, write)` on a fault: the page-cache frame of file page `pgoff`, **pinned** for this
//!   mapping (a strong reference to the cache page, counted per frame). Past the end of the file: `Bus`
//!   (Linux sends SIGBUS). A MAP_PRIVATE mapping maps it read-only and copies it on the first write (COW,
//!   MM's side); a MAP_SHARED one maps it with the VMA's rights, so both mappings of the Chromium pattern
//!   (create in /dev/shm, unlink, mmap twice) share one frame.
//! - `put_page(phys)` after MM removed the page-table entry **and the TLB shootdown was confirmed**: the pin
//!   goes; `false` means the frame is not this mapping's (MM's own private copy, MM frees it).
//! - `sync(first, last)` for msync(MS_SYNC): the cache's write-back of that range (tmpfs: nothing to write).
//!
//! The share counts: a cache page lives while the cache holds it or any pin does; the file (its inode and
//! cache) lives while an fd, a name or a `FileMap` holds it; a `FileMap` lives while a VMA holds it. So
//! close, unlink and munmap can come in any order and the frames go back exactly when the last of them ends:
//! never earlier (no program can reach a freed frame) and never later than the last unmap.

use crate::fd::OpenFile;
use crate::index::PageIndex;
use crate::inode::Inode;
use crate::mount_ns::Path;
use crate::pagecache::{CachePage, PageCache, PageFault, SyncKind};
use crate::Errno;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::Mutex;
use uapi::errno::*;

pub const PROT_WRITE: u64 = 2;
pub const PROT_EXEC: u64 = 4;
pub const MAP_SHARED: u64 = 0x01;
pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_SHARED_VALIDATE: u64 = 0x03;
pub const MAP_TYPE: u64 = 0x0f;
pub const MAP_GROWSDOWN: u64 = 0x100;
pub const MAP_SYNC: u64 = 0x8_0000;
/// linux-6.6/include/linux/mman.h:41 `LEGACY_MAP_MASK`: SHARED, PRIVATE, FIXED, ANONYMOUS, DENYWRITE,
/// EXECUTABLE, UNINITIALIZED, GROWSDOWN, LOCKED, NORESERVE, POPULATE, NONBLOCK, STACK, HUGETLB, 32BIT,
/// ABOVE4G, HUGE_2MB, HUGE_1GB (x86-64 values).
pub const LEGACY_MAP_MASK: u64 = 0x01 | 0x02 | 0x10 | 0x20 | 0x800 | 0x1000 | 0x400_0000 | 0x100 | 0x2000 | 0x4000 | 0x8000 | 0x1_0000 | 0x2_0000 | 0x4_0000 | 0x40 | 0x80 | (21 << 26) | (30 << 26);
/// `MAX_LFS_FILESIZE` on 64-bit.
const MAX_LFS_FILESIZE: u64 = i64::MAX as u64;

/// What MM needs from the object behind a file-backed (or later any shared) VMA.
pub trait MapBacking: Send + Sync {
    /// The frame of page `pgoff`, pinned for this mapping until `put_page`. `write`: a write fault of a shared
    /// mapping (the page becomes dirty for write-back).
    fn get_page(&self, pgoff: u64, write: bool) -> Result<u64, PageFault>;
    /// The mapping no longer maps `phys` (entry removed, shootdown confirmed). False: not this backing's
    /// frame (MM's private copy, MM frees it).
    fn put_page(&self, phys: u64) -> bool;
    /// MAP_SHARED with a writable fd (Linux VM_SHARED): writes reach the file.
    fn shared(&self) -> bool;
    /// Whether write-back needs to see writes through the mapping (a file system with a backend): MM then maps
    /// pages read-only on read faults so the first write faults with `write = true`. tmpfs: false.
    fn track_dirty(&self) -> bool;
    /// msync(MS_SYNC) of file pages [first, last] (Linux `vfs_fsync_range(file, .., datasync=1)`).
    fn sync(&self, first: u64, last: u64) -> Result<(), Errno>;
    /// The file behind it (for `/proc/<pid>/maps`), if it is one.
    fn file(&self) -> Option<&FileMap> {
        None
    }
}

/// MM-rmap (FS-14 change request 1): MM's reverse map, which removes a file's pages from every mapping
/// (Linux `unmap_mapping_range`, linux-6.6/mm/memory.c:3565). Called without any vfs lock held: MM takes each
/// address space's lock (`mm.as`), which comes before `vfs.inode.data` and `vfs.map.pins` (R5).
pub trait Unmapper: Sync {
    /// Removes the entries of file pages `first..=last` of `inode` from every address space (with the TLB
    /// shootdown) and hands the pages back (`put_page`). `even_cows`: private copies of those pages go too
    /// (truncate; a hole punch keeps them, as Linux's `even_cows = 0`).
    fn unmap(&self, inode: &Inode, first: u64, last: u64, even_cows: bool);
}

static UNMAPPER: ksync::OnceRef<dyn Unmapper> = ksync::OnceRef::new();

/// MM installs its reverse map once at boot (false: one was installed already).
pub fn set_unmapper(u: &'static dyn Unmapper) -> bool {
    UNMAPPER.set(u)
}

/// `unmap_mapping_range` of file pages `first..=last` (nothing without MM, e.g. in host tests).
pub(crate) fn unmap_mapping(inode: &Inode, first: u64, last: u64, even_cows: bool) {
    if first <= last
        && let Some(u) = UNMAPPER.get()
    {
        u.unmap(inode, first, last, even_cows);
    }
}

/// The result of `check`: how MM sets the VMA up.
pub struct Mapped {
    pub backing: Arc<FileMap>,
    /// Linux VM_SHARED: MAP_SHARED with an fd open for writing.
    pub shared: bool,
    /// Linux VM_MAYWRITE: an mprotect may add PROT_WRITE (false for MAP_SHARED of a read-only fd: EACCES).
    pub may_write: bool,
    /// Linux VM_MAYEXEC: false on a noexec mount (FS-13), so a later mprotect(PROT_EXEC) must be refused.
    pub may_exec: bool,
}

static PINS: AtomicU64 = AtomicU64::new(0);
static MAPS: AtomicU64 = AtomicU64::new(0);
klog::counter!(MAP_FAULTS, "vfs.map.faults");
klog::counter!(MAP_BUS, "vfs.map.sigbus");

/// Page pins of all file mappings now (a page mapped in two places counts twice).
pub fn pinned_pages() -> u64 {
    PINS.load(Ordering::Relaxed)
}

/// File mappings alive now.
pub fn live_maps() -> u64 {
    MAPS.load(Ordering::Relaxed)
}

/// The fd of an mmap call (`ksys_mmap_pgoff`'s `fget`: an O_PATH fd is EBADF).
pub fn fd_for_mmap(f: Option<Arc<OpenFile>>) -> Result<Arc<OpenFile>, Errno> {
    match f {
        Some(f) if !f.is_path_only() => Ok(f),
        _ => Err(EBADF),
    }
}

/// `file_mmap_size_max` (linux-6.6/mm/mmap.c:1163): regular files, block devices and sockets allow
/// MAX_LFS_FILESIZE, everything else the whole unsigned range.
fn size_max(f: &OpenFile) -> u64 {
    match f.file.mode() & uapi::S_IFMT {
        uapi::S_IFREG | crate::abi::S_IFBLK | crate::abi::S_IFSOCK => MAX_LFS_FILESIZE,
        _ => u64::MAX,
    }
}

/// The file checks of `do_mmap` for `mmap(.., prot, flags, f, pgoff << 12)` of `len` bytes (page-aligned,
/// non-zero; MM checked everything before them), in Linux's order: EOVERFLOW (`file_mmap_ok`), EOPNOTSUPP
/// (MAP_SHARED_VALIDATE with flags the file does not support), EACCES (PROT_WRITE on a shared mapping of an fd
/// not open for writing; any mapping of an fd not open for reading), ENODEV (no mmap: directories, devices,
/// proc and sys files, pipes), EINVAL (MAP_GROWSDOWN; a map type other than SHARED/PRIVATE/SHARED_VALIDATE),
/// EPERM (PROT_EXEC of a file on a noexec mount, FS-13), then the file's own mmap: EPERM for a writable
/// MAP_SHARED mapping of a write-sealed tmpfs file (FS-10).
pub fn check(f: &OpenFile, prot: u64, flags: u64, pgoff: u64, len: u64) -> Result<Mapped, Errno> {
    let max = size_max(f);
    if max != 0 && len > max {
        return Err(EOVERFLOW);
    }
    if pgoff > (max - len) >> 12 {
        return Err(EOVERFLOW);
    }
    let mut flags = flags;
    let ty = flags & MAP_TYPE;
    let (mut shared, mut may_write) = (false, true);
    match ty {
        MAP_SHARED | MAP_SHARED_VALIDATE => {
            if ty == MAP_SHARED {
                // Unknown flags are ignored with plain MAP_SHARED (backward compatibility).
                flags &= LEGACY_MAP_MASK;
            }
            // tmpfs supports no extension flag (shmem has no `mmap_supported_flags`): MAP_SYNC and friends
            // are EOPNOTSUPP with MAP_SHARED_VALIDATE.
            if flags & !LEGACY_MAP_MASK != 0 {
                return Err(EOPNOTSUPP);
            }
            if prot & PROT_WRITE != 0 && !f.can_write() {
                return Err(EACCES);
            }
            shared = f.can_write();
            may_write = f.can_write();
        }
        MAP_PRIVATE => {}
        _ => return Err(EINVAL),
    }
    if !f.can_read() {
        return Err(EACCES);
    }
    // FS-13: `path_noexec` in the PRIVATE/SHARED arms (linux-6.6/mm/mmap.c:1323): PROT_EXEC is EPERM,
    // otherwise VM_MAYEXEC goes.
    let noexec = f.path.as_ref().is_some_and(|p| p.noexec());
    if noexec && prot & PROT_EXEC != 0 {
        klog::event!(Info, "vfs", "mmap.noexec", mount = f.path.as_ref().map_or(0, |p| p.mnt.id));
        return Err(EPERM);
    }
    let path = f.path.clone().ok_or(ENODEV)?;
    let Some(cache) = path.dentry.inode.page_cache() else { return Err(ENODEV) };
    if flags & MAP_GROWSDOWN != 0 {
        return Err(EINVAL);
    }
    // FS-10: the file's own mmap (`shmem_mmap` -> `seal_check_write`) and `mapping_map_writable`: a write seal
    // refuses a new writable shared mapping (EPERM) and takes VM_MAYWRITE from a read-only one; a shared
    // mapping that may write is counted (F_SEAL_WRITE is EBUSY while one exists).
    let mut counted = false;
    if ty != MAP_PRIVATE {
        counted = cache.map_shared(prot & PROT_WRITE != 0, f.can_write()).inspect_err(|&e| {
            crate::memfd::seal_refused(crate::memfd::Refused::Map, path.dentry.inode.meta.ino, cache.seals().unwrap_or(0), e);
        })?;
        may_write = counted;
    }
    let backing = FileMap::build(path, shared, ty != MAP_PRIVATE, counted);
    Ok(Mapped { backing, shared, may_write, may_exec: !noexec })
}

struct Pin {
    /// The reference keeps the cache page (and its frame) alive while it is mapped.
    page: Arc<CachePage>,
    count: u64,
    /// MM-9: a write fault of this tracked shared mapping got the page. Write-back clears the page's dirty mark
    /// but nothing write-protects the mapping again, so a later store goes unseen; the last pin therefore marks
    /// the page dirty once more (Linux `zap_pte_range`: `folio_mark_dirty` for a dirty pte). Without it reclaim
    /// would take the then clean, unmapped page and lose the store.
    written: bool,
}

/// One file mapping (see the module docs).
pub struct FileMap {
    path: Path,
    shared: bool,
    /// MAP_SHARED or MAP_SHARED_VALIDATE (Linux VM_MAYSHARE: the `s` of `/proc/<pid>/maps`).
    may_share: bool,
    /// Pinned cache pages by frame number.
    pins: Mutex<PageIndex<Pin>>,
    /// FS-10: counted in the file's `writable_maps` (a shared mapping that may write); uncounted at the end.
    counted: bool,
}

impl FileMap {
    pub fn new(path: Path, shared: bool, may_share: bool) -> Arc<FileMap> {
        Self::build(path, shared, may_share, false)
    }
    /// `counted`: the file's cache counted this mapping as one that may write (`PageCache::map_shared`).
    pub(crate) fn build(path: Path, shared: bool, may_share: bool, counted: bool) -> Arc<FileMap> {
        MAPS.fetch_add(1, Ordering::Relaxed);
        Arc::new(FileMap { path, shared, may_share, pins: Mutex::named("vfs.map.pins", PageIndex::new()), counted })
    }
    /// MAP_SHARED of any fd (a read-only fd's shared mapping is never writable but still shared).
    pub fn may_share(&self) -> bool {
        self.may_share
    }
    /// Where the file was opened (its name for `/proc/<pid>/maps`; a removed name shows as deleted there).
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn inode(&self) -> &Arc<Inode> {
        &self.path.dentry.inode
    }
    fn cache(&self) -> Option<&PageCache> {
        self.path.dentry.inode.page_cache()
    }
    /// Pages this mapping has pinned now.
    pub fn pinned(&self) -> u64 {
        let g = self.pins.lock();
        let mut n = 0;
        g.for_each(&mut |_, p| {
            n += p.count;
            true
        });
        n
    }
}

impl MapBacking for FileMap {
    fn get_page(&self, pgoff: u64, write: bool) -> Result<u64, PageFault> {
        MAP_FAULTS.inc();
        let cache = self.cache().ok_or(PageFault::Io)?;
        let page = cache.page_for_map(pgoff, write && self.shared).inspect_err(|e| {
            if *e == PageFault::Bus {
                MAP_BUS.inc();
            }
        })?;
        #[cfg(target_os = "none")]
        let phys = page.phys().ok_or(PageFault::NoMemory)?;
        #[cfg(not(target_os = "none"))]
        let phys = page.ident();
        let written = write && self.shared && cache.has_backend();
        let mut g = self.pins.lock();
        let key = phys >> 12;
        if let Some(p) = g.get_mut(key) {
            p.count += 1;
            p.written |= written;
        } else {
            g.insert(key, Pin { page, count: 1, written }).map_err(|_| PageFault::NoMemory)?;
        }
        PINS.fetch_add(1, Ordering::Relaxed);
        Ok(phys)
    }

    fn put_page(&self, phys: u64) -> bool {
        let key = phys >> 12;
        let gone = {
            let mut g = self.pins.lock();
            let Some(p) = g.get_mut(key) else { return false };
            p.count -= 1;
            if p.count == 0 { g.remove(key) } else { None }
        };
        PINS.fetch_sub(1, Ordering::Relaxed);
        // Dirty again while the pin still holds the page, so reclaim never sees it clean and unmapped in between.
        if let Some(p) = &gone
            && p.written
        {
            p.page.set_dirty();
        }
        // The last pin of a page truncate cut off frees its frame here, outside the pin lock.
        drop(gone);
        true
    }

    fn shared(&self) -> bool {
        self.shared
    }

    fn track_dirty(&self) -> bool {
        self.shared && self.cache().is_some_and(|c| c.has_backend())
    }

    fn sync(&self, first: u64, last: u64) -> Result<(), Errno> {
        if !self.shared {
            return Ok(());
        }
        match self.cache() {
            Some(c) => c.sync_range(first, last, SyncKind::Msync),
            None => Ok(()),
        }
    }

    fn file(&self) -> Option<&FileMap> {
        Some(self)
    }
}

impl Drop for FileMap {
    fn drop(&mut self) {
        MAPS.fetch_sub(1, Ordering::Relaxed);
        if self.counted
            && let Some(c) = self.cache()
        {
            c.unmap_writable();
        }
        let g = self.pins.get_mut();
        if !g.is_empty() {
            // MM dropped the mapping while page-table entries may still point at these frames (a bug on its
            // side): keep the frames out of use for good rather than hand them out again.
            let mut n = 0u64;
            g.for_each(&mut |_, p| {
                n += p.count;
                true
            });
            PINS.fetch_sub(n, Ordering::Relaxed);
            klog::event!(Error, "vfs", "map.pins_leaked", pages = n;
                "a file mapping ended with pages still pinned: their frames are kept out of use");
            let pins = core::mem::take(g);
            core::mem::forget(pins);
        }
    }
}
