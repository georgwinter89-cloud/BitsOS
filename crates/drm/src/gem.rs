//! DR-6: amdgpu's GEM buffer objects (linux-6.6/drivers/gpu/drm/amd/amdgpu/amdgpu_gem.c, amdgpu_object.c) on the
//! memory a `GpuDevice` hands out (`GpuDevice::bo_memory`: `DmaPages` for GTT, the carve-out for VRAM).
//!
//! A `Bo` has its size (page-aligned at create), page alignment, create flags, preferred and allowed domains,
//! tiling flags and metadata (GEM_METADATA), a fake mmap offset (GEM_MMAP, from `DRM_FILE_PAGE_OFFSET_START` =
//! 4 GiB on, 1 TiB of them, freed ranges reused), a reservation object (`Resv`: fences of jobs for implicit
//! sync, shared with its dma-buf) and, while some file has a handle for it, the dma-buf it was exported as
//! (`obj->dma_buf`) and its GEM_FLINK name. Imported foreign dma-bufs become objects over the exporter's pages
//! (`prime`). The per-file state is `FileGem`; the render node's `mmap` of offsets is `gem_map`.
//!
//! Lifetime: handles (any file), dma-buf fds (through `prime::BoExporter`) and program mappings (they hold the
//! memory, not the object) keep things alive; the object's memory goes back to the backend at the last drop.

use crate::dispatch::Ctx;
use crate::dma_buf::{DmaBuf, Exporter};
use crate::fence::{self, Fence, Waiter};
use crate::file::{DrmFile, GemObject};
use crate::gpu::{BoPlace, GpuDevice};
use crate::resv::{Resv, Usage};
use crate::uapi::*;
use crate::wire::Wire;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::SpinLock;
use linux::errno::*;
use mm::device::Aperture;
use mm::dma_pages::PinOwner;
use vfs::Errno;

pub const PAGE: u64 = 4096;
/// `DRM_FILE_PAGE_OFFSET_START` (include/drm/drm_vma_manager.h:34) in pages: offsets start at 4 GiB, above any
/// offset a file means (amdgpu's BO manager uses the same `MMAP_BASE`).
pub const MMAP_START_PG: u64 = (1 << 32) / PAGE;
/// The end of the offset space: `DRM_FILE_PAGE_OFFSET_SIZE` pages after the start (drm_vma_manager.h:38, 1 TiB
/// on 64-bit). An object that does not fit in a free range answers ENOSPC (`drm_vma_offset_add`), whatever
/// its domain: GDS/GWS/OA objects have no pages, so this range is what bounds them, as in Linux.
const MMAP_END_PG: u64 = MMAP_START_PG + ((0xFFFF_FFFF >> 12) * 256);
/// Pages of the whole offset range: no object is larger.
pub const MMAP_PAGES: u64 = MMAP_END_PG - MMAP_START_PG;
/// The share of managed RAM one DRM file may pin (MM-14's suggestion: Linux's TTM limit, half of RAM).
pub const FILE_PIN_SHARE: u64 = 50;

/// `amdgpu_gem_create_ioctl`'s accepted flags (amdgpu_gem.c:297-305).
const CREATE_FLAGS: u64 = (AMDGPU_GEM_CREATE_CPU_ACCESS_REQUIRED
    | AMDGPU_GEM_CREATE_NO_CPU_ACCESS
    | AMDGPU_GEM_CREATE_CPU_GTT_USWC
    | AMDGPU_GEM_CREATE_VRAM_CLEARED
    | AMDGPU_GEM_CREATE_VM_ALWAYS_VALID
    | AMDGPU_GEM_CREATE_EXPLICIT_SYNC
    | AMDGPU_GEM_CREATE_ENCRYPTED
    | AMDGPU_GEM_CREATE_DISCARDABLE) as u64;
const SPECIAL: u32 = AMDGPU_GEM_DOMAIN_GDS | AMDGPU_GEM_DOMAIN_GWS | AMDGPU_GEM_DOMAIN_OA;

klog::counter!(BOS, "drm.gem.created");
klog::counter!(BOS_FREED, "drm.gem.freed");

/// Objects alive now (ktests' leak balance).
static LIVE: AtomicU64 = AtomicU64::new(0);
pub fn live() -> u64 {
    LIVE.load(Ordering::Relaxed)
}

// ---- per device

/// The device's GEM state: its mmap offsets (Linux's `vma_offset_manager`) and its identity for PRIME imports.
pub struct GemShared {
    offsets: SpinLock<Offsets>,
    /// `dev->object_name_idr` (GEM_FLINK names, from 1), taken before an object's state lock.
    pub(crate) names: SpinLock<crate::handles::Handles<Weak<Bo>>>,
    userptr_warned: AtomicBool,
    /// DR-9: this device's own source of user pages (tests); None: the kernel's (`userptr::set_source`).
    user_pages: SpinLock<Option<Arc<dyn crate::userptr::UserPages>>>,
}

impl GemShared {
    pub fn new() -> Arc<GemShared> {
        Arc::new(GemShared {
            offsets: SpinLock::named("drm.gem.offsets", Offsets { map: BTreeMap::new(), next: MMAP_START_PG }),
            names: SpinLock::named("drm.gem.names", crate::handles::Handles::new()),
            userptr_warned: AtomicBool::new(false),
            user_pages: SpinLock::named("drm.gem.user_pages", None),
        })
    }

    /// DR-9: a source of user pages for this device only (tests and the fake).
    pub fn set_user_pages(&self, s: Option<Arc<dyn crate::userptr::UserPages>>) {
        let old = core::mem::replace(&mut *self.user_pages.lock(), s);
        drop(old);
    }
    pub(crate) fn user_pages(&self) -> Option<Arc<dyn crate::userptr::UserPages>> {
        self.user_pages.lock().clone()
    }

    /// GEM_USERPTR without a source of user pages: ENODEV, one Warn per device, Debug after.
    pub(crate) fn note_userptr_refused(&self, size: u64, flags: u32) {
        if !self.userptr_warned.swap(true, Ordering::Relaxed) {
            klog::event!(Warn, "drm", "gem.userptr_refused", size = size, flags = klog::Hex(flags as u64);
                "GEM_USERPTR needs the address space's pages and invalidation (no source yet): ENODEV (Linux without HMM_MIRROR)");
        } else {
            klog::event!(Debug, "drm", "gem.userptr_refused", size = size, flags = klog::Hex(flags as u64));
        }
    }

    /// The object whose offset range starts at `pg` and holds `pages` (`drm_vma_offset_exact_lookup_locked`).
    pub(crate) fn exact(&self, pg: u64, pages: u64) -> Option<Arc<Bo>> {
        let g = self.offsets.lock();
        let (n, w) = g.map.get(&pg)?;
        if pages > *n {
            return None;
        }
        w.upgrade()
    }

    /// `drm_vma_offset_add`: a free range of `pages` (ENOSPC when none is left; nothing changes then).
    fn register(&self, pages: u64, bo: &Weak<Bo>) -> Result<u64, Errno> {
        if pages == 0 || pages > MMAP_END_PG - MMAP_START_PG {
            return Err(ENOSPC);
        }
        let mut g = self.offsets.lock();
        let pg = if g.next <= MMAP_END_PG - pages { g.next } else { g.first_fit(pages).ok_or(ENOSPC)? };
        g.next = g.next.max(pg + pages);
        g.map.insert(pg, (pages, bo.clone()));
        Ok(pg)
    }
}

/// The offset space: ranges handed out from `next` up; once the end is reached, the first free range that
/// fits (freed objects give theirs back, as in Linux's `drm_mm`).
struct Offsets {
    /// Start page -> (pages, object).
    map: BTreeMap<u64, (u64, Weak<Bo>)>,
    next: u64,
}

impl Offsets {
    fn first_fit(&self, pages: u64) -> Option<u64> {
        let mut at = MMAP_START_PG;
        for (s, (n, _)) in &self.map {
            if s - at >= pages {
                return Some(at);
            }
            at = s + n;
        }
        (MMAP_END_PG - at >= pages).then_some(at)
    }
}

// ---- the object

pub(crate) struct BoState {
    pub(crate) preferred: u32,
    pub(crate) allowed: u32,
    tiling: u64,
    metadata: Vec<u8>,
    metadata_flags: u64,
    /// Handles of all files (`handle_count`).
    pub(crate) handles: u64,
    /// `obj->dma_buf`: the buffer it was exported or imported as, kept while a handle exists.
    pub(crate) dma_buf: Option<Arc<DmaBuf>>,
    /// `obj->name`: its GEM_FLINK name (0: none), dropped with the last handle.
    pub(crate) name: u32,
}

pub struct Bo {
    me: Weak<Bo>,
    pub(crate) gem: Arc<GemShared>,
    size: u64,
    /// `tbo.page_alignment` (pages; Linux's u32 arithmetic of amdgpu_bo_create).
    page_align: u32,
    /// Create flags; CPU_ACCESS_REQUIRED comes and goes (a hint, set again by a CPU fault).
    flags: AtomicU64,
    place: Option<BoPlace>,
    pub(crate) mem: Arc<dyn Exporter>,
    resv: Arc<Resv>,
    pgoff: u64,
    /// The offset range is the object's (false only for a create that found no range: it never existed).
    registered: bool,
    /// `obj->import_attach->dmabuf`: a foreign buffer this object stands for.
    pub(crate) import: Option<Arc<DmaBuf>>,
    pub(crate) st: SpinLock<BoState>,
    /// The mapping of this object alone (render-node mmap with CR-DR6-1).
    pub(crate) cpu_map: SpinLock<Option<Arc<mm::device::DeviceMap>>>,
    /// DR-9: a GEM_USERPTR object's range (its `mem` is the same memory).
    userptr: Option<Arc<crate::userptr::UserptrMem>>,
}

/// What a create asks for.
pub(crate) struct NewBo {
    pub(crate) size: u64,
    pub(crate) page_align: u32,
    pub(crate) flags: u64,
    pub(crate) resv: Option<Arc<Resv>>,
    pub(crate) mem: Arc<dyn Exporter>,
    pub(crate) place: Option<BoPlace>,
    pub(crate) import: Option<Arc<DmaBuf>>,
    pub(crate) preferred: u32,
    pub(crate) allowed: u32,
    pub(crate) userptr: Option<Arc<crate::userptr::UserptrMem>>,
}

impl Bo {
    pub(crate) fn new(gem: &Arc<GemShared>, n: NewBo) -> Result<Arc<Bo>, Errno> {
        let pages = n.size / PAGE;
        let mut reg = Err(ENOSPC);
        let bo = Arc::new_cyclic(|me: &Weak<Bo>| {
            reg = gem.register(pages, me);
            LIVE.fetch_add(1, Ordering::Relaxed);
            Bo {
                me: me.clone(),
                gem: gem.clone(),
                size: n.size,
                page_align: n.page_align,
                flags: AtomicU64::new(n.flags),
                place: n.place,
                mem: n.mem,
                resv: n.resv.unwrap_or_else(Resv::new),
                pgoff: *reg.as_ref().unwrap_or(&0),
                registered: reg.is_ok(),
                import: n.import,
                st: SpinLock::named(
                    "drm.gem.bo",
                    BoState { preferred: n.preferred, allowed: n.allowed, tiling: 0, metadata: Vec::new(), metadata_flags: 0, handles: 0, dma_buf: None, name: 0 },
                ),
                cpu_map: SpinLock::named("drm.gem.bo_map", None),
                userptr: n.userptr,
            }
        });
        reg?;
        BOS.inc();
        klog::event!(Debug, "drm", "gem.created", size = bo.size, place = alloc::format!("{:?}", bo.place).as_str(), flags = klog::Hex(bo.flags()),
            offset = klog::Hex(bo.pgoff * PAGE), imported = bo.import.is_some());
        Ok(bo)
    }

    pub fn size(&self) -> u64 {
        self.size
    }
    pub(crate) fn weak(&self) -> Weak<Bo> {
        self.me.clone()
    }
    pub fn flags(&self) -> u64 {
        self.flags.load(Ordering::Relaxed)
    }
    /// A CPU touched the object through a mapping: `amdgpu_bo_fault_reserve_notify` remembers it
    /// (CPU_ACCESS_REQUIRED, amdgpu_object.c:1396).
    pub(crate) fn cpu_touched(&self) {
        self.flags.fetch_or(AMDGPU_GEM_CREATE_CPU_ACCESS_REQUIRED as u64, Ordering::Relaxed);
    }
    /// Where the memory is (None: GDS/GWS/OA, which have no pages; an import is GTT).
    pub fn place(&self) -> Option<BoPlace> {
        self.place
    }
    /// The fake mmap offset in bytes (`amdgpu_bo_mmap_offset`).
    pub fn mmap_offset(&self) -> u64 {
        self.pgoff * PAGE
    }
    pub(crate) fn pgoff(&self) -> u64 {
        self.pgoff
    }
    pub fn pages(&self) -> u64 {
        self.size / PAGE
    }
    pub fn resv(&self) -> &Arc<Resv> {
        &self.resv
    }
    pub fn memory(&self) -> &Arc<dyn Exporter> {
        &self.mem
    }
    pub fn is_import(&self) -> bool {
        self.import.is_some()
    }
    pub fn preferred_domains(&self) -> u32 {
        self.st.lock().preferred
    }
    pub fn allowed_domains(&self) -> u32 {
        self.st.lock().allowed
    }
    pub fn handle_count(&self) -> u64 {
        self.st.lock().handles
    }
    /// The program may map the object (`amdgpu_gem_object_mmap`: not with NO_CPU_ACCESS, not a userptr one).
    pub fn cpu_access(&self) -> bool {
        self.flags() & AMDGPU_GEM_CREATE_NO_CPU_ACCESS as u64 == 0 && self.userptr.is_none()
    }
    /// DR-9: a GEM_USERPTR object (`amdgpu_ttm_tt_get_usermm` is set).
    pub fn is_userptr(&self) -> bool {
        self.userptr.is_some()
    }
    pub fn userptr(&self) -> Option<&Arc<crate::userptr::UserptrMem>> {
        self.userptr.as_ref()
    }
    /// What MM maps for programs (None: no pages to map).
    pub(crate) fn aperture(&self) -> Option<Aperture> {
        self.mem.aperture()
    }

    /// `amdgpu_bo_fence` (amdgpu_object.c:1440): a job's fence on the object, as a reader (`shared`) or writer.
    pub fn fence(&self, f: &Arc<Fence>, shared: bool) -> Result<(), Errno> {
        self.resv.add_fence(f, if shared { Usage::Read } else { Usage::Write })
    }

    /// The fences a new job on the object waits for (`amdgpu_sync_resv` with the CS's mode, amdgpu_cs.c): an
    /// EXPLICIT_SYNC object only the kernel's own (moves, clears); others implicit sync's: a writer waits for
    /// writers and readers, a reader for the writers.
    pub fn implicit_deps(&self, write: bool) -> Vec<Arc<Fence>> {
        if self.flags() & AMDGPU_GEM_CREATE_EXPLICIT_SYNC as u64 != 0 {
            return self.resv.fences(Usage::Kernel);
        }
        self.resv.fences(Usage::rw(write))
    }

    /// DR-8: `implicit_deps` for a CS of the file `owner` (`amdgpu_cs_sync_rings`, amdgpu_cs.c:1190): the
    /// file's own jobs are not waited for (AMDGPU_SYNC_NE_OWNER), an EXPLICIT_SYNC object gives the kernel's.
    pub fn cs_deps(&self, write: bool, owner: u64) -> Vec<Arc<Fence>> {
        if self.flags() & AMDGPU_GEM_CREATE_EXPLICIT_SYNC as u64 != 0 {
            return self.resv.fences(Usage::Kernel);
        }
        self.resv.fences_not_owned(Usage::rw(write), owner)
    }

    /// Kernel access (tests, importers).
    pub fn read(&self, off: u64, out: &mut [u8]) -> bool {
        self.mem.read(off, out)
    }
    pub fn write(&self, off: u64, data: &[u8]) -> bool {
        self.mem.write(off, data)
    }
}

impl GemObject for Bo {
    fn size(&self) -> u64 {
        self.size
    }
    fn into_bo(self: Arc<Self>) -> Option<Arc<Bo>> {
        Some(self)
    }
    fn handle_opened(&self) {
        self.st.lock().handles += 1;
    }
    /// The last handle lets the exported dma-buf go (`drm_gem_object_exported_dma_buf_free`): its fds keep it.
    fn handle_closed(&self) {
        let (buf, named) = {
            let mut g = self.st.lock();
            g.handles = g.handles.saturating_sub(1);
            if g.handles == 0 { (g.dma_buf.take(), g.name != 0) } else { (None, false) }
        };
        if named {
            // `drm_gem_object_handle_free`, under the name lock: a GEM_OPEN in between keeps the name.
            let mut n = self.gem.names.lock();
            let mut g = self.st.lock();
            if g.handles == 0 && g.name != 0 {
                n.remove(g.name);
                g.name = 0;
            }
        }
        drop(buf);
    }
}

impl Drop for Bo {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::Relaxed);
        if !self.registered {
            return;
        }
        let mine = {
            let mut g = self.gem.offsets.lock();
            match g.map.get(&self.pgoff) {
                Some((_, w)) if w.ptr_eq(&self.me) => g.map.remove(&self.pgoff),
                _ => None,
            }
        };
        drop(mine);
        BOS_FREED.inc();
        klog::event!(Debug, "drm", "gem.freed", size = self.size, offset = klog::Hex(self.pgoff * PAGE));
    }
}

/// The memory of a GDS/GWS/OA object: none (Linux keeps them in the system domain without pages until a CS).
pub(crate) struct NoPages(pub(crate) u64);

impl Exporter for NoPages {
    fn name(&self) -> &'static str {
        "amdgpu"
    }
    fn size(&self) -> u64 {
        self.0
    }
    fn aperture(&self) -> Option<Aperture> {
        None
    }
    fn page_phys(&self, _page: u64) -> Option<u64> {
        None
    }
    fn read(&self, _off: u64, _out: &mut [u8]) -> bool {
        false
    }
    fn write(&self, _off: u64, _data: &[u8]) -> bool {
        false
    }
}

// ---- per file

/// `file_priv->prime`: handle <-> dma-buf of this file (`drm_prime_add_buf_handle`).
#[derive(Default)]
pub(crate) struct Prime {
    pub(crate) by_handle: BTreeMap<u32, Arc<DmaBuf>>,
    pub(crate) by_buf: BTreeMap<usize, u32>,
}

/// A DRM file's GEM state.
pub struct FileGem {
    /// The file's pin share (MM-14): one client cannot pin all of RAM.
    pub(crate) owner: Arc<PinOwner>,
    /// The VM root's reservation object: VM_ALWAYS_VALID objects share it (`vm->root.bo->tbo.base.resv`).
    pub(crate) vm_resv: Arc<Resv>,
    /// `file_priv->prime.lock`: an export or import of this file at a time.
    pub(crate) prime_lock: ksync::Mutex<()>,
    pub(crate) prime: SpinLock<Prime>,
    /// The render node's mapping view (`gem_map`) with the handle generation it was built for.
    pub(crate) map: SpinLock<(u64, Option<Arc<mm::device::DeviceMap>>)>,
    /// Counts handle changes.
    pub(crate) generation: AtomicU64,
}

impl Default for FileGem {
    fn default() -> Self {
        FileGem::new()
    }
}

pub(crate) fn buf_key(b: &Arc<DmaBuf>) -> usize {
    Arc::as_ptr(b) as *const () as usize
}

impl FileGem {
    pub fn new() -> FileGem {
        FileGem {
            owner: PinOwner::new(FILE_PIN_SHARE),
            vm_resv: Resv::new(),
            prime_lock: ksync::Mutex::named("drm.file.prime_lock", ()),
            prime: SpinLock::named("drm.file.prime", Prime::default()),
            map: SpinLock::named("drm.file.map", (0, None)),
            generation: AtomicU64::new(0),
        }
    }
    pub fn owner(&self) -> &Arc<PinOwner> {
        &self.owner
    }
    /// The handles changed: the mapping view is built again at the next mmap (dropped outside the lock).
    pub(crate) fn changed(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let old = self.map.lock().1.take();
        drop(old);
    }
    pub(crate) fn prime_forget(&self, h: u32) -> Option<Arc<DmaBuf>> {
        let mut g = self.prime.lock();
        let b = g.by_handle.remove(&h)?;
        g.by_buf.remove(&buf_key(&b));
        Some(b)
    }
    pub(crate) fn prime_clear(&self) -> Vec<Arc<DmaBuf>> {
        let mut g = self.prime.lock();
        g.by_buf.clear();
        core::mem::take(&mut g.by_handle).into_values().collect()
    }
}

// ---- GEM_CREATE

/// `amdgpu_is_tmz`: the chip has TMZ on (DEV_INFO's ids_flags say so).
fn tmz(gpu: &dyn GpuDevice) -> bool {
    gpu.dev_info().ids_flags & AMDGPU_IDS_FLAGS_TMZ as u64 != 0
}

/// `amdgpu_bo_create`'s size and alignment rules (amdgpu_object.c:558-573), Linux's integer widths included:
/// GWS/OA count entries (one page each), GDS bytes in units of 4 (one page each), all else bytes rounded up to
/// pages. Returns (size in bytes, page alignment).
fn shape(size: u64, align: u64, domain: u32) -> (u64, u32) {
    let a = align as u32 as i32;
    if domain & (AMDGPU_GEM_DOMAIN_GWS | AMDGPU_GEM_DOMAIN_OA) != 0 {
        (size.wrapping_shl(12), a as u32)
    } else if domain & AMDGPU_GEM_DOMAIN_GDS != 0 {
        ((size.wrapping_add(3) & !3).wrapping_shl(12), (a.wrapping_add(3) & !3) as u32)
    } else {
        (size.wrapping_add(PAGE - 1) & !(PAGE - 1), ((a.wrapping_add(4095) & !4095) >> 12) as u32)
    }
}

/// `amdgpu_bo_validate_size`: with GTT in the domains the object must be smaller than GTT, else with VRAM
/// smaller than VRAM; ENOMEM otherwise.
fn validate_size(gpu: &dyn GpuDevice, size: u64, domain: u32) -> Result<(), Errno> {
    let m = gpu.memory();
    let ok = if domain & AMDGPU_GEM_DOMAIN_GTT != 0 {
        size < m.gtt_total
    } else if domain & AMDGPU_GEM_DOMAIN_VRAM != 0 {
        size < m.vram_total
    } else {
        true
    };
    if ok { Ok(()) } else { Err(ENOMEM) }
}

/// `amdgpu_gem_object_create` -> `amdgpu_bo_create` for one domain set: the first place of
/// `amdgpu_bo_placement_from_domain`'s order (VRAM, GTT, then system) that has room.
pub(crate) fn create_bo(gpu: &dyn GpuDevice, file: &FileGem, size: u64, align: u64, domain: u32, flags: u64, resv: Option<Arc<Resv>>) -> Result<Arc<Bo>, Errno> {
    let (size, page_align) = shape(size, align, domain);
    validate_size(gpu, size, domain)?;
    if size == 0 {
        // ttm_bo_init_reserved -> drm_vma_offset_add -> drm_mm_insert_node: ENOSPC for zero pages.
        return Err(ENOSPC);
    }
    let preferred = domain;
    let mut allowed = preferred;
    if flags & AMDGPU_GEM_CREATE_DISCARDABLE as u64 == 0 && allowed == AMDGPU_GEM_DOMAIN_VRAM {
        allowed |= AMDGPU_GEM_DOMAIN_GTT;
    }
    let mut flags = flags;
    if gpu.profile().ras_features.is_some_and(|m| m != 0) {
        flags |= AMDGPU_GEM_CREATE_VRAM_WIPE_ON_RELEASE as u64;
    }
    let (mem, place): (Arc<dyn Exporter>, Option<BoPlace>) = if domain & SPECIAL != 0 {
        (Arc::new(NoPages(size)), None)
    } else {
        let mut order: Vec<BoPlace> = Vec::new();
        if domain & AMDGPU_GEM_DOMAIN_VRAM != 0 {
            order.push(BoPlace::Vram);
        }
        if domain & AMDGPU_GEM_DOMAIN_GTT != 0 {
            order.push(BoPlace::Gtt);
        }
        if domain & AMDGPU_GEM_DOMAIN_CPU != 0 || order.is_empty() {
            order.push(BoPlace::System);
        }
        let wc = flags & AMDGPU_GEM_CREATE_CPU_GTT_USWC as u64 != 0;
        let mut got = Err(ENOMEM);
        for p in order {
            got = gpu.bo_memory(p, size / PAGE, page_align, wc && p != BoPlace::Vram, &file.owner).map(|m| (m, Some(p)));
            if got.is_ok() {
                break;
            }
        }
        got?
    };
    // "Treat CPU_ACCESS_REQUIRED only as a hint if given by UMD" (amdgpu_object.c:653): gone once created.
    flags &= !(AMDGPU_GEM_CREATE_CPU_ACCESS_REQUIRED as u64);
    Bo::new(&gpu.shared().gem, NewBo { size, page_align, flags, resv, mem, place, import: None, preferred, allowed, userptr: None })
}

/// DR-9: the object of a GEM_USERPTR (`amdgpu_gem_object_create(.., DOMAIN_CPU, 0, ..)`, then GTT as preferred
/// and allowed domain, amdgpu_gem.c:413): its memory is the user range.
pub(crate) fn userptr_bo(gpu: &dyn GpuDevice, size: u64, resv: Arc<Resv>, mem: Arc<crate::userptr::UserptrMem>) -> Result<Arc<Bo>, Errno> {
    let mut flags = 0u64;
    if gpu.profile().ras_features.is_some_and(|m| m != 0) {
        flags |= AMDGPU_GEM_CREATE_VRAM_WIPE_ON_RELEASE as u64;
    }
    let n = NewBo {
        size,
        page_align: 0,
        flags,
        resv: Some(resv),
        mem: mem.clone(),
        place: Some(BoPlace::Gtt),
        import: None,
        preferred: AMDGPU_GEM_DOMAIN_GTT,
        allowed: AMDGPU_GEM_DOMAIN_GTT,
        userptr: Some(mem),
    };
    Bo::new(&gpu.shared().gem, n)
}

/// `amdgpu_gem_create_ioctl` (amdgpu_gem.c:278): checks in Linux's order, the two retries (without
/// CPU_ACCESS_REQUIRED, then VRAM with GTT), a handle; `out` replaces the whole union.
pub fn create_ioctl(ctx: &Ctx, gpu: &dyn GpuDevice, data: &mut [u8]) -> Result<i64, Errno> {
    let args = drm_amdgpu_gem_create::decode(data).ok_or(EINVAL)?;
    let a = args.r#in();
    let mut flags = a.domain_flags;
    if a.domains & AMDGPU_GEM_DOMAIN_DOORBELL as u64 != 0 || flags & !CREATE_FLAGS != 0 || a.domains & !(AMDGPU_GEM_DOMAIN_MASK as u64) != 0 {
        return Err(EINVAL);
    }
    if flags & AMDGPU_GEM_CREATE_ENCRYPTED as u64 != 0 && !tmz(gpu) {
        klog::event!(Info, "drm", "gem.no_tmz"; "an encrypted buffer was asked for, the chip has TMZ off: EINVAL");
        return Err(EINVAL);
    }
    let always_valid = flags & AMDGPU_GEM_CREATE_VM_ALWAYS_VALID as u64 != 0;
    if a.domains & SPECIAL as u64 != 0 {
        if always_valid {
            return Err(EINVAL);
        }
        flags |= AMDGPU_GEM_CREATE_NO_CPU_ACCESS as u64;
    }
    let resv = always_valid.then(|| ctx.file.gem.vm_resv.clone());
    let mut domain = a.domains as u32;
    let bo = loop {
        match create_bo(gpu, &ctx.file.gem, a.bo_size, a.alignment, domain, flags, resv.clone()) {
            Ok(b) => break b,
            Err(e) => {
                if flags & AMDGPU_GEM_CREATE_CPU_ACCESS_REQUIRED as u64 != 0 {
                    flags &= !(AMDGPU_GEM_CREATE_CPU_ACCESS_REQUIRED as u64);
                    continue;
                }
                if domain == AMDGPU_GEM_DOMAIN_VRAM {
                    domain |= AMDGPU_GEM_DOMAIN_GTT;
                    continue;
                }
                klog::event!(Debug, "drm", "gem.create_failed", size = a.bo_size, domains = klog::Hex(domain as u64), align = a.alignment, errno = e);
                return Err(e);
            }
        }
    };
    let h = ctx.file.handle_create(bo)?;
    let mut o = drm_amdgpu_gem_create::ZERO;
    o.set_out(&drm_amdgpu_gem_create_out { handle: h, _pad: 0 });
    o.encode(data).ok_or(EINVAL)?;
    Ok(0)
}

// ---- GEM_MMAP, GEM_WAIT_IDLE, GEM_METADATA, GEM_OP, GEM_USERPTR

/// `amdgpu_gem_mmap_ioctl` -> `amdgpu_mode_dumb_mmap` (amdgpu_gem.c:459-490): ENOENT, EPERM without CPU access.
pub fn mmap_ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    let args = drm_amdgpu_gem_mmap::decode(data).ok_or(EINVAL)?;
    let h = args.r#in().handle;
    let mut o = drm_amdgpu_gem_mmap::ZERO;
    // Linux clears the union before the lookup: an error answer comes back zeroed too.
    o.encode(data).ok_or(EINVAL)?;
    let bo = ctx.file.bo(h).ok_or(ENOENT)?;
    if !bo.cpu_access() {
        return Err(EPERM);
    }
    o.set_out(&drm_amdgpu_gem_mmap_out { addr_ptr: bo.mmap_offset() });
    o.encode(data).ok_or(EINVAL)?;
    Ok(0)
}

/// Naps between looks at a fence while a wait goes on (a callback wakes it at once).
const NAP_NS: u64 = 10_000_000;

/// `dma_resv_wait_timeout(.., USAGE_READ, intr, timeout)`: Ok(true) when every fence signaled, Ok(false) when
/// the deadline came first; EINTR on a kill or a signal. `end` None = no deadline (bounded by the fences: a
/// reset or unplug force-completes them, DR-3); Some(0) = one look.
pub(crate) fn wait_resv(ctx: &Ctx, resv: &Resv, usage: Usage, end: Option<u64>) -> Result<bool, Errno> {
    for f in resv.fences(usage) {
        if fence::is_signaled(&f) {
            continue;
        }
        let w = Waiter::new();
        let _reg = fence::add_callback(&f, w.clone());
        while !fence::is_signaled(&f) {
            if ctx.mem.interrupted() || ctx.mem.signal_pending() {
                return Err(EINTR);
            }
            let nap = match end {
                None => NAP_NS,
                Some(e) => match fence::now() {
                    Some(now) if now < e => (e - now).min(NAP_NS),
                    _ => return Ok(false),
                },
            };
            w.sleep(nap);
        }
    }
    Ok(true)
}

/// `amdgpu_gem_timeout` as a deadline: a negative timeout waits for ever (None), a past one looks once.
pub(crate) fn deadline(timeout_ns: u64) -> Option<u64> {
    if (timeout_ns as i64) < 0 {
        return None;
    }
    Some(timeout_ns)
}

/// `amdgpu_gem_wait_idle_ioctl` (amdgpu_gem.c:519): status 1 while busy at the deadline.
pub fn wait_idle_ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    let args = drm_amdgpu_gem_wait_idle::decode(data).ok_or(EINVAL)?;
    let a = args.r#in();
    let bo = ctx.file.bo(a.handle).ok_or(ENOENT)?;
    let idle = wait_resv(ctx, &bo.resv, Usage::Read, deadline(a.timeout))?;
    let mut o = drm_amdgpu_gem_wait_idle::ZERO;
    o.set_out(&drm_amdgpu_gem_wait_idle_out { status: !idle as u32, domain: 0 });
    o.encode(data).ok_or(EINVAL)?;
    Ok(0)
}

const METADATA_MAX: usize = 64 * 4;

/// `amdgpu_gem_metadata_ioctl` (amdgpu_gem.c:552) with `amdgpu_bo_{get,set}_{tiling_flags,metadata}`
/// (amdgpu_object.c:1126-1240). GFX 11 has no tiling check (only families up to CZ do). An unknown op answers
/// 0 and changes nothing, as in Linux. Bytes of `data` past the metadata come back as the caller sent them.
pub fn metadata_ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    let mut a = drm_amdgpu_gem_metadata::decode(data).ok_or(EINVAL)?;
    let bo = ctx.file.bo(a.handle).ok_or(ENOENT)?;
    if a.op == AMDGPU_GEM_METADATA_OP_GET_METADATA {
        let g = bo.st.lock();
        a.data.tiling_info = g.tiling;
        a.data.data_size_bytes = g.metadata.len() as u32;
        for (k, c) in g.metadata.chunks(4).enumerate() {
            let mut w = a.data.data[k].to_le_bytes();
            w[..c.len()].copy_from_slice(c);
            a.data.data[k] = u32::from_le_bytes(w);
        }
        a.data.flags = g.metadata_flags;
    } else if a.op == AMDGPU_GEM_METADATA_OP_SET_METADATA {
        let n = a.data.data_size_bytes as usize;
        if n > METADATA_MAX {
            return Err(EINVAL);
        }
        let mut md = Vec::new();
        md.try_reserve_exact(n).map_err(|_| ENOMEM)?;
        md.extend(a.data.data.iter().flat_map(|w| w.to_le_bytes()).take(n));
        let old = {
            let mut g = bo.st.lock();
            g.tiling = a.data.tiling_info;
            if n == 0 {
                // A zero size drops the metadata and keeps the old flags (amdgpu_bo_set_metadata).
                core::mem::take(&mut g.metadata)
            } else {
                g.metadata_flags = a.data.flags;
                core::mem::replace(&mut g.metadata, md)
            }
        };
        drop(old);
    }
    a.encode(data).ok_or(EINVAL)?;
    Ok(0)
}

/// `amdgpu_gem_op_ioctl` (amdgpu_gem.c:804): GET_GEM_CREATE_INFO copies `drm_amdgpu_gem_create_in` to `value`,
/// SET_PLACEMENT changes the preferred and allowed domains (no move: the object stays where it is until a CS
/// validates it, GX), anything else EINVAL.
pub fn op_ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    let a = drm_amdgpu_gem_op::decode(data).ok_or(EINVAL)?;
    let bo = ctx.file.bo(a.handle).ok_or(ENOENT)?;
    match a.op {
        AMDGPU_GEM_OP_GET_GEM_CREATE_INFO => {
            let info = drm_amdgpu_gem_create_in {
                bo_size: bo.size,
                alignment: bo.page_align.wrapping_shl(12) as u64,
                domains: bo.preferred_domains() as u64,
                domain_flags: bo.flags(),
            };
            let mut b = [0u8; drm_amdgpu_gem_create_in::SIZE];
            info.encode(&mut b).ok_or(EINVAL)?;
            if !ctx.mem.write(a.value, &b) {
                return Err(EFAULT);
            }
            Ok(0)
        }
        AMDGPU_GEM_OP_SET_PLACEMENT => {
            if bo.import.is_some() && a.value & AMDGPU_GEM_DOMAIN_VRAM as u64 != 0 {
                return Err(EINVAL);
            }
            if bo.is_userptr() {
                return Err(EPERM);
            }
            let p = (a.value & (AMDGPU_GEM_DOMAIN_VRAM | AMDGPU_GEM_DOMAIN_GTT | AMDGPU_GEM_DOMAIN_CPU) as u64) as u32;
            let mut g = bo.st.lock();
            g.preferred = p;
            g.allowed = if p == AMDGPU_GEM_DOMAIN_VRAM { p | AMDGPU_GEM_DOMAIN_GTT } else { p };
            Ok(0)
        }
        _ => Err(EINVAL),
    }
}

/// `amdgpu_gem_userptr_ioctl` (amdgpu_gem.c:378): DR-9's `userptr`.
pub fn userptr_ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    crate::userptr::ioctl(ctx, data)
}

/// The last close of a file: what DR-6 keeps besides the handles (the mapping view).
pub fn release_file(file: &DrmFile) {
    file.gem.changed();
}
