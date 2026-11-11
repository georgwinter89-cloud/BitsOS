//! DR-9: GEM_USERPTR objects (linux-6.6/drivers/gpu/drm/amd/amdgpu/amdgpu_gem.c:378, amdgpu_hmm.c,
//! amdgpu_ttm.c:659 `amdgpu_ttm_tt_get_user_pages`): a GPU object over a range of the creating program's own
//! memory, page-aligned, read-only with AMDGPU_GEM_USERPTR_READONLY (the GPU cannot write it: its writes fault).
//!
//! The pages come from a `UserPages` source, the address-space side of Linux's `hmm_range_fault` (MM provides it;
//! a device may have its own for tests). Without a source GEM_USERPTR stays what Linux without HMM answers
//! (ENODEV). When the program unmaps or changes the range, its address space calls `invalidate` (Linux's MMU
//! interval notifier, `amdgpu_hmm_invalidate_gfx`): the object waits (bounded) for the GPU jobs on it and marks
//! its pages stale; the next CS that lists it gets the pages again (EFAULT once they are gone, EAGAIN when they
//! changed meanwhile). The source's pages stay referenced until replaced, so a GPU that still reads a stale page
//! reads memory nobody else got.

use crate::dispatch::Ctx;
use crate::dma_buf::Exporter;
use crate::fence::{self, Waiter};
use crate::gem::{Bo, PAGE};
use crate::resv::{Resv, Usage};
use crate::uapi::*;
use crate::wire::Wire;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::SpinLock;
use linux::errno::*;
use mm::device::Aperture;
use vfs::Errno;

/// The longest an invalidation waits for the GPU jobs on an object (Linux waits without end; every job here ends
/// in bounded time, a hung one at the job timeout of 10 s).
const INVALIDATE_WAIT_NS: u64 = 10_000_000_000;
const NAP_NS: u64 = 10_000_000;

/// The calling program's memory as pages a GPU object can use.
pub trait UserPages: Send + Sync {
    /// The caller's address space (`current->mm`): an id that stays the same for the program's life and is not
    /// reused while it lives. None: the caller has none (a kernel thread).
    fn current_mm(&self) -> Option<u64>;
    /// The pages of `[addr, addr + size)` of address space `mm` (`vma_lookup` + `hmm_range_fault`), held until
    /// the returned memory is dropped: EFAULT when a page is not mapped (or not writable unless `readonly`), EPERM
    /// when `anon_only` and the first mapping is a file's, ESRCH while the program ends.
    fn get(&self, mm: u64, addr: u64, size: u64, readonly: bool, anon_only: bool) -> Result<Arc<dyn Exporter>, Errno>;
}

static SOURCE: SpinLock<Option<Arc<dyn UserPages>>> = SpinLock::named("drm.userptr.source", None);

/// The kernel's source (MM's, CR-DR9-2); None takes it away again.
pub fn set_source(s: Option<Arc<dyn UserPages>>) {
    let had = SOURCE.lock().is_some();
    klog::event!(Info, "drm", "userptr.source", set = s.is_some(), had = had);
    let old = core::mem::replace(&mut *SOURCE.lock(), s);
    drop(old);
}

/// The kernel has a source (MM's, CR-DR9-2): until then GEM_USERPTR answers ENODEV.
pub fn source_set() -> bool {
    SOURCE.lock().is_some()
}

/// The device's own source (tests), else the kernel's.
fn source(ctx: &Ctx) -> Option<Arc<dyn UserPages>> {
    let dev = ctx.dev.driver().gpu().and_then(|g| g.shared().gem.user_pages());
    dev.or_else(|| SOURCE.lock().clone())
}

// ---- the registry the address spaces invalidate through

/// (mm, start, object) -> (size, object). Ranges overlap freely; the longest object's size bounds the look back
/// from `start` (`sizes` counts the objects per size, so the bound shrinks again when they go).
/// Only weak references are taken under the lock: an object's last drop takes the lock itself.
struct Registry {
    map: BTreeMap<(u64, u64, usize), (u64, Weak<UserptrMem>)>,
    sizes: BTreeMap<u64, usize>,
}

impl Registry {
    fn longest(&self) -> u64 {
        self.sizes.last_key_value().map_or(0, |(n, _)| *n)
    }
    fn size_gone(&mut self, size: u64) {
        if let Some(c) = self.sizes.get_mut(&size) {
            *c -= 1;
            if *c == 0 {
                self.sizes.remove(&size);
            }
        }
    }
}

static REG: SpinLock<Registry> =
    SpinLock::named("drm.userptr.registry", Registry { map: BTreeMap::new(), sizes: BTreeMap::new() });

/// Objects registered now for address space `mm` (tests).
pub fn registered_in(mm: u64) -> usize {
    REG.lock().map.range((mm, 0, 0)..=(mm, u64::MAX, usize::MAX)).count()
}

/// The address space `mm` changed `[start, end)` (munmap, mremap, mprotect, a COW break...): every object over
/// a part of it waits for its GPU jobs (bounded) and gets its pages again at its next CS. Returns the objects hit.
/// Called by MM (CR-DR9-2) in the thread that changes the mapping, with no lock of MM's that a GPU job's end
/// needs.
pub fn invalidate(mm: u64, start: u64, end: u64) -> usize {
    // An empty or inverted range touches nothing (and would be an inverted BTreeMap range).
    if end <= start {
        return 0;
    }
    let weak: alloc::vec::Vec<Weak<UserptrMem>> = {
        let g = REG.lock();
        let from = start.saturating_sub(g.longest()).min(end);
        g.map
            .range((mm, from, 0)..(mm, end, 0))
            .filter(|((_, a, _), (n, _))| *a < end && a.saturating_add(*n) > start)
            .map(|(_, (_, w))| w.clone())
            .collect()
    };
    let hit: alloc::vec::Vec<Arc<UserptrMem>> = weak.iter().filter_map(Weak::upgrade).collect();
    // One bound for the whole call, however many objects it hits (B4).
    let mut budget = WaitBudget::new();
    for m in &hit {
        m.seq.fetch_add(1, Ordering::AcqRel);
        if !wait_idle(&m.resv, &mut budget) {
            klog::event!(Warn, "drm", "userptr.invalidate_timeout", addr = klog::Hex(m.addr), size = m.size;
                "the GPU still uses unmapped user pages after 10 s: it keeps reading the old pages");
        }
    }
    if !hit.is_empty() {
        klog::event!(Debug, "drm", "userptr.invalidated", mm = mm, start = klog::Hex(start), end = klog::Hex(end), objects = hit.len());
    }
    hit.len()
}

/// The program `mm` ends (`mmu_notifier release`): all of it.
pub fn mm_gone(mm: u64) -> usize {
    invalidate(mm, 0, u64::MAX)
}

/// The time one invalidation may still wait: a deadline on the fence clock, or a count of naps without one.
struct WaitBudget {
    end: Option<u64>,
    naps: u64,
}

impl WaitBudget {
    fn new() -> Self {
        WaitBudget { end: fence::now().map(|n| n.saturating_add(INVALIDATE_WAIT_NS)), naps: INVALIDATE_WAIT_NS / NAP_NS }
    }
    /// The next nap, None once the budget is spent.
    fn next_nap(&mut self) -> Option<u64> {
        match (self.end, fence::now()) {
            (Some(e), Some(n)) => (n < e).then(|| (e - n).min(NAP_NS)),
            _ => {
                self.naps = self.naps.checked_sub(1)?;
                Some(NAP_NS)
            }
        }
    }
}

/// `dma_resv_wait_timeout(BOOKKEEP, uninterruptible)`, bounded by `budget`: true when idle.
fn wait_idle(resv: &Resv, budget: &mut WaitBudget) -> bool {
    for f in resv.fences(Usage::Bookkeep) {
        let w = Waiter::new();
        let _reg = fence::add_callback(&f, w.clone());
        while !fence::is_signaled(&f) {
            let Some(nap) = budget.next_nap() else { return false };
            w.sleep(nap);
        }
    }
    true
}

// ---- the object's memory

/// A userptr object's memory: the range, its flags and the pages it got last (None before the first).
pub struct UserptrMem {
    mm: u64,
    addr: u64,
    size: u64,
    flags: u32,
    source: Arc<dyn UserPages>,
    resv: Arc<Resv>,
    pages: SpinLock<Option<Arc<dyn Exporter>>>,
    /// Counts invalidations; `valid` is the count the pages were got at (`mmu_interval_read_begin/retry`).
    seq: AtomicU64,
    valid: AtomicU64,
}

impl UserptrMem {
    pub fn addr(&self) -> u64 {
        self.addr
    }
    pub fn readonly(&self) -> bool {
        self.flags & AMDGPU_GEM_USERPTR_READONLY != 0
    }
    /// The pages are the ones of the address space now (no invalidation since they were got).
    pub fn current(&self) -> bool {
        self.pages.lock().is_some() && self.valid.load(Ordering::Acquire) == self.seq.load(Ordering::Acquire)
    }

    /// `amdgpu_ttm_tt_get_user_pages` + `_done`: the pages again unless they are current; EAGAIN when the range
    /// changed while they were got.
    fn get_pages(&self) -> Result<(), Errno> {
        let s = self.seq.load(Ordering::Acquire);
        if self.pages.lock().is_some() && self.valid.load(Ordering::Acquire) == s {
            return Ok(());
        }
        let p = self.source.get(self.mm, self.addr, self.size, self.readonly(), self.flags & AMDGPU_GEM_USERPTR_ANONONLY != 0)?;
        let old = {
            let mut g = self.pages.lock();
            if self.seq.load(Ordering::Acquire) != s {
                return Err(EAGAIN);
            }
            self.valid.store(s, Ordering::Release);
            g.replace(p)
        };
        drop(old);
        Ok(())
    }

    fn with<R>(&self, f: impl FnOnce(&dyn Exporter) -> R) -> Option<R> {
        let p = self.pages.lock().clone()?;
        Some(f(&*p))
    }
}

impl Drop for UserptrMem {
    fn drop(&mut self) {
        let key = (self.mm, self.addr, self as *const UserptrMem as usize);
        let mut g = REG.lock();
        if g.map.remove(&key).is_some() {
            g.size_gone(self.size);
        }
    }
}

impl Exporter for UserptrMem {
    fn name(&self) -> &'static str {
        "amdgpu"
    }
    fn size(&self) -> u64 {
        self.size
    }
    /// No CPU mapping through DRM (`amdgpu_gem_object_mmap`: EPERM; the program has the memory anyway).
    fn aperture(&self) -> Option<Aperture> {
        None
    }
    fn mmap_check(&self, _shared: bool) -> Result<(), Errno> {
        Err(EPERM)
    }
    fn page_phys(&self, page: u64) -> Option<u64> {
        self.with(|p| p.page_phys(page)).flatten()
    }
    /// The pages of now; a revalidation replaces them (`get_pages`), so a page table must hold these, not this.
    fn frames(&self) -> Option<Arc<dyn Exporter>> {
        self.pages.lock().clone()
    }
    fn read(&self, off: u64, out: &mut [u8]) -> bool {
        self.with(|p| p.read(off, out)).unwrap_or(false)
    }
    /// A read-only range takes no writes (Linux maps it without AMDGPU_PTE_WRITEABLE: the GPU's write faults).
    fn write(&self, off: u64, data: &[u8]) -> bool {
        !self.readonly() && self.with(|p| p.write(off, data)).unwrap_or(false)
    }
}

// ---- the ioctl and the checks other calls make

/// `amdgpu_gem_userptr_ioctl` (amdgpu_gem.c:378): Linux's argument checks in order (page offsets EINVAL, flags
/// EINVAL, writable without REGISTER EACCES), the object's own refusals (ENOSPC for zero pages or past the offset
/// range, ENOMEM for a page array over kvmalloc's limit), ENODEV without a source (Linux without HMM), with
/// VALIDATE the pages now (their errors; the object goes again), then a handle.
pub fn ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    let mut a = drm_amdgpu_gem_userptr::decode(data).ok_or(EINVAL)?;
    if (a.addr | a.size) & (PAGE - 1) != 0 {
        return Err(EINVAL);
    }
    // CachyOS 7.3 (not 6.6): `access_ok(addr, size)` right after the alignment, before the flags: a range that
    // wraps or ends above the user address space is EFAULT (checked against CachyOS with abi-drm-list).
    if a.addr.checked_add(a.size).is_none_or(|e| e > hal::USER_TOP) {
        return Err(EFAULT);
    }
    let known = AMDGPU_GEM_USERPTR_READONLY | AMDGPU_GEM_USERPTR_ANONONLY | AMDGPU_GEM_USERPTR_VALIDATE | AMDGPU_GEM_USERPTR_REGISTER;
    if a.flags & !known != 0 {
        return Err(EINVAL);
    }
    if a.flags & (AMDGPU_GEM_USERPTR_READONLY | AMDGPU_GEM_USERPTR_REGISTER) == 0 {
        return Err(EACCES);
    }
    if a.size == 0 {
        return Err(ENOSPC);
    }
    let gpu = ctx.dev.driver().gpu().ok_or(ENODEV)?;
    let gem = &gpu.shared().gem;
    // The object first (amdgpu_gem_object_create): its mmap offset range (ENOSPC), then ttm_sg_tt_init's page
    // array, one page pointer and one DMA address per page in one kvcalloc (ENOMEM over INT_MAX bytes).
    let pages = a.size / PAGE;
    if pages > crate::gem::MMAP_PAGES {
        return Err(ENOSPC);
    }
    if pages * 16 > i32::MAX as u64 {
        return Err(ENOMEM);
    }
    let src = source(ctx);
    let Some((src, mm)) = src.and_then(|s| s.current_mm().map(|m| (s, m))) else {
        gem.note_userptr_refused(a.size, a.flags);
        return Err(ENODEV);
    };
    let resv = Resv::new();
    let mem = Arc::new(UserptrMem {
        mm,
        addr: a.addr,
        size: a.size,
        flags: a.flags,
        source: src,
        resv: resv.clone(),
        pages: SpinLock::named("drm.userptr.pages", None),
        seq: AtomicU64::new(1),
        valid: AtomicU64::new(0),
    });
    let bo = crate::gem::userptr_bo(gpu, a.size, resv, mem.clone())?;
    {
        let mut g = REG.lock();
        if g.map.insert((mm, a.addr, Arc::as_ptr(&mem) as usize), (a.size, Arc::downgrade(&mem))).is_none() {
            *g.sizes.entry(a.size).or_insert(0) += 1;
        }
    }
    if a.flags & AMDGPU_GEM_USERPTR_VALIDATE != 0
        && let Err(e) = mem.get_pages()
    {
        klog::event!(Debug, "drm", "gem.userptr_validate_failed", addr = klog::Hex(a.addr), size = a.size, errno = e);
        return Err(e);
    }
    a.handle = ctx.file.handle_create(bo)?;
    klog::event!(Debug, "drm", "gem.userptr_created", handle = a.handle, size = a.size, flags = klog::Hex(a.flags as u64));
    a.encode(data).ok_or(EINVAL)?;
    Ok(0)
}

/// `amdgpu_ttm_tt_get_usermm(..) != current->mm` (bo_list_create, gem_object_open, cs_parser_bos): EPERM for a
/// userptr object of another address space (a forked child with its parent's fd); Ok for every other object.
pub(crate) fn check_mm(ctx: &Ctx, bo: &Bo) -> Result<(), Errno> {
    let Some(m) = bo.userptr() else { return Ok(()) };
    if m.source.current_mm() != Some(m.mm) {
        klog::event!(Debug, "drm", "gem.userptr_other_mm", addr = klog::Hex(m.addr); "EPERM");
        return Err(EPERM);
    }
    let _ = ctx;
    Ok(())
}

/// A CS lists the object: its pages again if they went stale (EFAULT, EPERM, ESRCH from the source; EAGAIN when
/// they changed meanwhile: libdrm's callers submit again).
pub(crate) fn revalidate(ctx: &Ctx, bo: &Bo) -> Result<(), Errno> {
    let Some(m) = bo.userptr() else { return Ok(()) };
    let _ = ctx;
    m.get_pages().inspect_err(|&e| {
        klog::event!(Debug, "drm", "cs.userptr_pages_failed", addr = klog::Hex(m.addr), size = m.size, errno = e);
    })
}

// ---- a fake address space (host tests, the fake GPU's ktests)

/// One page of a fake address space.
#[derive(Clone)]
struct FakePage {
    data: Arc<SpinLock<alloc::vec::Vec<u8>>>,
    writable: bool,
    file: bool,
}

/// Address spaces a test maps and unmaps by hand: `unmap` calls `invalidate` as MM will. The caller's address
/// space is `set_current`'s (a forked child is another number).
pub struct FakeUserPages {
    current: AtomicU64,
    pages: SpinLock<BTreeMap<(u64, u64), FakePage>>,
}

impl FakeUserPages {
    pub fn new(mm: u64) -> Arc<FakeUserPages> {
        Arc::new(FakeUserPages { current: AtomicU64::new(mm), pages: SpinLock::named("drm.userptr.fake", BTreeMap::new()) })
    }
    pub fn set_current(&self, mm: u64) {
        self.current.store(mm, Ordering::Release);
    }
    /// Fresh zero pages at `addr` (a file mapping with `file`).
    pub fn map(&self, mm: u64, addr: u64, pages: u64, writable: bool, file: bool) {
        let mut g = self.pages.lock();
        for i in 0..pages {
            let data = Arc::new(SpinLock::named("drm.userptr.fake_page", alloc::vec![0u8; PAGE as usize]));
            g.insert((mm, addr + i * PAGE), FakePage { data, writable, file });
        }
    }
    /// munmap: the pages go, then the address space tells the objects over them (returns how many).
    pub fn unmap(&self, mm: u64, addr: u64, pages: u64) -> usize {
        let gone: alloc::vec::Vec<FakePage> = {
            let mut g = self.pages.lock();
            (0..pages).filter_map(|i| g.remove(&(mm, addr + i * PAGE))).collect()
        };
        let n = invalidate(mm, addr, addr + pages * PAGE);
        drop(gone);
        n
    }
    /// The program's own access to its memory (false on a hole).
    pub fn read(&self, mm: u64, addr: u64, out: &mut [u8]) -> bool {
        self.each(mm, addr, out.len(), |d, k| out[k..k + d.len()].copy_from_slice(d))
    }
    pub fn write(&self, mm: u64, addr: u64, data: &[u8]) -> bool {
        self.each(mm, addr, data.len(), |d, k| d.copy_from_slice(&data[k..k + d.len()]))
    }
    fn each(&self, mm: u64, addr: u64, n: usize, mut f: impl FnMut(&mut [u8], usize)) -> bool {
        let mut k = 0;
        while k < n {
            let at = addr + k as u64;
            let o = (at % PAGE) as usize;
            let c = (n - k).min(PAGE as usize - o);
            let Some(p) = self.pages.lock().get(&(mm, at - o as u64)).cloned() else { return false };
            f(&mut p.data.lock()[o..o + c], k);
            k += c;
        }
        true
    }
}

impl UserPages for FakeUserPages {
    fn current_mm(&self) -> Option<u64> {
        Some(self.current.load(Ordering::Acquire))
    }
    fn get(&self, mm: u64, addr: u64, size: u64, readonly: bool, anon_only: bool) -> Result<Arc<dyn Exporter>, Errno> {
        let g = self.pages.lock();
        let n = size / PAGE;
        let mut v = alloc::vec::Vec::new();
        v.try_reserve_exact(n as usize).map_err(|_| ENOMEM)?;
        for i in 0..n {
            let p = g.get(&(mm, addr + i * PAGE)).ok_or(EFAULT)?;
            if i == 0 && anon_only && p.file {
                return Err(EPERM);
            }
            if !readonly && !p.writable {
                return Err(EFAULT);
            }
            v.push(p.clone());
        }
        Ok(Arc::new(FakeRange { pages: v }))
    }
}

/// Pages a fake address space handed out (they stay the object's after an unmap, as pinned frames would).
struct FakeRange {
    pages: alloc::vec::Vec<FakePage>,
}

impl FakeRange {
    fn each(&self, off: u64, n: usize, mut f: impl FnMut(&mut [u8], usize)) -> bool {
        if off.checked_add(n as u64).is_none_or(|e| e > self.pages.len() as u64 * PAGE) {
            return false;
        }
        let mut k = 0;
        while k < n {
            let at = off + k as u64;
            let o = (at % PAGE) as usize;
            let c = (n - k).min(PAGE as usize - o);
            let mut d = self.pages[(at / PAGE) as usize].data.lock();
            f(&mut d[o..o + c], k);
            k += c;
        }
        true
    }
}

impl Exporter for FakeRange {
    fn name(&self) -> &'static str {
        "userptr-fake"
    }
    fn size(&self) -> u64 {
        self.pages.len() as u64 * PAGE
    }
    fn aperture(&self) -> Option<Aperture> {
        None
    }
    fn page_phys(&self, _page: u64) -> Option<u64> {
        None
    }
    fn read(&self, off: u64, out: &mut [u8]) -> bool {
        self.each(off, out.len(), |d, k| out[k..k + d.len()].copy_from_slice(d))
    }
    fn write(&self, off: u64, data: &[u8]) -> bool {
        self.each(off, data.len(), |d, k| d.copy_from_slice(&data[k..k + d.len()]))
    }
}
