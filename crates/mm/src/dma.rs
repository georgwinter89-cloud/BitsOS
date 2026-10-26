//! DMA buffers for drivers (owner: MM; contract `C: DmaPool`, V2-PLAN §2.5, desk decision 6a).
//!
//! A `DmaBuffer` is physically contiguous (exactly the pages it needs, no power-of-two rounding), zeroed, and
//! has a device address (= physical, IOMMU off) and a CPU view: the `hal::dma_buf::DmaRegion` it owns, which
//! only mm-core's DMA allocation can build. Drivers touch the memory only through its accessors (bounds-checked,
//! one aligned access per typed value, `rep movsb` copies) and never derive a CPU address from
//! `device_addr()`. Constraints per device (`DmaConstraints`):
//! - `limit`: highest device address + 1 (`dma32()` = 4 GiB for the RTL8852BE, `dma64()` = no limit, any
//!   other mask through `from_mask_bits`); 64-bit buffers come from NORMAL first so DMA32 stays for the
//!   devices that need it.
//! - `align`: power of two, at least a page.
//! - `boundary`: power of two the buffer never crosses (xHCI 64 KiB); a buffer longer than it is refused.
//! - `cache`: WriteBack (coherent, x86 PCIe snoops), Uncached or WriteCombining (no-snoop devices like HDA).
//!   UC/WC retype the buffer's direct-map pages (Linux `set_memory_uc/wc`): RAM gets one memory type only.
//!   With several CPUs the retype shoots the old type down on every CPU (MM-6). Allocating and dropping a
//!   UC/WC buffer is thread context only (the retype runs with interrupts off and takes frames).
//!
//! Freed frames are reused at once: a driver must stop the device (reset it or clear bus master, and wait for
//! it to settle) before it drops a `DmaBuffer` the device was ever given.

use alloc::vec::Vec;
use hal::dma_buf::DmaRegion;
use hal::paging::{CacheType, MapError};
use mm_core::dma::{DmaAllocError, DmaBlock};

klog::counter!(DMA_ALLOCS, "mm.dma.allocs");
klog::counter!(DMA_FAILS, "mm.dma.failures");
klog::counter!(DMA_RETYPED, "mm.dma.retyped_pages");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaCache {
    WriteBack,
    Uncached,
    WriteCombining,
}

impl DmaCache {
    fn cache_type(self) -> CacheType {
        match self {
            DmaCache::WriteBack => CacheType::WriteBack,
            DmaCache::Uncached => CacheType::Uncached,
            DmaCache::WriteCombining => CacheType::WriteCombining,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaConstraints {
    /// Highest usable device address + 1 (e.g. 1 << 32); `u64::MAX` = no limit.
    pub limit: u64,
    /// Alignment of the start in bytes (a power of two; 0 or less than a page = a page).
    pub align: u64,
    /// The buffer must not cross a multiple of this (a power of two; 0 = no boundary).
    pub boundary: u64,
    pub cache: DmaCache,
}

impl Default for DmaConstraints {
    fn default() -> Self {
        Self::dma32()
    }
}

impl DmaConstraints {
    /// Below 4 GiB, page-aligned, no boundary, write-back (the G0 default).
    pub const fn dma32() -> Self {
        DmaConstraints { limit: 1 << 32, align: 4096, boundary: 0, cache: DmaCache::WriteBack }
    }
    /// Anywhere (64-bit DMA), page-aligned, no boundary, write-back.
    pub const fn dma64() -> Self {
        DmaConstraints { limit: u64::MAX, align: 4096, boundary: 0, cache: DmaCache::WriteBack }
    }
    /// The device drives `bits` address lines (Linux `DMA_BIT_MASK(bits)`); 64 or more = no limit.
    pub const fn from_mask_bits(bits: u32) -> Self {
        let limit = if bits >= 64 { u64::MAX } else { 1u64 << bits };
        DmaConstraints { limit, ..Self::dma64() }
    }
    pub const fn with_align(mut self, align: u64) -> Self {
        self.align = align;
        self
    }
    pub const fn with_boundary(mut self, boundary: u64) -> Self {
        self.boundary = boundary;
        self
    }
    pub const fn with_cache(mut self, cache: DmaCache) -> Self {
        self.cache = cache;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaError {
    /// No contiguous run of that size below the limit, or no frame for a page table a UC/WC retype needs
    /// (transient: worth a retry later).
    NoMemory,
    /// The memory type cannot be set now (before the kernel's own page tables, with a PAT that is not Linux's
    /// layout, or a CPU did not confirm the retype's TLB shootdown: those frames stay out of use).
    Unsupported,
    /// Constraints that can never be met: zero length, alignment or boundary not a power of two, a buffer
    /// longer than its boundary, a limit below the buffer's size. Also an internal fault of the UC/WC retype
    /// (logged at Error as `dma.retype_fault`).
    Invalid,
    /// Longer than the largest contiguous block (1 GiB) or aligned beyond it.
    TooLarge,
}

/// Largest contiguous buffer (and alignment): one buddy block of the top order.
pub const MAX_BYTES: u64 = mm_core::PAGE << mm_core::MAX_ORDER;

/// Checks a request and returns the alignment to allocate with (bytes, a power of two >= a page). Why a
/// boundary needs nothing more: the run starts at a buddy block of size B >= len aligned to B; if B <=
/// boundary the block lies inside one boundary window, otherwise the start is a multiple of the boundary
/// and len <= boundary.
pub fn plan(len: usize, c: &DmaConstraints) -> Result<u64, DmaError> {
    if len == 0 {
        return Err(DmaError::Invalid);
    }
    if len as u64 > MAX_BYTES {
        return Err(DmaError::TooLarge);
    }
    let align = c.align.max(mm_core::PAGE);
    if !align.is_power_of_two() {
        return Err(DmaError::Invalid);
    }
    if align > MAX_BYTES {
        return Err(DmaError::TooLarge);
    }
    if c.boundary != 0 && (!c.boundary.is_power_of_two() || c.boundary < len as u64) {
        return Err(DmaError::Invalid);
    }
    if c.limit < (len as u64).div_ceil(mm_core::PAGE) * mm_core::PAGE {
        return Err(DmaError::Invalid);
    }
    Ok(align)
}

/// What a driver sees when the direct map could not take a UC/WC type: no table frame is a transient shortage
/// like any other; a refusal that holds until something changes (a CPU that did not answer the shootdown, no
/// kernel tables, wrong PAT) is `Unsupported`; anything else is an internal fault (the frames came from the buddy, so they are
/// aligned, mapped RAM).
fn retype_error(e: MapError) -> DmaError {
    match e {
        MapError::NoMemory => DmaError::NoMemory,
        MapError::NeedsShootdown | MapError::WrongRange | MapError::Unsupported => DmaError::Unsupported,
        _ => DmaError::Invalid,
    }
}

/// An owned DMA buffer. Dropping it returns the frames to the allocator, which hands them out again at once:
/// drop it only after the device can no longer access it (see the module docs).
pub struct DmaBuffer {
    block: DmaBlock,
    constraints: DmaConstraints,
}

impl core::fmt::Debug for DmaBuffer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "DmaBuffer({:#x}, {} bytes, {:?})", self.device_addr(), self.len(), self.constraints.cache)
    }
}

impl DmaBuffer {
    /// `len` zeroed bytes meeting `c`.
    pub fn alloc(len: usize, c: DmaConstraints) -> Result<Self, DmaError> {
        let r = plan(len, &c).and_then(|align| {
            mm_core::dma::alloc(len, align, c.limit, c.cache.cache_type()).map_err(|e| match e {
                DmaAllocError::BadRequest => DmaError::Invalid,
                DmaAllocError::NoMemory => DmaError::NoMemory,
                DmaAllocError::Retype(m) => {
                    let d = retype_error(m);
                    if d == DmaError::Invalid {
                        klog::event!(Error, "mm", "dma.retype_fault", len = len, error = alloc::format!("{m:?}");
                            "retyping the direct map failed in a way it never should (bug in MM)");
                    }
                    d
                }
            })
        });
        match r {
            Ok(block) => {
                DMA_ALLOCS.inc();
                if let Some(st) = block.retype_stats() {
                    DMA_RETYPED.add(st.pages);
                    let phys = klog::Hex(block.region().phys());
                    let cache = alloc::format!("{:?}", c.cache);
                    // Splitting large pages changes the direct map for good (Info); a plain retype is routine.
                    if st.split_1g + st.split_2m > 0 {
                        klog::event!(Info, "mm", "dma.retyped", phys = phys, pages = st.pages, cache = cache, split_1g = st.split_1g,
                            split_2m = st.split_2m; "direct-map pages of a DMA buffer retyped, large pages split");
                    } else {
                        klog::event!(Debug, "mm", "dma.retyped", phys = phys, pages = st.pages, cache = cache, split_1g = 0, split_2m = 0;
                            "direct-map pages of a DMA buffer retyped");
                    }
                }
                Ok(DmaBuffer { block, constraints: c })
            }
            Err(e) => {
                DMA_FAILS.inc();
                klog::event!(Warn, "mm", "dma.alloc_failed", len = len, limit = klog::Hex(c.limit), align = c.align,
                    boundary = c.boundary, cache = alloc::format!("{:?}", c.cache), error = alloc::format!("{e:?}");
                    "DMA buffer refused");
                Err(e)
            }
        }
    }

    /// The address the device uses.
    pub fn device_addr(&self) -> u64 {
        self.block.region().phys()
    }
    /// The physical address (equal to `device_addr` while the IOMMU is off).
    pub fn phys(&self) -> u64 {
        self.block.region().phys()
    }
    /// The CPU view: the handle every access goes through (`cpu().cpu_addr()` for logs).
    pub fn cpu(&self) -> &DmaRegion {
        self.block.region()
    }
    pub fn cpu_addr(&self) -> u64 {
        self.block.region().cpu_addr()
    }
    pub fn len(&self) -> usize {
        self.block.region().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn constraints(&self) -> DmaConstraints {
        self.constraints
    }
    pub fn cache(&self) -> DmaCache {
        self.constraints.cache
    }

    /// Byte copies (bounds-checked; false = nothing done).
    pub fn read(&self, off: usize, out: &mut [u8]) -> bool {
        self.cpu().copy_out(off, out)
    }
    pub fn write(&self, off: usize, data: &[u8]) -> bool {
        self.cpu().copy_in(off, data)
    }
    pub fn copy_in(&self, off: usize, src: &[u8]) -> bool {
        self.cpu().copy_in(off, src)
    }
    pub fn copy_out(&self, off: usize, dst: &mut [u8]) -> bool {
        self.cpu().copy_out(off, dst)
    }
    pub fn fill(&self, off: usize, n: usize, byte: u8) -> bool {
        self.cpu().fill(off, n, byte)
    }
    pub fn zero(&self) -> bool {
        self.fill(0, self.len(), 0)
    }

    /// Typed values: one naturally aligned volatile access each (None/false if outside or misaligned).
    pub fn read_u8(&self, off: usize) -> Option<u8> {
        self.cpu().read_u8(off)
    }
    pub fn read_u16(&self, off: usize) -> Option<u16> {
        self.cpu().read_u16(off)
    }
    pub fn read_u32(&self, off: usize) -> Option<u32> {
        self.cpu().read_u32(off)
    }
    pub fn read_u64(&self, off: usize) -> Option<u64> {
        self.cpu().read_u64(off)
    }
    pub fn write_u8(&self, off: usize, v: u8) -> bool {
        self.cpu().write_u8(off, v)
    }
    pub fn write_u16(&self, off: usize, v: u16) -> bool {
        self.cpu().write_u16(off, v)
    }
    pub fn write_u32(&self, off: usize, v: u32) -> bool {
        self.cpu().write_u32(off, v)
    }
    pub fn write_u64(&self, off: usize, v: u64) -> bool {
        self.cpu().write_u64(off, v)
    }
    /// Before ringing a doorbell: makes earlier writes visible to the device (drains WC buffers).
    pub fn write_barrier(&self) {
        self.cpu().write_barrier()
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut v = alloc::vec![0u8; self.len()];
        self.read(0, &mut v);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retype_errors_map_to_what_a_driver_can_act_on() {
        assert_eq!(retype_error(MapError::NoMemory), DmaError::NoMemory);
        for e in [MapError::NeedsShootdown, MapError::WrongRange, MapError::Unsupported] {
            assert_eq!(retype_error(e), DmaError::Unsupported, "{e:?}");
        }
        for e in [MapError::NotMapped, MapError::NotAligned, MapError::HugePageInTheWay] {
            assert_eq!(retype_error(e), DmaError::Invalid, "{e:?}");
        }
    }

    const P: usize = 4096;

    #[test]
    fn plans_valid_requests() {
        assert_eq!(plan(1, &DmaConstraints::dma32()), Ok(4096));
        assert_eq!(plan(10_000, &DmaConstraints::dma64().with_align(0)), Ok(4096), "align 0 = a page");
        assert_eq!(plan(64, &DmaConstraints::dma32().with_align(64)), Ok(4096), "small alignments become a page");
        assert_eq!(plan(P, &DmaConstraints::dma32().with_align(2 << 20)), Ok(2 << 20));
        assert_eq!(plan(64 << 10, &DmaConstraints::dma32().with_boundary(64 << 10)), Ok(4096), "exactly the boundary");
        assert_eq!(plan(100, &DmaConstraints::dma32().with_boundary(2048)), Ok(4096), "boundary below a page");
        assert_eq!(plan(MAX_BYTES as usize, &DmaConstraints::dma64()), Ok(4096));
        assert_eq!(plan(P, &DmaConstraints::from_mask_bits(12)), Ok(4096), "a 4 KiB limit still fits one page");
    }

    #[test]
    fn refuses_hostile_requests() {
        let c = DmaConstraints::dma64();
        assert_eq!(plan(0, &c), Err(DmaError::Invalid));
        assert_eq!(plan(usize::MAX, &c), Err(DmaError::TooLarge));
        assert_eq!(plan(MAX_BYTES as usize + 1, &c), Err(DmaError::TooLarge));
        assert_eq!(plan(P, &c.with_align(3 << 12)), Err(DmaError::Invalid));
        assert_eq!(plan(P, &c.with_align(u64::MAX)), Err(DmaError::Invalid));
        assert_eq!(plan(P, &c.with_align(1 << 63)), Err(DmaError::TooLarge));
        assert_eq!(plan(P, &c.with_boundary(3 << 12)), Err(DmaError::Invalid));
        assert_eq!(plan(P + 1, &c.with_boundary(P as u64)), Err(DmaError::Invalid), "longer than its boundary");
        assert_eq!(plan(P, &DmaConstraints { limit: 0, ..c }), Err(DmaError::Invalid));
        assert_eq!(plan(P + 1, &DmaConstraints { limit: 2 * P as u64 - 1, ..c }), Err(DmaError::Invalid));
    }

    #[test]
    fn masks() {
        assert_eq!(DmaConstraints::from_mask_bits(32).limit, 1 << 32);
        assert_eq!(DmaConstraints::from_mask_bits(64).limit, u64::MAX);
        assert_eq!(DmaConstraints::from_mask_bits(200).limit, u64::MAX);
        assert_eq!(DmaConstraints::default(), DmaConstraints::dma32(), "the G0 default stays");
    }

    /// The boundary argument of `plan`, checked exhaustively for small sizes: a run of `len` at the start of
    /// a block of size B (the next power of two of len, at least `align`), aligned to B, never crosses.
    #[test]
    fn runs_at_block_starts_never_cross_their_boundary() {
        for pages in 1..=40u64 {
            for align_pages in [1u64, 2, 8, 32] {
                for boundary_pages in [1u64, 2, 4, 16, 64, 128] {
                    if pages > boundary_pages {
                        continue;
                    }
                    let order = mm_core::buddy::exact_order(pages, align_pages).unwrap();
                    let block = 1u64 << order;
                    for k in 0..8u64 {
                        let start = k * block;
                        assert_eq!(start / boundary_pages, (start + pages - 1) / boundary_pages,
                            "{pages} pages at {start} cross a {boundary_pages}-page boundary");
                    }
                }
            }
        }
    }
}
