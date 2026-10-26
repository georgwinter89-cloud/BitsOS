//! Device-backed mappings (MM-5b, GX-D7): a program maps a device file (`/dev/fb0` first, DP-2; later GPU
//! surfaces, GX) and its pages come from the device's fault hook. MM never frees or copies such a page:
//! unmapping, a fork, `munmap` or the end of the program only drop the translation.
//!
//! Apertures (the trust boundary): a device declares the memory it may hand out once, in `DeviceMap::new`, and
//! its fault hook answers an aperture index and a page inside it, never a physical address. MM checks each
//! aperture once and owns what keeps it alive, so safe driver code can neither hand a program a kernel frame
//! (heap, page table, page cache) nor map RAM with another memory type than the direct map's (SDM 11.12.4; the
//! `DmaRegion::new` contract):
//! - `Aperture::Ram(DmaBuffer)`: a DMA buffer (WB for a shadow buffer, or UC/WC: MM-4 retyped its direct-map
//!   alias already). Its pages are mapped with the buffer's type; the map keeps the buffer, so its frames stay
//!   allocated while any mapping or VMA of the device exists.
//! - `Aperture::Mmio`: device memory outside RAM (a framebuffer or VRAM BAR, registers), page-aligned, with the
//!   type of every kernel window over it (`vmap::check_aperture`; Linux `reserve_pfn_range` refuses the same).
//!
//! An answer outside the declared apertures ends the program with SIGBUS and an Error event `device.bad_page`.
//!
//! Dirty tracking (Linux `fb_deferred_io`): a device answers a read fault with `writable: false`, so the first
//! store to the page faults again with `write = true`; the device marks the page dirty and answers writable.
//! After it copied the dirty pages out it calls `DeviceMap::write_protect` for them, which makes every mapping of
//! them read-only again (with the TLB shootdown), so the next store is seen again. Order on the device side:
//! clear the dirty mark, `write_protect`, then copy (a store after the protect faults and re-dirties the page).
//!
//! The VMA is a file VMA (`VmaKind::File`, `FileRef::device`), so `/proc/<pid>/maps` names the device file;
//! device VMAs are never charged to the commit, never shared copy-on-write, and `madvise(DONTNEED)` on them is
//! EINVAL (Linux `VM_PFNMAP`). Growing one with `mremap` is EFAULT (Linux `VM_DONTEXPAND`); moving is fine.
//! MAP_PRIVATE behaves as MAP_SHARED (no copy-on-write of device pages), so it needs the same rights: PROT_WRITE
//! (now or by a later mprotect) only on an fd open for writing.
//!
//! Locks: `fault` runs under the address space's lock ("mm.as"); `write_protect` takes each mapping space's lock
//! in turn and must be called without holding any lock `fault` takes (mm.as > the device's own locks).

use crate::aspace::AsCore;
use crate::dma::DmaBuffer;
use crate::vmap::{self, ApertureRefusal};
use crate::PAGE;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use hal::paging::CacheType;
use ksync::SpinLock;
use vfs::mmap::{FileMap, MapBacking};
use vfs::pagecache::PageFault;
use vfs::OpenFile;

/// Memory a device may hand to programs (see the module docs).
pub enum Aperture {
    /// Device memory outside RAM: `phys` and `len` page-aligned, `cache` the type programs get.
    Mmio { phys: u64, len: u64, cache: CacheType },
    /// A DMA buffer; programs get its pages with the buffer's own type. The map keeps it alive.
    Ram(Arc<DmaBuffer>),
}

/// Why `DeviceMap::new` refused the apertures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApertureError {
    /// No aperture, or one of length 0.
    Empty,
    /// An MMIO aperture not page-aligned, or past the physical address space.
    NotAligned,
    /// An MMIO aperture over RAM (use a DMA buffer for RAM).
    OverlapsRam,
    /// A kernel window maps part of an MMIO aperture with another memory type.
    TypeConflict,
    /// Before MM's device mapper is up.
    NotReady,
    NoMemory,
}

/// One checked aperture.
struct Ap {
    phys: u64,
    pages: u64,
    cache: CacheType,
    /// Keeps a RAM aperture's frames allocated while the map lives.
    _ram: Option<Arc<DmaBuffer>>,
}

/// What a device's fault hook answers: a page of one of its apertures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevicePage {
    /// Index into the apertures given to `DeviceMap::new`.
    pub aperture: usize,
    /// Page inside that aperture.
    pub page: u64,
    /// May the program write it now (only if the mapping allows writes too)? False keeps the page read-only so
    /// the next store faults with `write = true` (dirty tracking).
    pub writable: bool,
}

/// A device answer checked against the apertures: what MM maps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub(crate) phys: u64,
    pub(crate) cache: CacheType,
    pub(crate) writable: bool,
}

/// The device side of a mapping (implemented by the driver, e.g. DP-2's fbdev).
pub trait DeviceOps: Send + Sync {
    /// Asked once per `mmap` before the mapping exists: `pgoff` = offset / 4096, `pages` = length, `prot` the
    /// PROT_* bits, `shared` = MAP_SHARED. Refuse with a positive errno (Linux's `f_op->mmap` answers, e.g.
    /// EINVAL for a range past the device memory).
    fn check(&self, _pgoff: u64, _pages: u64, _prot: u64, _shared: bool) -> Result<(), i64> {
        Ok(())
    }
    /// The page behind `pgoff` for an access; `write`: a store (the device marks it dirty). `Bus` ends the
    /// program with SIGBUS (outside the device memory), `NoMemory` with the out-of-memory message.
    fn fault(&self, pgoff: u64, write: bool) -> Result<DevicePage, PageFault>;
}

/// One mappable device: its hook and the address spaces that map it (for `write_protect`).
pub struct DeviceMap {
    ops: Arc<dyn DeviceOps>,
    apertures: Vec<Ap>,
    spaces: SpinLock<Vec<Weak<AsCore>>>,
}

klog::counter!(DEVICE_FAULTS, "mm.device.faults");
klog::counter!(DEVICE_PROTECTED, "mm.device.protected_pages");
klog::counter!(DEVICE_BAD_PAGES, "mm.device.bad_pages");

/// The highest physical address an MMIO aperture may reach (x86-64's 52-bit limit).
const PHYS_LIMIT: u64 = 1 << 52;

fn check_one(a: Aperture) -> Result<Ap, ApertureError> {
    match a {
        Aperture::Ram(b) => {
            let pages = (b.len() as u64).div_ceil(PAGE);
            if pages == 0 {
                return Err(ApertureError::Empty);
            }
            Ok(Ap { phys: b.phys(), pages, cache: b.cpu().cache(), _ram: Some(b) })
        }
        Aperture::Mmio { phys, len, cache } => {
            if len == 0 {
                return Err(ApertureError::Empty);
            }
            let end = phys.checked_add(len).filter(|&e| e <= PHYS_LIMIT).ok_or(ApertureError::NotAligned)?;
            if phys % PAGE != 0 || len % PAGE != 0 {
                return Err(ApertureError::NotAligned);
            }
            vmap::check_aperture(phys, end, cache).map_err(|r| match r {
                ApertureRefusal::NotReady => ApertureError::NotReady,
                ApertureRefusal::Ram => ApertureError::OverlapsRam,
                ApertureRefusal::TypeConflict(_) => ApertureError::TypeConflict,
            })?;
            Ok(Ap { phys, pages: len / PAGE, cache, _ram: None })
        }
    }
}

impl DeviceMap {
    /// A mappable device with its hook and the memory it may hand out (checked here, see the module docs).
    pub fn new(ops: Arc<dyn DeviceOps>, apertures: Vec<Aperture>) -> Result<Arc<DeviceMap>, ApertureError> {
        if apertures.is_empty() {
            return Err(ApertureError::Empty);
        }
        let mut aps = Vec::new();
        aps.try_reserve_exact(apertures.len()).map_err(|_| ApertureError::NoMemory)?;
        for (i, a) in apertures.into_iter().enumerate() {
            match check_one(a) {
                Ok(ap) => aps.push(ap),
                Err(e) => {
                    klog::event!(Warn, "mm", "device.aperture_refused", index = i, error = alloc::format!("{e:?}");
                        "device memory refused for program mappings");
                    return Err(e);
                }
            }
        }
        Ok(Arc::new(DeviceMap { ops, apertures: aps, spaces: SpinLock::named("mm.device.spaces", Vec::new()) }))
    }

    pub(crate) fn ops(&self) -> &dyn DeviceOps {
        &*self.ops
    }

    /// The device's answer for `pgoff`, checked against its apertures: anything outside them is `Bus`.
    pub(crate) fn fault(&self, pgoff: u64, write: bool) -> Result<Resolved, PageFault> {
        DEVICE_FAULTS.inc();
        let pg = self.ops.fault(pgoff, write)?;
        match self.apertures.get(pg.aperture).filter(|a| pg.page < a.pages) {
            Some(a) => Ok(Resolved { phys: a.phys + pg.page * PAGE, cache: a.cache, writable: pg.writable }),
            None => {
                DEVICE_BAD_PAGES.inc();
                klog::event!(Error, "mm", "device.bad_page", pgoff = pgoff, aperture = pg.aperture, page = pg.page,
                    apertures = self.apertures.len(); "a device answered a page outside its apertures (driver bug): SIGBUS");
                Err(PageFault::Bus)
            }
        }
    }

    /// Remembers that `space` maps this device (once; dead entries are dropped on the way). False: no room.
    pub(crate) fn add_space(&self, space: &Weak<AsCore>) -> bool {
        let mut g = self.spaces.lock();
        g.retain(|w| w.strong_count() > 0);
        if g.iter().any(|w| w.ptr_eq(space)) {
            return true;
        }
        if g.try_reserve(1).is_err() {
            return false;
        }
        g.push(space.clone());
        true
    }

    /// Address spaces mapping this device now.
    pub fn mapped_by(&self) -> usize {
        self.spaces.lock().iter().filter(|w| w.strong_count() > 0).count()
    }

    /// Makes device pages `first..=last` read-only in every address space that maps them (with the TLB
    /// shootdown), so the next store to each faults with `write = true`. Returns how many page entries changed.
    /// Call it without holding a lock the fault hook takes.
    pub fn write_protect(&self, first: u64, last: u64) -> u64 {
        let mut n = 0;
        let mut i = 0;
        loop {
            // One entry at a time: no allocation, and no space lock taken under the spin lock.
            let w = {
                let g = self.spaces.lock();
                match g.get(i) {
                    Some(w) => w.clone(),
                    None => break,
                }
            };
            i += 1;
            if let Some(s) = w.upgrade() {
                n += s.device_write_protect(self, first, last);
            }
        }
        DEVICE_PROTECTED.add(n);
        n
    }
}

/// The `MapBacking` of one device mapping: the device's frames are never handed back or freed, the name is the
/// device file's (for `/proc/<pid>/maps`).
pub(crate) struct DevBacking {
    pub(crate) map: Arc<DeviceMap>,
    name: Option<Arc<FileMap>>,
}

impl MapBacking for DevBacking {
    fn get_page(&self, pgoff: u64, write: bool) -> Result<u64, PageFault> {
        self.map.fault(pgoff, write).map(|p| p.phys)
    }
    /// Every frame of a device mapping is the device's: nothing to give back, nothing for MM to free.
    fn put_page(&self, _phys: u64) -> bool {
        true
    }
    fn shared(&self) -> bool {
        true
    }
    fn track_dirty(&self) -> bool {
        true
    }
    fn sync(&self, _first: u64, _last: u64) -> Result<(), i64> {
        Ok(())
    }
    fn file(&self) -> Option<&FileMap> {
        self.name.as_deref()
    }
}

static REGISTRY: SpinLock<Vec<(u64, Arc<DeviceMap>)>> = SpinLock::named("mm.device.registry", Vec::new());

/// Makes the character device `rdev` (`vfs::abi::encode_dev(major, minor)`) mappable through `map`. False: no
/// room, or the device is registered already.
pub fn register(rdev: u64, map: Arc<DeviceMap>) -> bool {
    let mut g = REGISTRY.lock();
    if g.iter().any(|(r, _)| *r == rdev) || g.try_reserve(1).is_err() {
        return false;
    }
    g.push((rdev, map));
    drop(g);
    klog::event!(Info, "mm", "device.registered", major = vfs::abi::dev_major(rdev), minor = vfs::abi::dev_minor(rdev); "device file can be mapped by programs");
    true
}

/// Takes `rdev` out again (existing mappings stay until unmapped).
pub fn unregister(rdev: u64) -> bool {
    let mut g = REGISTRY.lock();
    let before = g.len();
    g.retain(|(r, _)| *r != rdev);
    g.len() != before
}

/// The mappable device behind an open file, if it is a registered character device.
pub fn lookup(f: &OpenFile) -> Option<Arc<DeviceMap>> {
    if f.file.mode() & uapi::S_IFMT != uapi::S_IFCHR {
        return None;
    }
    let rdev = f.file.rdev();
    REGISTRY.lock().iter().find(|(r, _)| *r == rdev).map(|(_, m)| m.clone())
}

const PROT_WRITE: u64 = 2;
const MAP_SHARED: u64 = 0x01;
const MAP_PRIVATE: u64 = 0x02;
const MAP_SHARED_VALIDATE: u64 = 0x03;
const MAP_TYPE: u64 = 0x0f;

/// The result of `check`: what the VMA gets.
pub(crate) struct DevMapped {
    pub(crate) backing: Arc<DevBacking>,
    pub(crate) may_write: bool,
}

/// The file checks of `do_mmap` for a device file (linux-6.6/mm/mmap.c:1279-1337, as `vfs::mmap::check` for
/// files): unknown flags with MAP_SHARED_VALIDATE (EOPNOTSUPP, devices support no extension flag), PROT_WRITE on
/// a mapping of an fd not open for writing (EACCES; for MAP_PRIVATE too, see the module docs), an fd not open
/// for reading (EACCES), a map type
/// other than SHARED/PRIVATE/SHARED_VALIDATE (EINVAL); then the device's own `check`.
pub(crate) fn check(f: &OpenFile, map: &Arc<DeviceMap>, prot: u64, flags: u64, pgoff: u64, len: u64) -> Result<DevMapped, i64> {
    use uapi::errno::{EACCES, EINVAL, EOPNOTSUPP};
    let ty = flags & MAP_TYPE;
    let may_write;
    let shared = match ty {
        MAP_SHARED | MAP_SHARED_VALIDATE => {
            if ty == MAP_SHARED_VALIDATE && flags & !vfs::mmap::LEGACY_MAP_MASK != 0 {
                return Err(EOPNOTSUPP);
            }
            if prot & PROT_WRITE != 0 && !f.can_write() {
                return Err(EACCES);
            }
            may_write = f.can_write();
            true
        }
        // No copy-on-write for device pages: a private mapping writes the device's frames, so it needs the
        // rights of a shared one (Linux would copy instead; V2 refuses rather than give a read-only fd writes).
        MAP_PRIVATE => {
            if prot & PROT_WRITE != 0 && !f.can_write() {
                return Err(EACCES);
            }
            may_write = f.can_write();
            false
        }
        _ => return Err(EINVAL),
    };
    if !f.can_read() {
        return Err(EACCES);
    }
    map.ops().check(pgoff, len / crate::PAGE, prot, shared)?;
    let name = f.path.clone().map(|p| FileMap::new(p, shared && f.can_write(), shared));
    Ok(DevMapped { backing: Arc::new(DevBacking { map: map.clone(), name }), may_write })
}
