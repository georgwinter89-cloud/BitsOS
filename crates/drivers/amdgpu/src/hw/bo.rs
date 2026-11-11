//! GX-8a: the memory of a GEM object on the chip (`GpuDevice::bo_memory`). VRAM: a contiguous range of the user heap,
//! wiped, programs map it write-combined; contiguous so one mapping can use huge leaves and fragments (a request the
//! heap cannot place contiguously answers ENOMEM and the uAPI tries GTT). GTT and system objects: scattered pinned
//! `DmaPages`, write-back and snooped by default, write-combined and not snooped for CPU_GTT_USWC
//! (amdgpu_ttm.c:1084 `amdgpu_ttm_tt_pde_flags`). Each object charges its pool until it drops.

use super::heap::{Heaps, Seg};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use drm::dma_buf::Exporter;
use drm::gpu::BoPlace;
use hal::paging::CacheType;
use linux::errno::ENOMEM;
use mm::device::Aperture;
use mm::dma_pages::PinOwner;
#[cfg(not(test))]
use mm::dma::DmaCache;
#[cfg(not(test))]
use mm::dma_pages::{DmaPages, DmaPagesOptions};
use vfs::Errno;

const PAGE: u64 = 4096;
/// A wipe gives the CPU up after this much: a create of many GiB must not hold it for seconds.
pub const WIPE_CHUNK: u64 = 64 << 20;

/// Bytes of GEM objects in GTT and how far it may go.
pub struct GttPool {
    pub used: AtomicU64,
    pub total: u64,
}

/// A VRAM object.
pub struct VramBo {
    heaps: Arc<Heaps>,
    handle: u32,
    bytes: u64,
    phys: u64,
    segs: Vec<Seg>,
}

impl VramBo {
    /// `pages` pages at a multiple of `align_pages` pages. A larger object gets a natural alignment up to a slab, so
    /// its mapping can use fragments and huge leaves, as Linux's buddy allocator gives them.
    pub fn new(heaps: &Arc<Heaps>, pages: u64, align_pages: u32) -> Result<Arc<VramBo>, Errno> {
        let bytes = pages.checked_mul(PAGE).ok_or(ENOMEM)?;
        let natural = if bytes >= super::heap::SLAB {
            super::heap::SLAB
        } else {
            bytes.next_power_of_two()
        };
        let align = (u64::from(align_pages).max(1) * PAGE).next_power_of_two().max(natural);
        let r = heaps.alloc_user(bytes, align).ok_or(ENOMEM)?;
        let phys = heaps.carve.phys_of(r.range.start).ok_or(ENOMEM)?;
        let bo = Arc::new(VramBo { heaps: heaps.clone(), handle: r.handle, bytes, phys, segs: r.segs });
        // The pages held another program's data (Linux clears on request with SDMA, CREATE_VRAM_CLEARED; here always:
        // the same price as GTT's zeroed pages, and no process reads another's).
        for s in &bo.segs {
            let mut done = 0;
            while done < s.len {
                let n = (s.len - done).min(WIPE_CHUNK);
                if !s.win.fill(s.at + done, n, 0) {
                    klog::event!(Error, "gx", "hw.wipe_failed", at = klog::Hex(s.at + done), len = n);
                    return Err(ENOMEM);
                }
                done += n;
                if done < s.len {
                    ksync::yield_now();
                }
            }
        }
        // Not only in this CPU's write-combining buffers: a late eviction would overwrite a program's first store, and
        // another CPU could read the previous owner's data until then.
        bo.heaps.io.barrier();
        Ok(bo)
    }

    /// The carve-out offset of the first page.
    pub fn offset(&self) -> u64 {
        self.phys - self.heaps.carve.phys_base()
    }

    /// `f` per part of `[off, off + n)`: (segment, offset in its window, offset in the access, bytes).
    fn parts(&self, off: u64, n: usize, f: &mut dyn FnMut(&Seg, u64, usize, usize) -> bool) -> bool {
        let Some(end) = off.checked_add(n as u64).filter(|&e| e <= self.bytes) else { return false };
        let (mut at, mut seg_start) = (off, 0u64);
        for s in &self.segs {
            let seg_end = seg_start + s.len;
            if at < seg_end && at < end {
                let len = seg_end.min(end) - at;
                if !f(s, s.at + (at - seg_start), (at - off) as usize, len as usize) {
                    return false;
                }
                at += len;
            }
            seg_start = seg_end;
        }
        at == end
    }
}

impl Drop for VramBo {
    fn drop(&mut self) {
        // The windows first: a slab that goes back must lose its window before anyone else gets the range.
        drop(core::mem::take(&mut self.segs));
        self.heaps.release_user(self.handle);
    }
}

impl Exporter for VramBo {
    fn name(&self) -> &'static str {
        "amdgpu"
    }
    fn size(&self) -> u64 {
        self.bytes
    }
    fn aperture(&self) -> Option<Aperture> {
        self.heaps.io.mappable().then_some(Aperture::Mmio { phys: self.phys, len: self.bytes, cache: CacheType::WriteCombining })
    }
    fn page_phys(&self, page: u64) -> Option<u64> {
        let off = page.checked_mul(PAGE).filter(|&o| o < self.bytes)?;
        Some(self.phys + off)
    }
    fn read(&self, off: u64, out: &mut [u8]) -> bool {
        let n = out.len();
        self.parts(off, n, &mut |s, at, into, len| s.win.read(at, &mut out[into..into + len]))
    }
    fn write(&self, off: u64, data: &[u8]) -> bool {
        let ok = self.parts(off, data.len(), &mut |s, at, into, len| s.win.write(at, &data[into..into + len]));
        self.heaps.io.barrier();
        ok
    }
}

/// The pages of a GTT or system object: `DmaPages` in the kernel; in host tests (no frames) heap bytes at made-up,
/// scattered addresses.
enum Store {
    #[cfg(not(test))]
    Pages(Arc<DmaPages>),
    #[cfg(test)]
    Heap(ksync::SpinLock<Vec<u8>>, u64),
}

#[cfg(test)]
static NEXT_FAKE: AtomicU64 = AtomicU64::new(0x1_0000_0000);

impl Store {
    #[cfg(not(test))]
    fn alloc(pages: u64, wc: bool, owner: &Arc<PinOwner>) -> Result<Store, Errno> {
        let o = DmaPagesOptions::default();
        let o = if wc { o.with_cache(DmaCache::WriteCombining) } else { o };
        DmaPages::alloc_for(pages, o, Some(owner)).map(Store::Pages).map_err(|e| e.errno())
    }
    #[cfg(test)]
    fn alloc(pages: u64, _wc: bool, _owner: &Arc<PinOwner>) -> Result<Store, Errno> {
        let n = usize::try_from(pages.checked_mul(PAGE).ok_or(ENOMEM)?).map_err(|_| ENOMEM)?;
        let mut v = Vec::new();
        v.try_reserve_exact(n).map_err(|_| ENOMEM)?;
        v.resize(n, 0);
        Ok(Store::Heap(ksync::SpinLock::new(v), NEXT_FAKE.fetch_add((pages + 1) * PAGE, Ordering::Relaxed)))
    }
}

/// A GTT or system object.
pub struct SysBo {
    store: Store,
    place: BoPlace,
    bytes: u64,
    pool: Arc<GttPool>,
}

impl SysBo {
    /// ENOMEM when GTT is full or a pin limit refuses; the uAPI then tries the next place.
    pub fn new(pool: &Arc<GttPool>, place: BoPlace, pages: u64, wc: bool, owner: &Arc<PinOwner>) -> Result<Arc<SysBo>, Errno> {
        let bytes = pages.checked_mul(PAGE).ok_or(ENOMEM)?;
        if place == BoPlace::Gtt && pool.used.try_update(Ordering::AcqRel, Ordering::Acquire, |u| u.checked_add(bytes).filter(|&n| n <= pool.total)).is_err() {
            klog::event!(Debug, "gx", "hw.gtt_full", bytes = bytes, total = pool.total);
            return Err(ENOMEM);
        }
        match Store::alloc(pages, wc, owner) {
            Ok(store) => Ok(Arc::new(SysBo { store, place, bytes, pool: pool.clone() })),
            Err(e) => {
                if place == BoPlace::Gtt {
                    pool.used.fetch_sub(bytes, Ordering::AcqRel);
                }
                Err(e)
            }
        }
    }
}

impl Drop for SysBo {
    fn drop(&mut self) {
        if self.place == BoPlace::Gtt {
            self.pool.used.fetch_sub(self.bytes, Ordering::AcqRel);
        }
    }
}

impl Exporter for SysBo {
    fn name(&self) -> &'static str {
        "amdgpu"
    }
    fn size(&self) -> u64 {
        self.bytes
    }
    fn aperture(&self) -> Option<Aperture> {
        match &self.store {
            #[cfg(not(test))]
            Store::Pages(p) => Some(Aperture::Pages(p.clone())),
            #[cfg(test)]
            Store::Heap(..) => None,
        }
    }
    fn page_phys(&self, page: u64) -> Option<u64> {
        match &self.store {
            #[cfg(not(test))]
            Store::Pages(p) => p.page_phys(page),
            #[cfg(test)]
            Store::Heap(_, at) => page.checked_mul(PAGE).filter(|&o| o < self.bytes).map(|o| at + o),
        }
    }
    fn read(&self, off: u64, out: &mut [u8]) -> bool {
        match &self.store {
            #[cfg(not(test))]
            Store::Pages(p) => p.read(off, out),
            #[cfg(test)]
            Store::Heap(v, _) => {
                let g = v.lock();
                let Some(r) = usize::try_from(off).ok().and_then(|s| g.get(s..s.checked_add(out.len())?)) else { return false };
                out.copy_from_slice(r);
                true
            }
        }
    }
    fn write(&self, off: u64, data: &[u8]) -> bool {
        match &self.store {
            #[cfg(not(test))]
            Store::Pages(p) => p.write(off, data),
            #[cfg(test)]
            Store::Heap(v, _) => {
                let mut g = v.lock();
                let Some(r) = usize::try_from(off).ok().and_then(|s| g.get_mut(s..s.checked_add(data.len())?)) else { return false };
                r.copy_from_slice(data);
                true
            }
        }
    }
}
