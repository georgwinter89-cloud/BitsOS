//! DR-5: `/dev/udmabuf` (linux-6.6/drivers/dma-buf/udmabuf.c): turns pages of sealed memfds into a dma-buf, so a
//! program without a GPU buffer (lavapipe, a Wayland client, the tests) can hand memory to the compositor as a
//! dma-buf. The buffer holds the memfd's own page-cache pages: what is written through the memfd is seen through
//! the dma-buf and back, also after the memfd is closed (`shmem_read_mapping_page` + `get_page`).
//!
//! The node is a misc device (char 10:258, the minor a 6.x kernel hands out dynamically) with mode 0666: a
//! buffer only references pages the caller already owns through its memfd, so it gives nobody more than they
//! have (CachyOS opens it to the seat user the same way, with an ACL).
//!
//! Limits are Linux's module parameters as knobs: `list_limit` (items of one CREATE_LIST, default 1024 as
//! Linux; an ABI rule, EINVAL beyond) and `size_limit_mb` (default: none, R1; the pages are already charged to
//! their memfd, the buffer adds only its page list). The page lists are kernel heap an unprivileged caller
//! controls (items may overlap, so one call can name the same pages 1024 times), so all live udmabufs together
//! may hold at most `ref_limit` page references (default: four per page of managed RAM, which keeps the lists
//! under 1 % of RAM; a knob, R1); beyond that a create answers ENOMEM.

use crate::dma_buf::{DmaBuf, Direction, Exporter};
use crate::sync_file;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use linux::errno::*;
use mm::device::Aperture;
use vfs::fd::FdTable;
use vfs::pagecache::{CachePage, PageFault};
use vfs::sys::UserMem;
use vfs::{Errno, File};

/// `EBADFD` ("file descriptor in bad state"; not in `uapi::errno`): udmabuf's answer for an fd that is no memfd.
pub const EBADFD: Errno = 77;

pub const UDMABUF_FLAGS_CLOEXEC: u32 = 0x01;
/// `_IOW('u', 0x42, struct udmabuf_create)`.
pub const UDMABUF_CREATE: u32 = 0x4018_7542;
/// `_IOW('u', 0x43, struct udmabuf_create_list)` (the header only: 8 bytes).
pub const UDMABUF_CREATE_LIST: u32 = 0x4008_7543;
/// `struct udmabuf_create` and `struct udmabuf_create_item`: 24 bytes each.
const ITEM_SIZE: usize = 24;
const PAGE: u64 = 4096;
const F_SEAL_SHRINK: u32 = vfs::memfd::F_SEAL_SHRINK;
const F_SEAL_WRITE: u32 = vfs::memfd::F_SEAL_WRITE;

/// `misc` major and the minor of the node.
pub const MAJOR: u32 = 10;
pub const MINOR: u32 = 258;

/// Linux's module parameters `list_limit` and `size_limit_mb` (0: no size limit), fixed (C1 removed the setters:
/// no CONFIG or settings key ever reached them).
const LIST_LIMIT: u64 = 1024;
const SIZE_LIMIT_MB: u64 = 0;
/// Page references the live udmabufs hold.
static REFS: AtomicU64 = AtomicU64::new(0);
/// References per page of managed RAM the default `ref_limit` allows.
const REFS_PER_PAGE: u64 = 4;

#[cfg(test)]
std::thread_local! {
    /// A host test's own `ref_limit` (0: none): host tests run in parallel and share the static one.
    static TEST_REF_LIMIT: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

klog::counter!(UDMABUFS, "drm.udmabuf.created");
klog::counter!(UDMABUF_REFUSED, "drm.udmabuf.refused");

/// The `ref_limit` in force: page references all live udmabufs may hold together, four per page of managed RAM
/// (u64::MAX without a frame allocator: host tests).
pub fn ref_limit() -> u64 {
    #[cfg(test)]
    {
        let t = TEST_REF_LIMIT.with(|c| c.get());
        if t != 0 {
            return t;
        }
    }
    match mm_core::frames().map(|f| f.stats().total_bytes / PAGE).unwrap_or(0) {
        0 => u64::MAX,
        n => n.saturating_mul(REFS_PER_PAGE),
    }
}

/// Page references the live udmabufs hold now.
pub fn refs_held() -> u64 {
    REFS.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn set_test_ref_limit(pages: u64) {
    TEST_REF_LIMIT.with(|c| c.set(pages));
}

/// Adds `n` to `held` unless that passes `limit`; the count held before on refusal.
pub(crate) fn reserve(held: &AtomicU64, n: u64, limit: u64) -> Result<(), u64> {
    let mut cur = held.load(Ordering::Relaxed);
    loop {
        let Some(next) = cur.checked_add(n).filter(|&x| x <= limit) else { return Err(cur) };
        match held.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => return Ok(()),
            Err(c) => cur = c,
        }
    }
}

/// `n` page references of the budget, given back on drop.
struct Charge(u64);

impl Charge {
    fn take(n: u64) -> Result<Charge, Errno> {
        let limit = ref_limit();
        match reserve(&REFS, n, limit) {
            Ok(()) => Ok(Charge(n)),
            Err(held) => {
                klog::event!(Warn, "drm", "udmabuf.ref_limit_hit", pages = n, held = held, limit = limit; "udmabuf page lists over ref_limit: ENOMEM");
                Err(ENOMEM)
            }
        }
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        REFS.fetch_sub(self.0, Ordering::AcqRel);
    }
}

/// One item of a create call (`struct udmabuf_create_item`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item {
    pub memfd: u32,
    pub offset: u64,
    pub size: u64,
}

impl Item {
    fn decode(b: &[u8]) -> Item {
        let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u64_at = |o: usize| u64::from(u32_at(o)) | (u64::from(u32_at(o + 4)) << 32);
        Item { memfd: u32_at(0), offset: u64_at(8), size: u64_at(16) }
    }
}

/// The pages of a udmabuf: page-cache pages of memfds (tmpfs pages are never reclaimed or moved, and the
/// references keep their frames when the memfd shrinks by hole punching or goes away).
pub struct ShmemPages {
    /// Shared so a mapping can hold the pages themselves (CR-DR5-2: MM maps only frames whose owner it holds).
    pages: Arc<Vec<Arc<CachePage>>>,
    /// The list's share of `ref_limit`; lives as long as the list (a mapping keeps the exporter alive).
    _charge: Charge,
}

impl ShmemPages {
    pub fn pages(&self) -> u64 {
        self.pages.len() as u64
    }
    pub fn page_phys(&self, page: u64) -> Option<u64> {
        self.pages.get(usize::try_from(page).ok()?)?.phys()
    }
    pub fn page(&self, page: u64) -> Option<&Arc<CachePage>> {
        self.pages.get(usize::try_from(page).ok()?)
    }
    /// The pages as one shared list (the mapping's aperture).
    pub fn shared(&self) -> Arc<Vec<Arc<CachePage>>> {
        self.pages.clone()
    }
}

/// The exporter (`struct udmabuf`).
pub struct Udmabuf {
    pages: Arc<ShmemPages>,
    /// `ubuf->sg`: made by the first begin of CPU access; an end before that is EINVAL (end_cpu_udmabuf).
    sg: AtomicBool,
}

impl Udmabuf {
    pub fn pages(&self) -> &Arc<ShmemPages> {
        &self.pages
    }

    /// Copies between the buffer and `buf` page by page; false past the end.
    fn each(&self, off: u64, len: usize, mut f: impl FnMut(&CachePage, usize, core::ops::Range<usize>)) -> bool {
        let Some(end) = off.checked_add(len as u64) else { return false };
        if end > self.pages.pages() * PAGE {
            return false;
        }
        let mut pos = off;
        let mut done = 0usize;
        while done < len {
            let in_page = (pos % PAGE) as usize;
            let n = (PAGE as usize - in_page).min(len - done);
            match self.pages.page(pos / PAGE) {
                Some(p) => f(p, in_page, done..done + n),
                None => return false,
            }
            pos += n as u64;
            done += n;
        }
        true
    }
}

impl Exporter for Udmabuf {
    fn name(&self) -> &'static str {
        "udmabuf"
    }
    fn size(&self) -> u64 {
        self.pages.pages() * PAGE
    }
    /// The memfd's frames as they are (CR-DR5-2).
    fn aperture(&self) -> Option<Aperture> {
        Some(Aperture::Frames(self.pages.shared()))
    }
    /// `mmap_udmabuf`: MAP_SHARED only.
    fn mmap_check(&self, shared: bool) -> Result<(), Errno> {
        if shared { Ok(()) } else { Err(EINVAL) }
    }
    /// `begin_cpu_udmabuf`: the first call makes the sg table; x86 is coherent, so there is nothing to flush.
    fn begin_cpu(&self, _dir: Direction) -> Result<(), Errno> {
        self.sg.store(true, Ordering::Release);
        Ok(())
    }
    fn end_cpu(&self, _dir: Direction) -> Result<(), Errno> {
        if self.sg.load(Ordering::Acquire) { Ok(()) } else { Err(EINVAL) }
    }
    fn page_phys(&self, page: u64) -> Option<u64> {
        self.pages.page_phys(page)
    }
    fn read(&self, off: u64, out: &mut [u8]) -> bool {
        self.each(off, out.len(), |p, at, r| p.read(at, &mut out[r]))
    }
    fn write(&self, off: u64, data: &[u8]) -> bool {
        self.each(off, data.len(), |p, at, r| p.write(at, &data[r]))
    }
}

/// `udmabuf_create` (udmabuf.c:197-292) for the items of one call: the size checks over all items first, then
/// each memfd (EBADFD: no open file or not shmem; EINVAL: F_SEAL_SHRINK missing or F_SEAL_WRITE set, a page past
/// the memfd's end), then the buffer and its fd (O_RDWR, close-on-exec with UDMABUF_FLAGS_CLOEXEC; other flag
/// bits are ignored as in Linux 6.6).
pub fn create(fds: Option<&FdTable>, flags: u32, items: &[Item]) -> Result<i32, Errno> {
    let r = build(fds, items).and_then(|b| b.install(fds, flags & UDMABUF_FLAGS_CLOEXEC != 0));
    match r {
        Ok(_) => UDMABUFS.inc(),
        Err(e) => {
            UDMABUF_REFUSED.inc();
            klog::event!(Debug, "drm", "udmabuf.refused", items = items.len(), errno = e);
        }
    }
    r
}

/// The buffer of `items` without an fd (kernel users and tests).
pub fn build(fds: Option<&FdTable>, items: &[Item]) -> Result<Arc<DmaBuf>, Errno> {
    let pglimit = if SIZE_LIMIT_MB == 0 { u64::MAX } else { SIZE_LIMIT_MB.saturating_mul(1 << 20) / PAGE };
    let mut count: u64 = 0;
    for it in items {
        if it.offset % PAGE != 0 || it.size % PAGE != 0 {
            return Err(EINVAL);
        }
        count = count.saturating_add(it.size / PAGE);
        if count > pglimit {
            return Err(EINVAL);
        }
    }
    if count == 0 {
        return Err(EINVAL);
    }
    // Overlapping items count once per reference: the list holds one entry per reference.
    let charge = Charge::take(count)?;
    // The page list grows item by item after each range was checked against its memfd, so a huge size cannot
    // make the kernel reserve memory for pages that do not exist (Linux allocates the whole array first, bounded
    // by its 64 MiB default).
    let mut pages: Vec<Arc<CachePage>> = Vec::new();
    for it in items {
        let f = fds.ok_or(EBADFD)?.get(it.memfd as usize).map_err(|_| EBADFD)?;
        let Some(cache) = f.inode().and_then(|i| i.page_cache()) else { return Err(EBADFD) };
        // A tmpfs/memfd page cache is the one with seals (`shmem_mapping`); every other file is EBADFD.
        let Some(seals) = cache.seals() else { return Err(EBADFD) };
        if seals & F_SEAL_SHRINK == 0 || seals & F_SEAL_WRITE != 0 {
            return Err(EINVAL);
        }
        // Past the end of the file: `shmem_read_mapping_page`'s EINVAL.
        if it.offset.checked_add(it.size).is_none_or(|end| end > cache.size().div_ceil(PAGE) * PAGE) {
            return Err(EINVAL);
        }
        pages.try_reserve(usize::try_from(it.size / PAGE).map_err(|_| ENOMEM)?).map_err(|_| ENOMEM)?;
        let first = it.offset / PAGE;
        for k in 0..it.size / PAGE {
            let pg = first.checked_add(k).ok_or(EINVAL)?;
            let p = cache.page_for_map(pg, false).map_err(|e| match e {
                PageFault::NoMemory => ENOMEM,
                // Past the end of the file (`shmem_get_folio_gfp`'s EINVAL) or no room in the tmpfs pool.
                PageFault::Bus => EINVAL,
                _ => EIO,
            })?;
            pages.push(p);
        }
    }
    let exp = Arc::new(Udmabuf { pages: Arc::new(ShmemPages { pages: Arc::new(pages), _charge: charge }), sg: AtomicBool::new(false) });
    DmaBuf::export(exp, None)
}

/// The ioctls of the node with an explicit fd table (host tests).
pub fn ioctl_with(cmd: u32, arg: u64, mem: &dyn UserMem, fds: Option<&FdTable>) -> Result<i64, Errno> {
    match cmd {
        UDMABUF_CREATE => {
            let mut b = [0u8; ITEM_SIZE];
            if !mem.read(arg, &mut b) {
                return Err(EFAULT);
            }
            // `struct udmabuf_create`: memfd, flags, offset, size (flags where an item has its padding).
            let it = Item::decode(&b);
            let flags = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
            create(fds, flags, &[it]).map(i64::from)
        }
        UDMABUF_CREATE_LIST => {
            let mut h = [0u8; 8];
            if !mem.read(arg, &mut h) {
                return Err(EFAULT);
            }
            let flags = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
            let count = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
            if u64::from(count) > LIST_LIMIT {
                return Err(EINVAL);
            }
            let len = (count as usize).checked_mul(ITEM_SIZE).ok_or(ENOMEM)?;
            let mut raw: Vec<u8> = Vec::new();
            raw.try_reserve_exact(len).map_err(|_| ENOMEM)?;
            raw.resize(len, 0);
            if len != 0 && !mem.read(arg.wrapping_add(8), &mut raw) {
                return Err(EFAULT);
            }
            let mut items: Vec<Item> = Vec::new();
            items.try_reserve_exact(count as usize).map_err(|_| ENOMEM)?;
            items.extend(raw.chunks_exact(ITEM_SIZE).map(Item::decode));
            create(fds, flags, &items).map(i64::from)
        }
        _ => Err(ENOTTY),
    }
}

/// The node `/dev/udmabuf` (`udmabuf_misc`): no read, write or poll, only the two ioctls.
pub struct Node;

impl File for Node {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn mode(&self) -> u32 {
        linux::S_IFCHR | 0o666
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(MAJOR, MINOR)
    }
    /// `udmabuf_fops` has no `.poll`: epoll refuses it, poll sees the default mask.
    fn can_poll(&self) -> bool {
        false
    }
    fn ioctl_user(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Option<Result<i64, Errno>> {
        let fds = sync_file::current_fds();
        Some(ioctl_with(cmd as u32, arg, mem, fds.as_deref()))
    }
}

fn make() -> Arc<dyn File> {
    Arc::new(Node)
}
vfs::devnode!("/dev/udmabuf", make);
