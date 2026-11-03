//! Pinned scattered DMA pages (MM-14, GX roadmap wave 23; scout gx-scout-vm-rings.md section 8 items 1 and 5):
//! the system memory of GPU buffers (GTT objects, rings, page tables, dma-bufs, udmabuf).
//!
//! A `DmaPages` holds N frames that need not be contiguous (blocks of up to 2 MiB, largest first, as Linux's TTM
//! pool), zeroed, pinned for its whole life: never reclaimed, moved, merged into huge pages, copied on fork or
//! swapped (V2 has no swap; reclaim only shrinks the page cache; THP only touches `Kind::User` frames). The
//! device gets their addresses per page or per block (`device_addr`, `segment`): physical while the IOMMU is off
//! (V2 keeps it off); `device_addr_of` is the one place an IOMMU mapping would go. The CPU type is per object:
//! write-back (cached, the device snoops: x86 PCIe default) unless the driver asks for WC or UC, which retypes
//! the direct map of each block (Linux `set_pages_array_wc`, linux-6.6/arch/x86/mm/pat/set_memory.c:2258).
//!
//! Lifetime: the object is shared as `Arc<DmaPages>`. Programs map it through a device VMA
//! (`device::Aperture::Pages`; the map keeps an `Arc`), a driver that gave the device the addresses holds a
//! `DevicePin` (another `Arc` plus a pin count) until the device can no longer reach them. The frames go back on
//! the last drop: last close of the handles, last unmap (also at the end of the program) and last pin. A
//! mapping whose TLB shootdown a CPU did not confirm keeps the object until its address space drops (aspace),
//! and for good when that address space is leaked. The last drop belongs in thread context (see `DevicePin`);
//! under the address-space mutex "mm.as" is fine (it sleeps, its waiters answer shootdowns).
//!
//! Limits (Georg's rule: dynamic, no fixed number): all pinned pages together may take `share` percent of the
//! managed RAM (default 50 %, Linux's TTM default `ttm_pages_limit` = half of RAM,
//! linux-6.6/drivers/gpu/drm/ttm/ttm_device.c:87-88, which amdgpu uses as GTT size, amdgpu_ttm.c:1926), measured
//! again when a request is refused (managed memory grows after boot). A `PinOwner` (a GPU client, a udmabuf
//! creator) can bind its own share on top, so one program cannot pin all of it. Every pinned page is charged
//! to the strict commit as well (V2-PLAN R2): it is RAM the system can no longer promise to programs. A refusal
//! is ENOMEM (Linux `ttm_global_swapout` / `account_locked_vm`, mm/util.c:517).
//!
//! Counters: `stats()` (pinned pages, limit, objects, device pins) for meminfo-like reports; the `mm.dmapages.*`
//! metrics; per program the device pages mapped show as device memory in its stats (aspace `Class::Device`).

use crate::commit::{self, Budget};
use crate::dma::DmaCache;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use hal::dma_buf::DmaRegion;
use hal::paging::CacheType;
use mm_core::dma::DmaAllocError;
use mm_core::dma_pages::PageRuns;
use mm_core::{Zone, PAGE};

klog::counter!(ALLOCS, "mm.dmapages.allocs");
klog::counter!(FAILS, "mm.dmapages.failures");
klog::counter!(PAGES_IN, "mm.dmapages.pages_allocated");
klog::counter!(PAGES_OUT, "mm.dmapages.pages_freed");
klog::counter!(RETYPED, "mm.dmapages.retyped_pages");
klog::counter!(LIMIT_REFUSED, "mm.dmapages.limit_refused");
klog::counter!(PINS, "mm.dmapages.device_pins");

/// Default share of the managed RAM all pinned pages may take, in percent (TTM's half of RAM).
pub const DEFAULT_SHARE: u64 = 50;

/// Pinned pages system-wide.
static PINNED: Budget = Budget::new();
static SHARE: AtomicU64 = AtomicU64::new(DEFAULT_SHARE);
/// Managed RAM in bytes last measured (0: not yet).
static TOTAL: AtomicU64 = AtomicU64::new(0);
/// A limit in pages a test set (0: none); wins over the share.
static TEST_LIMIT: AtomicU64 = AtomicU64::new(0);
static OBJECTS: AtomicU64 = AtomicU64::new(0);
static LIVE_PINS: AtomicU64 = AtomicU64::new(0);
static LIVE_RETYPED: AtomicU64 = AtomicU64::new(0);

/// The pin limit in pages for `total` bytes of managed memory and `share` percent (pure).
pub fn limit_for(total: u64, share: u64) -> u64 {
    // total / PAGE < 2^40, so the product cannot overflow for a share up to 100.
    (total / PAGE) * share.min(100) / 100
}

fn measure() -> u64 {
    let t = mm_core::frames().map(|f| f.stats().total_bytes).unwrap_or(0);
    if t > 0 {
        TOTAL.store(t, Ordering::Relaxed);
    }
    t
}

fn total() -> u64 {
    match TOTAL.load(Ordering::Relaxed) {
        0 => measure(),
        t => t,
    }
}

/// The system's pin limit in pages (u64::MAX without a frame allocator: host tests).
pub fn limit() -> u64 {
    match TEST_LIMIT.load(Ordering::Relaxed) {
        0 => match total() {
            0 => u64::MAX,
            t => limit_for(t, SHARE.load(Ordering::Relaxed)),
        },
        l => l,
    }
}

/// Sets the system's share in percent (1..=100; a later CONFIG key). False: out of range, nothing changed.
pub fn set_share(percent: u64) -> bool {
    if !(1..=100).contains(&percent) {
        return false;
    }
    SHARE.store(percent, Ordering::Relaxed);
    klog::event!(Info, "mm", "dmapages.share", percent = percent, limit_pages = limit(); "pin limit for DMA pages changed");
    true
}

/// Tests (QemuOnly): a pin limit in pages, None = the share again.
pub fn set_test_limit(pages: Option<u64>) {
    TEST_LIMIT.store(pages.unwrap_or(0), Ordering::Relaxed);
}

/// Tests (QemuOnly): the next allocations fail after this many pages (the out-of-frames path), None = off.
pub fn test_fail_after(pages: Option<u64>) {
    mm_core::dma_pages::test_fail_after(pages);
}

/// The numbers of every pinned DMA page now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PinStats {
    pub pinned_pages: u64,
    pub limit_pages: u64,
    pub objects: u64,
    pub device_pins: u64,
    /// Pages of live objects with a WC/UC direct map.
    pub retyped_pages: u64,
    /// Pages of WC/UC objects whose last drop came with interrupts off, kept out of use for good (should be 0).
    pub kept_irq_off: u64,
}

pub fn stats() -> PinStats {
    PinStats {
        pinned_pages: PINNED.committed(),
        limit_pages: limit(),
        objects: OBJECTS.load(Ordering::Relaxed),
        device_pins: LIVE_PINS.load(Ordering::Relaxed),
        retyped_pages: LIVE_RETYPED.load(Ordering::Relaxed),
        kept_irq_off: mm_core::dma_pages::KEPT_IRQ_OFF.load(Ordering::Relaxed),
    }
}

/// Charges `n` pages to the system's pin budget (all or nothing; measured once more before a refusal).
fn charge_system(n: u64) -> bool {
    if PINNED.charge(n, limit()) {
        return true;
    }
    TEST_LIMIT.load(Ordering::Relaxed) == 0 && measure() > 0 && PINNED.charge(n, limit())
}

/// A party that pins pages (a GPU client, a udmabuf creator) with its own share of the managed RAM on top of
/// the system's limit.
pub struct PinOwner {
    pinned: Budget,
    share: AtomicU64,
}

impl PinOwner {
    /// An owner that may pin `share` percent (1..=100, clamped) of the managed RAM.
    pub fn new(share: u64) -> Arc<PinOwner> {
        Arc::new(PinOwner { pinned: Budget::new(), share: AtomicU64::new(share.clamp(1, 100)) })
    }
    pub fn pinned(&self) -> u64 {
        self.pinned.committed()
    }
    /// The owner's limit in pages now (u64::MAX without a frame allocator).
    pub fn limit(&self) -> u64 {
        match total() {
            0 => u64::MAX,
            t => limit_for(t, self.share.load(Ordering::Relaxed)),
        }
    }
    pub fn set_share(&self, share: u64) {
        self.share.store(share.clamp(1, 100), Ordering::Relaxed);
    }
    fn charge(&self, n: u64) -> bool {
        self.pinned.charge(n, self.limit()) || (measure() > 0 && self.pinned.charge(n, self.limit()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaPagesError {
    /// Zero pages, or more than any machine has.
    Invalid,
    /// The system's pin limit is reached.
    PinLimit,
    /// The owner's own share is used up.
    OwnerLimit,
    /// The strict commit has no room for the pages.
    Commit,
    /// No frames.
    NoMemory,
    /// The direct map could not take the WC/UC type.
    Unsupported,
}

impl DmaPagesError {
    /// The positive errno a syscall answers (Linux: ENOMEM for every shortage, EINVAL for a bad size).
    pub fn errno(self) -> i64 {
        match self {
            DmaPagesError::Invalid => uapi::errno::EINVAL,
            _ => uapi::errno::ENOMEM,
        }
    }
}

/// How the pages are allocated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaPagesOptions {
    pub cache: DmaCache,
    /// Only below 4 GiB (devices with 32-bit addresses).
    pub dma32: bool,
    /// Largest block in frames as an order (9 = 2 MiB, the default; 0 = single frames only).
    pub max_order: u32,
}

impl Default for DmaPagesOptions {
    fn default() -> Self {
        DmaPagesOptions { cache: DmaCache::WriteBack, dma32: false, max_order: mm_core::HUGE_ORDER }
    }
}

impl DmaPagesOptions {
    pub fn with_cache(self, cache: DmaCache) -> Self {
        DmaPagesOptions { cache, ..self }
    }
    pub fn dma32(self) -> Self {
        DmaPagesOptions { dma32: true, ..self }
    }
    pub fn with_max_order(self, max_order: u32) -> Self {
        DmaPagesOptions { max_order, ..self }
    }
}

fn cache_type(c: DmaCache) -> CacheType {
    match c {
        DmaCache::WriteBack => CacheType::WriteBack,
        DmaCache::Uncached => CacheType::Uncached,
        DmaCache::WriteCombining => CacheType::WriteCombining,
    }
}

/// The address the device uses for physical address `phys`: equal while the IOMMU is off (the hook for an
/// IOMMU mapping later).
fn device_addr_of(phys: u64) -> u64 {
    phys
}

/// A pinned set of scattered DMA pages (see the module docs).
pub struct DmaPages {
    /// Taken out at the drop so the frames go back before the charges do.
    runs: Option<PageRuns>,
    pages: u64,
    cache: DmaCache,
    owner: Option<Arc<PinOwner>>,
    pins: AtomicU64,
}

impl core::fmt::Debug for DmaPages {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "DmaPages({} pages in {} blocks, {:?})", self.pages, self.segments(), self.cache)
    }
}

impl DmaPages {
    /// `pages` zeroed, pinned pages with options `o`. Thread context only.
    pub fn alloc(pages: u64, o: DmaPagesOptions) -> Result<Arc<DmaPages>, DmaPagesError> {
        Self::alloc_for(pages, o, None)
    }

    /// As `alloc`, charged to `owner`'s share too.
    pub fn alloc_for(pages: u64, o: DmaPagesOptions, owner: Option<&Arc<PinOwner>>) -> Result<Arc<DmaPages>, DmaPagesError> {
        let r = Self::charge_and_take(pages, o, owner);
        match &r {
            Ok(p) => {
                ALLOCS.inc();
                PAGES_IN.add(pages);
                OBJECTS.fetch_add(1, Ordering::Relaxed);
                if let Some(st) = p.runs.as_ref().map(|r| r.retype_stats()).filter(|st| st.pages > 0) {
                    RETYPED.add(st.pages);
                    LIVE_RETYPED.fetch_add(pages, Ordering::Relaxed);
                    let cache = alloc::format!("{:?}", o.cache);
                    if st.split_1g + st.split_2m > 0 {
                        klog::event!(Info, "mm", "dmapages.retyped", pages = st.pages, blocks = p.segments(), cache = cache,
                            split_1g = st.split_1g, split_2m = st.split_2m; "direct-map pages of DMA pages retyped, large pages split");
                    } else {
                        klog::event!(Debug, "mm", "dmapages.retyped", pages = st.pages, blocks = p.segments(), cache = cache,
                            split_1g = 0, split_2m = 0; "direct-map pages of DMA pages retyped");
                    }
                }
            }
            Err(e) => {
                FAILS.inc();
                if matches!(e, DmaPagesError::PinLimit | DmaPagesError::OwnerLimit) {
                    LIMIT_REFUSED.inc();
                }
                klog::event!(Warn, "mm", "dmapages.alloc_failed", pages = pages, cache = alloc::format!("{:?}", o.cache), dma32 = o.dma32,
                    error = alloc::format!("{e:?}"), pinned = PINNED.committed(), limit = limit(); "DMA pages refused");
            }
        }
        r
    }

    fn charge_and_take(pages: u64, o: DmaPagesOptions, owner: Option<&Arc<PinOwner>>) -> Result<Arc<DmaPages>, DmaPagesError> {
        if pages == 0 || pages > (1 << 52) / PAGE {
            return Err(DmaPagesError::Invalid);
        }
        if !charge_system(pages) {
            return Err(DmaPagesError::PinLimit);
        }
        if let Some(w) = owner {
            if !w.charge(pages) {
                PINNED.uncharge(pages);
                return Err(DmaPagesError::OwnerLimit);
            }
        }
        let undo = |owner: Option<&Arc<PinOwner>>| {
            PINNED.uncharge(pages);
            if let Some(w) = owner {
                w.pinned.uncharge(pages);
            }
        };
        if !commit::charge(pages) {
            undo(owner);
            return Err(DmaPagesError::Commit);
        }
        let zone = if o.dma32 { Zone::Dma32 } else { Zone::Normal };
        let runs = mm_core::dma_pages::alloc_runs(pages, o.max_order, zone, cache_type(o.cache)).map_err(|e| {
            commit::uncharge(pages);
            undo(owner);
            match e {
                DmaAllocError::BadRequest => DmaPagesError::Invalid,
                DmaAllocError::NoMemory => DmaPagesError::NoMemory,
                DmaAllocError::Retype(hal::paging::MapError::NoMemory) => DmaPagesError::NoMemory,
                DmaAllocError::Retype(_) => DmaPagesError::Unsupported,
            }
        })?;
        let me = DmaPages { runs: Some(runs), pages, cache: o.cache, owner: owner.cloned(), pins: AtomicU64::new(0) };
        Ok(Arc::new(me))
    }

    fn runs(&self) -> Option<&PageRuns> {
        self.runs.as_ref()
    }
    pub fn pages(&self) -> u64 {
        self.pages
    }
    pub fn len(&self) -> u64 {
        self.pages * PAGE
    }
    pub fn is_empty(&self) -> bool {
        self.pages == 0
    }
    pub fn cache(&self) -> DmaCache {
        self.cache
    }
    pub(crate) fn cache_type(&self) -> CacheType {
        cache_type(self.cache)
    }
    /// How many contiguous blocks (scatter-gather entries) the pages form.
    pub fn segments(&self) -> usize {
        self.runs().map_or(0, |r| r.runs())
    }
    /// Block `i` as the device sees it: (device address, length in bytes); in page order.
    pub fn segment(&self, i: usize) -> Option<(u64, u64)> {
        self.runs()?.run(i).map(|(p, n)| (device_addr_of(p), n * PAGE))
    }
    /// The device address of page `page`.
    pub fn device_addr(&self, page: u64) -> Option<u64> {
        self.page_phys(page).map(device_addr_of)
    }
    /// The physical address of page `page` (equal to `device_addr` while the IOMMU is off).
    pub fn page_phys(&self, page: u64) -> Option<u64> {
        self.runs()?.page_phys(page)
    }
    /// Device pins held now (`pin_for_device`).
    pub fn device_pins(&self) -> u64 {
        self.pins.load(Ordering::Relaxed)
    }

    /// Records that a device was given the addresses: the pages stay until the pin drops, whatever else goes
    /// (handles, mappings, the program).
    pub fn pin_for_device(self: &Arc<Self>) -> DevicePin {
        self.pins.fetch_add(1, Ordering::Relaxed);
        LIVE_PINS.fetch_add(1, Ordering::Relaxed);
        PINS.inc();
        DevicePin { pages: self.clone() }
    }

    /// The CPU view of byte `off` and the offset inside it.
    fn at(&self, off: u64) -> Option<(&DmaRegion, usize)> {
        self.runs()?.locate(off)
    }

    /// Byte copies (bounds-checked over the whole object, across blocks; false = nothing done).
    pub fn read(&self, off: u64, out: &mut [u8]) -> bool {
        self.chunks(off, out.len(), |r, o, at, n| r.copy_out(o, &mut out[at..at + n]))
    }
    pub fn write(&self, off: u64, data: &[u8]) -> bool {
        self.chunks(off, data.len(), |r, o, at, n| r.copy_in(o, &data[at..at + n]))
    }
    pub fn fill(&self, off: u64, len: usize, byte: u8) -> bool {
        self.chunks(off, len, |r, o, _, n| r.fill(o, n, byte))
    }

    /// Calls `f(region, offset in it, offset in the caller's buffer, bytes)` for each block piece of
    /// [off, off + len), after checking the whole range lies inside the object.
    fn chunks(&self, off: u64, len: usize, mut f: impl FnMut(&DmaRegion, usize, usize, usize) -> bool) -> bool {
        if off.checked_add(len as u64).is_none_or(|e| e > self.len()) {
            return false;
        }
        let mut done = 0usize;
        while done < len {
            let Some((r, o)) = self.at(off + done as u64) else { return false };
            let n = (r.len() - o).min(len - done);
            if !f(r, o, done, n) {
                return false;
            }
            done += n;
        }
        true
    }

    /// One naturally aligned volatile access (never crosses a page, so never a block); None/false outside.
    pub fn read_u32(&self, off: u64) -> Option<u32> {
        self.at(off).and_then(|(r, o)| r.read_u32(o))
    }
    pub fn write_u32(&self, off: u64, v: u32) -> bool {
        self.at(off).is_some_and(|(r, o)| r.write_u32(o, v))
    }
    pub fn read_u64(&self, off: u64) -> Option<u64> {
        self.at(off).and_then(|(r, o)| r.read_u64(o))
    }
    pub fn write_u64(&self, off: u64, v: u64) -> bool {
        self.at(off).is_some_and(|(r, o)| r.write_u64(o, v))
    }
    /// Makes earlier CPU writes visible to the device (drains WC buffers) before a doorbell.
    pub fn write_barrier(&self) {
        if let Some(r) = self.runs() {
            r.write_barrier();
        }
    }
}

impl Drop for DmaPages {
    fn drop(&mut self) {
        let retyped = self.runs().is_some_and(|r| r.retype_stats().pages > 0);
        // Frames first (back to WB, to the allocator), then the charges: the commit never counts frames free
        // that are still taken.
        drop(self.runs.take());
        PAGES_OUT.add(self.pages);
        OBJECTS.fetch_sub(1, Ordering::Relaxed);
        if retyped {
            LIVE_RETYPED.fetch_sub(self.pages, Ordering::Relaxed);
        }
        commit::uncharge(self.pages);
        if !PINNED.uncharge(self.pages) {
            klog::event!(Error, "mm", "dmapages.underflow", pages = self.pages; "pin budget went below zero (bug in MM)");
        }
        if let Some(w) = &self.owner {
            w.pinned.uncharge(self.pages);
        }
    }
}

/// A device's hold on a `DmaPages` (see `DmaPages::pin_for_device`). Drop it only once the device can no longer
/// reach the pages (its fence passed, or it was stopped), and in thread context: the fence interrupt wakes a
/// thread that drops it (ksync's interrupt rule). The last drop of a WC/UC object retypes its direct map back
/// (TLB shootdowns, table frames); with interrupts off its frames are kept out of use for good instead
/// (`PinStats::kept_irq_off`). A WB object's last drop only frees frames.
pub struct DevicePin {
    pages: Arc<DmaPages>,
}

impl DevicePin {
    pub fn pages(&self) -> &Arc<DmaPages> {
        &self.pages
    }
}

impl Drop for DevicePin {
    fn drop(&mut self) {
        self.pages.pins.fetch_sub(1, Ordering::Relaxed);
        LIVE_PINS.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn limit_is_a_share_of_ram() {
        assert_eq!(limit_for(32 * GIB, 50), 16 * GIB / PAGE, "TTM's half of RAM");
        assert_eq!(limit_for(512 << 20, 50), (256 << 20) / PAGE);
        assert_eq!(limit_for(32 * GIB, 250), 32 * GIB / PAGE, "share capped at 100 %");
        assert_eq!(limit_for(1 << 52, 100), (1 << 52) / PAGE, "no overflow at the largest physical space");
        assert_eq!(limit_for(0, 50), 0);
    }

    #[test]
    fn owner_budget_is_all_or_nothing_and_balances() {
        // No frame allocator on the host: the owner's limit is open, the budget still counts exactly.
        let w = PinOwner::new(10);
        assert_eq!(w.limit(), u64::MAX);
        assert!(w.charge(100) && w.charge(28));
        assert_eq!(w.pinned(), 128);
        assert!(w.pinned.uncharge(128));
        assert!(!w.pinned.uncharge(1), "an underflow is reported");
        assert_eq!(w.pinned(), 0);
    }

    #[test]
    fn errors_map_to_linux_errno() {
        assert_eq!(DmaPagesError::Invalid.errno(), uapi::errno::EINVAL);
        for e in [DmaPagesError::PinLimit, DmaPagesError::OwnerLimit, DmaPagesError::Commit, DmaPagesError::NoMemory, DmaPagesError::Unsupported] {
            assert_eq!(e.errno(), uapi::errno::ENOMEM);
        }
    }

    #[test]
    fn host_alloc_has_no_frames() {
        // Without a frame allocator nothing is taken and every charge is undone.
        let before = (PINNED.committed(), commit::committed());
        assert_eq!(DmaPages::alloc(0, DmaPagesOptions::default()).err(), Some(DmaPagesError::Invalid));
        assert_eq!(DmaPages::alloc(4, DmaPagesOptions::default()).err(), Some(DmaPagesError::NoMemory));
        assert_eq!((PINNED.committed(), commit::committed()), before);
    }
}
