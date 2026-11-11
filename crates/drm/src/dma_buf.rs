//! DR-5: the dma-buf fd type (linux-6.6/drivers/dma-buf/dma-buf.c): one buffer shared between processes and
//! devices. A buffer comes from an exporter (`Exporter`: `/dev/udmabuf` today, GEM objects with DR-6) and has a
//! reservation object (`Resv`) for implicit sync. The fd answers like Linux's `dma_buf_fops`:
//!
//! - fstat: an anonymous inode of the dmabuf file system (mode 0600 without type bits, the size, a unique inode
//!   number per buffer, `dma_buf_getfile`); read and write EINVAL (no `.read`/`.write`);
//! - lseek: only SEEK_END and SEEK_SET with offset 0 (the size, 0), everything else EINVAL (`dma_buf_llseek`);
//! - mmap: the exporter's pages through MM's device mappings; a range past the buffer is EINVAL
//!   (`dma_buf_mmap_internal`), then the exporter's own rule (udmabuf: MAP_SHARED only);
//! - poll: EPOLLIN once the writers' fences have signaled, EPOLLOUT once all implicit-sync fences have
//!   (`dma_buf_poll`), computed now; the reservation object wakes the queue after a fence signaled (FS-17);
//! - ioctls: DMA_BUF_IOCTL_SYNC (flag checks of `dma_buf_ioctl`, then the exporter's begin/end), SET_NAME_A/B
//!   (`strndup_user` of 32 bytes), EXPORT_SYNC_FILE / IMPORT_SYNC_FILE (`dma_buf_export_sync_file`,
//!   `dma_buf_import_sync_file`), anything else ENOTTY;
//! - fdinfo: `size:`, `count:`, `exp_name:` and `name:` (`dma_buf_show_fdinfo`); /proc/self/fd links read
//!   `/dmabuf:<name>` (`dmabuffs_dname`).
//!
//! As with sync_file, the VFS has no downcast for `dyn File`: a registry finds the buffer behind an fd by its
//! address (compared only).

use crate::fence;
use crate::resv::{Resv, ResvWatch, Usage};
use crate::sync_file::{self, SyncFile};
use crate::uapi::{dma_buf_export_sync_file, dma_buf_import_sync_file, dma_buf_sync};
use crate::uapi::{DMA_BUF_IOCTL_EXPORT_SYNC_FILE, DMA_BUF_IOCTL_IMPORT_SYNC_FILE, DMA_BUF_IOCTL_SYNC, DMA_BUF_NAME_LEN};
use crate::uapi::{DMA_BUF_SET_NAME_A, DMA_BUF_SET_NAME_B, DMA_BUF_SYNC_END, DMA_BUF_SYNC_RW, DMA_BUF_SYNC_VALID_FLAGS_MASK, DMA_BUF_SYNC_WRITE};
use crate::wire::Wire;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::SpinLock;
use linux::errno::*;
use mm::device::{Aperture, DeviceMap, DeviceOps, DevicePage};
use vfs::fd::{FdTable, OpenFile};
use vfs::inode::{Cred, Meta};
use vfs::pagecache::PageFault;
use vfs::poll::{Events, PollQueue};
use vfs::sys::UserMem;
use vfs::{Errno, File};

const PAGE: u64 = 4096;

klog::counter!(DMA_BUFS, "drm.dma_bufs.exported");
klog::counter!(DMA_BUFS_FREED, "drm.dma_bufs.released");

/// `enum dma_data_direction` of a CPU access (from DMA_BUF_SYNC_READ/WRITE).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// SYNC_READ: the CPU reads what a device wrote (`DMA_FROM_DEVICE`).
    FromDevice,
    /// SYNC_WRITE: the CPU writes for a device (`DMA_TO_DEVICE`).
    ToDevice,
    Bidirectional,
}

/// The exporter of a buffer (`struct dma_buf_ops`): where its pages are and what CPU access needs.
pub trait Exporter: Send + Sync {
    /// `exp_name` in fdinfo ("udmabuf", "amdgpu").
    fn name(&self) -> &'static str;
    /// Bytes, a multiple of the page size.
    fn size(&self) -> u64;
    /// What MM maps for a program (built once at export); None: the buffer cannot be mapped (EINVAL, Linux's
    /// answer for an exporter without `.mmap`).
    fn aperture(&self) -> Option<Aperture>;
    /// WY-17: a buffer made of several pieces maps as several apertures (fbdev's windows, 2 MiB DMA buffers);
    /// `locate` says where a page lies. One piece unless an exporter says otherwise.
    fn apertures(&self) -> Vec<Aperture> {
        self.aperture().into_iter().collect()
    }
    /// WY-17: page `pgoff` of the buffer as (index in `apertures`, page in that aperture).
    fn locate(&self, pgoff: u64) -> (usize, u64) {
        (0, pgoff)
    }
    /// The exporter's own mmap rule after the size check (`ops->mmap`); `shared` = MAP_SHARED.
    fn mmap_check(&self, _shared: bool) -> Result<(), Errno> {
        Ok(())
    }
    /// `begin_cpu_access` / `end_cpu_access` (DMA_BUF_IOCTL_SYNC without/with SYNC_END).
    fn begin_cpu(&self, _dir: Direction) -> Result<(), Errno> {
        Ok(())
    }
    fn end_cpu(&self, _dir: Direction) -> Result<(), Errno> {
        Ok(())
    }
    /// The physical address of page `page` for an importing device (the sg table's entries); None outside or
    /// without frames (host tests).
    fn page_phys(&self, page: u64) -> Option<u64>;
    /// Who owns the frames `page_phys` names right now, when they can change under this object (a userptr range,
    /// which a revalidation points at new pages). None: this object itself owns them for its whole life. A device's
    /// page tables copy frames: they must read them from this and hold this, or an entry outlives its frame.
    fn frames(&self) -> Option<Arc<dyn Exporter>> {
        None
    }
    /// Kernel access (`dma_buf_vmap` for the kernel and the tests): false outside the buffer.
    fn read(&self, off: u64, out: &mut [u8]) -> bool;
    fn write(&self, off: u64, data: &[u8]) -> bool;
    /// DR-6: the amdgpu object an export came from (`dma_buf->priv` with `amdgpu_dmabuf_ops`), so a re-import on
    /// the same device finds it again.
    fn gem_bo(&self) -> Option<Arc<crate::gem::Bo>> {
        None
    }
    /// DR-6: a program's mapping faulted a page in (amdgpu remembers CPU access).
    fn cpu_fault(&self) {}
}

/// A buffer of `mm::dma_pages::DmaPages` (DR-6's GEM objects, WY-17's scanout buffers).
pub struct PagesExporter {
    pub pages: Arc<mm::dma_pages::DmaPages>,
    pub name: &'static str,
}

impl Exporter for PagesExporter {
    fn name(&self) -> &'static str {
        self.name
    }
    fn size(&self) -> u64 {
        self.pages.pages() * PAGE
    }
    fn aperture(&self) -> Option<Aperture> {
        Some(Aperture::Pages(self.pages.clone()))
    }
    fn page_phys(&self, page: u64) -> Option<u64> {
        self.pages.page_phys(page)
    }
    fn read(&self, off: u64, out: &mut [u8]) -> bool {
        self.pages.read(off, out)
    }
    fn write(&self, off: u64, data: &[u8]) -> bool {
        self.pages.write(off, data)
    }
}

// ---- registry

static FILES: SpinLock<BTreeMap<usize, Weak<DmaBuf>>> = SpinLock::named("drm.dma_buf.registry", BTreeMap::new());

fn addr<T>(t: &T) -> usize {
    t as *const T as *const () as usize
}

/// The buffer behind an open file (`is_dma_buf_file` + `file->private_data`), None for any other file.
pub fn as_dma_buf(f: &Arc<dyn File>) -> Option<Arc<DmaBuf>> {
    let a = Arc::as_ptr(f) as *const () as usize;
    let b = FILES.lock().get(&a)?.upgrade()?;
    (addr(&*b) == a).then_some(b)
}

/// Buffers alive now (ktests' leak balance).
pub fn live() -> usize {
    FILES.lock().len()
}

// ---- the file system of dma-buf inodes (`dma_buf_fs_type`)

static DEV: ksync::Once<u64> = ksync::Once::new();
/// `dmabuf_inode`: inode numbers of the dmabuf file system, never reused.
static NEXT_INO: AtomicU64 = AtomicU64::new(1);

fn dev() -> u64 {
    *DEV.get_or_init(vfs::dentry::anon_dev)
}

/// The creator's fsuid/fsgid for the inode (`alloc_anon_inode`: `current_fsuid()`); root in kernel threads.
fn creator() -> Cred {
    #[cfg(not(test))]
    if let Some(p) = proc::current() {
        return p.fs_cred();
    }
    Cred::ROOT
}

// ---- the buffer

pub struct DmaBuf {
    me: Weak<DmaBuf>,
    exp: Arc<dyn Exporter>,
    size: u64,
    resv: Arc<Resv>,
    meta: Meta,
    /// `dmabuf->name` (SET_NAME), at most 31 bytes.
    name: SpinLock<Option<String>>,
    poll: PollQueue,
    map: Option<Arc<DeviceMap>>,
    /// DR-6: Linux has one `struct file` per buffer, made at the export with its access mode: the open file of
    /// the fds (kept weakly, the file holds the buffer) and the mode of the first install.
    file: SpinLock<(Weak<OpenFile>, Option<u64>)>,
}

/// The device side of a dma-buf mapping (MM's hook): where page `pgoff` of the buffer lies says the exporter
/// (`Exporter::locate`; page `pgoff` of its one aperture for all but fbdev's windows).
pub(crate) struct MapOps {
    exp: Arc<dyn Exporter>,
    pages: u64,
}

impl MapOps {
    pub(crate) fn new(exp: Arc<dyn Exporter>, pages: u64) -> MapOps {
        MapOps { exp, pages }
    }
}

impl DeviceOps for MapOps {
    /// `dma_buf_mmap_internal`: past the buffer EINVAL, then the exporter's rule.
    fn check(&self, pgoff: u64, pages: u64, _prot: u64, shared: bool) -> Result<(), i64> {
        if pgoff.checked_add(pages).is_none_or(|end| end > self.pages) {
            return Err(EINVAL);
        }
        self.exp.mmap_check(shared)
    }
    fn fault(&self, pgoff: u64, _write: bool) -> Result<DevicePage, PageFault> {
        if pgoff >= self.pages {
            return Err(PageFault::Bus);
        }
        self.exp.cpu_fault();
        let (aperture, page) = self.exp.locate(pgoff);
        Ok(DevicePage { aperture, page, writable: true })
    }
}

impl DmaBuf {
    /// `dma_buf_export`: a new buffer of `exp`, with its own reservation object or a shared one (a GEM
    /// object's). EINVAL for an empty or unaligned size, ENOMEM when MM cannot take the pages for mappings.
    pub fn export(exp: Arc<dyn Exporter>, resv: Option<Arc<Resv>>) -> Result<Arc<DmaBuf>, Errno> {
        let size = exp.size();
        if size == 0 || size % PAGE != 0 {
            return Err(EINVAL);
        }
        let apertures = exp.apertures();
        let map = if apertures.is_empty() {
            None
        } else {
            let ops = Arc::new(MapOps::new(exp.clone(), size / PAGE));
            match DeviceMap::new(ops, apertures) {
                Ok(m) => Some(m),
                Err(e) => {
                    klog::event!(Warn, "drm", "dma_buf.no_mapping", exporter = exp.name(), error = alloc::format!("{e:?}"); "the buffer cannot be mapped by programs");
                    return Err(ENOMEM);
                }
            }
        };
        let resv = resv.unwrap_or_else(Resv::new);
        let ino = NEXT_INO.fetch_add(1, Ordering::Relaxed);
        let b = Arc::new_cyclic(|me| DmaBuf {
            me: me.clone(),
            exp,
            size,
            resv,
            meta: Meta::new(ino, 0o600, creator()),
            name: SpinLock::named("drm.dma_buf.name", None),
            poll: PollQueue::new(),
            map,
            file: SpinLock::named("drm.dma_buf.file", (Weak::new(), None)),
        });
        FILES.lock().insert(addr(&*b), Arc::downgrade(&b));
        DMA_BUFS.inc();
        let w: Weak<dyn ResvWatch> = b.me.clone();
        b.resv.watch(w)?;
        klog::event!(Debug, "drm", "dma_buf.exported", exporter = b.exp.name(), size = size, ino = ino);
        Ok(b)
    }

    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn exporter(&self) -> &Arc<dyn Exporter> {
        &self.exp
    }
    pub fn resv(&self) -> &Arc<Resv> {
        &self.resv
    }
    pub fn ino(&self) -> u64 {
        self.meta.ino
    }
    /// What MM maps for `mmap` of the fd (the desk wires `mm::device::lookup` to it, see plan/notes/dr-DR-5.md).
    pub fn device_map(&self) -> Option<&Arc<DeviceMap>> {
        self.map.as_ref()
    }
    pub fn name(&self) -> Option<String> {
        self.name.lock().clone()
    }

    /// `dma_buf_fd`: installs the buffer as an fd open for reading and writing (Linux's exporters pass O_RDWR),
    /// close-on-exec on request. EMFILE without a table.
    pub fn install(self: &Arc<Self>, fds: Option<&FdTable>, cloexec: bool) -> Result<i32, Errno> {
        self.install_mode(fds, cloexec, true)
    }

    /// `dma_buf_fd` of the buffer's one open file: the first install makes it, read-only unless `rdwr`
    /// (PRIME's DRM_RDWR); later installs while it is open share it (its mode and status flags), as Linux's
    /// fds of one dma-buf do.
    pub fn install_mode(self: &Arc<Self>, fds: Option<&FdTable>, cloexec: bool, rdwr: bool) -> Result<i32, Errno> {
        let fds = fds.ok_or(EMFILE)?;
        let of = {
            let mut g = self.file.lock();
            match g.0.upgrade() {
                Some(of) => of,
                None => {
                    let mode = *g.1.get_or_insert(if rdwr { vfs::abi::O_RDWR } else { vfs::abi::O_RDONLY });
                    let of = OpenFile::anon(self.clone(), mode);
                    g.0 = Arc::downgrade(&of);
                    of
                }
            }
        };
        let n = fds.install_flags(of, 0, cloexec)?;
        i32::try_from(n).map_err(|_| EMFILE)
    }

    /// The ioctls with an explicit fd table (host tests); `ioctl_user` uses the caller's.
    pub fn ioctl_with(&self, cmd: u32, arg: u64, mem: &dyn UserMem, fds: Option<&FdTable>) -> Result<i64, Errno> {
        match cmd {
            DMA_BUF_IOCTL_SYNC => self.sync(arg, mem),
            DMA_BUF_SET_NAME_A | DMA_BUF_SET_NAME_B => self.set_name(arg, mem),
            DMA_BUF_IOCTL_EXPORT_SYNC_FILE => self.export_sync_file(arg, mem, fds),
            DMA_BUF_IOCTL_IMPORT_SYNC_FILE => self.import_sync_file(arg, mem, fds),
            _ => Err(ENOTTY),
        }
    }

    /// DMA_BUF_IOCTL_SYNC (dma-buf.c:452-476).
    fn sync(&self, arg: u64, mem: &dyn UserMem) -> Result<i64, Errno> {
        let mut b = [0u8; dma_buf_sync::SIZE];
        if !mem.read(arg, &mut b) {
            return Err(EFAULT);
        }
        let s = dma_buf_sync::decode(&b).ok_or(EFAULT)?;
        if s.flags & !(DMA_BUF_SYNC_VALID_FLAGS_MASK as u64) != 0 {
            return Err(EINVAL);
        }
        let dir = match s.flags as u32 & DMA_BUF_SYNC_RW {
            1 => Direction::FromDevice,
            2 => Direction::ToDevice,
            3 => Direction::Bidirectional,
            _ => return Err(EINVAL),
        };
        if s.flags as u32 & DMA_BUF_SYNC_END != 0 { self.exp.end_cpu(dir) } else { self.begin_cpu(dir, mem) }.map(|_| 0)
    }

    /// `dma_buf_begin_cpu_access`: waits (interruptibly in Linux) for the fences the access must see, then the
    /// exporter. The wait is bounded by the fences themselves; V2's buffers have none that never signal (an
    /// unplugged device force-completes its fences, DR-3).
    fn begin_cpu(&self, dir: Direction, mem: &dyn UserMem) -> Result<(), Errno> {
        let write = dir != Direction::FromDevice;
        for f in self.resv.fences(Usage::rw(write)) {
            wait(&f, mem)?;
        }
        self.exp.begin_cpu(dir)
    }

    /// `dma_buf_set_name` (dma-buf.c:320): `strndup_user(buf, DMA_BUF_NAME_LEN)`: EFAULT, EINVAL without a NUL in
    /// 32 bytes.
    fn set_name(&self, arg: u64, mem: &dyn UserMem) -> Result<i64, Errno> {
        let s = mem.read_cstr(arg, DMA_BUF_NAME_LEN as usize).map_err(|e| if e == ENAMETOOLONG { EINVAL } else { e })?;
        let name = String::from_utf8_lossy(&s).into_owned();
        klog::event!(Debug, "drm", "dma_buf.named", ino = self.meta.ino, name = name.as_str());
        *self.name.lock() = Some(name);
        Ok(0)
    }

    /// `dma_buf_export_sync_file` (dma-buf.c:336): the fences an access of the given kind waits for as one
    /// sync_file (the signaled stub without any); fd reserved, copied back, then installed.
    fn export_sync_file(&self, arg: u64, mem: &dyn UserMem, fds: Option<&FdTable>) -> Result<i64, Errno> {
        let mut b = [0u8; dma_buf_export_sync_file::SIZE];
        if !mem.read(arg, &mut b) {
            return Err(EFAULT);
        }
        let mut a = dma_buf_export_sync_file::decode(&b).ok_or(EFAULT)?;
        if a.flags & !DMA_BUF_SYNC_RW != 0 || a.flags & DMA_BUF_SYNC_RW == 0 {
            return Err(EINVAL);
        }
        let t = fds.ok_or(EMFILE)?;
        let n = t.reserve(0, true)?;
        let fd = match i32::try_from(n) {
            Ok(fd) => fd,
            Err(_) => {
                t.unreserve(n);
                return Err(EMFILE);
            }
        };
        let f = self.resv.singleton(Usage::rw(a.flags & DMA_BUF_SYNC_WRITE != 0)).unwrap_or_else(fence::stub);
        let sf = SyncFile::new(f, b"");
        a.fd = fd;
        if a.encode(&mut b).is_none() || !mem.write(arg, &b) {
            t.unreserve(n);
            return Err(EFAULT);
        }
        t.commit(n, OpenFile::anon(sf, vfs::abi::O_RDONLY));
        Ok(0)
    }

    /// `dma_buf_import_sync_file` (dma-buf.c:392): the sync_file's fences join the set as writers (SYNC_WRITE)
    /// or readers. EINVAL for an fd that is no sync_file.
    fn import_sync_file(&self, arg: u64, mem: &dyn UserMem, fds: Option<&FdTable>) -> Result<i64, Errno> {
        let mut b = [0u8; dma_buf_import_sync_file::SIZE];
        if !mem.read(arg, &mut b) {
            return Err(EFAULT);
        }
        let a = dma_buf_import_sync_file::decode(&b).ok_or(EFAULT)?;
        if a.flags & !DMA_BUF_SYNC_RW != 0 || a.flags & DMA_BUF_SYNC_RW == 0 {
            return Err(EINVAL);
        }
        let sf = sync_file::lookup(fds, a.fd).ok().and_then(|f| sync_file::as_sync_file(&f)).ok_or(EINVAL)?;
        let usage = if a.flags & DMA_BUF_SYNC_WRITE != 0 { Usage::Write } else { Usage::Read };
        self.resv.add_fence(sf.fence(), usage)?;
        Ok(0)
    }

    /// `dma_buf_poll`'s mask, computed now.
    fn ready(&self) -> u32 {
        let mut ev = 0;
        if self.resv.test_signaled(Usage::Write) {
            ev |= Events::IN;
        }
        if self.resv.test_signaled(Usage::Read) {
            ev |= Events::OUT;
        }
        ev
    }
}

/// One fence of a CPU access (`dma_resv_wait_timeout(.., intr = true, MAX_SCHEDULE_TIMEOUT)`): a kill or a
/// signal ends it with EINTR (Linux's ERESTARTSYS).
fn wait(f: &Arc<fence::Fence>, mem: &dyn UserMem) -> Result<(), Errno> {
    if fence::is_signaled(f) {
        return Ok(());
    }
    let w = fence::Waiter::new();
    let _reg = fence::add_callback(f, w.clone());
    // Naps of 10 ms between the checks; a hung device's fences are force-completed by its reset or unplug
    // (DR-3), so the fence itself bounds the wait.
    while !fence::is_signaled(f) {
        if mem.interrupted() || mem.signal_pending() {
            return Err(EINTR);
        }
        w.sleep(10_000_000);
    }
    Ok(())
}

impl ResvWatch for DmaBuf {
    /// A fence of the set signaled: wake the pollers with what is ready now (state changed before, FS-17).
    fn changed(&self) {
        let ev = self.ready();
        if ev != 0 {
            self.poll.wake_poll(ev);
        }
    }
}

impl Drop for DmaBuf {
    fn drop(&mut self) {
        FILES.lock().remove(&addr(self));
        DMA_BUFS_FREED.inc();
        klog::event!(Debug, "drm", "dma_buf.released", exporter = self.exp.name(), ino = self.meta.ino);
    }
}

impl File for DmaBuf {
    /// No `.read` in `dma_buf_fops`: EINVAL.
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn size(&self) -> u64 {
        self.size
    }
    /// `alloc_anon_inode`: S_IRUSR | S_IWUSR, no type bits.
    fn mode(&self) -> u32 {
        0o600
    }
    /// `dma_buf_getfile`: i_size and the blocks are the buffer's size, the inode number the buffer's own.
    fn attr(&self) -> Option<vfs::abi::Attr> {
        let mut a = self.meta.attr(dev());
        a.size = self.size;
        a.blocks = self.size / 512;
        Some(a)
    }
    fn meta(&self) -> Option<&Meta> {
        Some(&self.meta)
    }
    /// `dma_buf_llseek`.
    fn lseek(&self, off: i64, whence: u32) -> Option<Result<u64, Errno>> {
        let base = match whence {
            vfs::abi::SEEK_END => self.size,
            vfs::abi::SEEK_SET => 0,
            _ => return Some(Err(EINVAL)),
        };
        Some(if off != 0 { Err(EINVAL) } else { Ok(base) })
    }
    fn events(&self) -> Events {
        Events(self.ready())
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        Some(&self.poll)
    }
    fn can_poll(&self) -> bool {
        true
    }
    /// EPOLLIN/EPOLLOUT only, without the RDNORM/WRNORM a generic mask adds.
    fn poll_exact(&self) -> bool {
        true
    }
    /// `dmabuffs_dname`: "/dmabuf:" and the name.
    fn link_name(&self) -> Option<String> {
        let mut s = String::from("/dmabuf:");
        if let Some(n) = self.name.lock().as_deref() {
            s.push_str(n);
        }
        Some(s)
    }
    /// MM maps the exporter's pages (CR-DR5-1).
    fn device_map(&self) -> Option<Arc<dyn core::any::Any + Send + Sync>> {
        self.map.clone().map(|m| m as Arc<dyn core::any::Any + Send + Sync>)
    }
    /// `dma_buf_show_fdinfo`. `count` is the number of open files of the buffer (Linux counts the file's
    /// references: its fds and mappings; V2's VFS does not hand the count of one open file to the file).
    fn show_fdinfo(&self, out: &mut String) {
        let count = self.me.strong_count();
        let _ = write!(out, "size:\t{}\ncount:\t{}\nexp_name:\t{}\n", self.size, count, self.exp.name());
        if let Some(n) = self.name.lock().as_deref() {
            let _ = writeln!(out, "name:\t{n}");
        }
    }
    /// Linux takes `unsigned int cmd`.
    fn ioctl_user(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Option<Result<i64, Errno>> {
        let fds = sync_file::current_fds();
        Some(self.ioctl_with(cmd as u32, arg, mem, fds.as_deref()))
    }
}

/// Kernel helper for tests and importers: a buffer's bytes as a vector (`dma_buf_vmap` + copy).
pub fn read_all(b: &DmaBuf) -> Option<Vec<u8>> {
    let mut v = Vec::new();
    v.try_reserve_exact(b.size as usize).ok()?;
    v.resize(b.size as usize, 0);
    b.exp.read(0, &mut v).then_some(v)
}
