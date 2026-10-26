//! The buddy frame allocator (MM-1; contract `C: FrameAlloc`).
//!
//! Zones DMA32 (< 4 GiB) and NORMAL (>= 4 GiB); NORMAL requests try NORMAL first and fall back to DMA32,
//! DMA32 requests never leave DMA32. Each zone is a set of regions (contiguous runs of pool memory from the
//! map); a region has one hierarchical bitmap of free blocks per order and a 16-bit state per frame
//! (`meta`). Blocks are 2^order frames aligned to their size in physical memory and never cross a region,
//! so a block never spans a hole, a reserved page or the 4 GiB boundary. Freeing merges with the buddy
//! while the buddy is a free block of the same order. Order-0 frames go through per-CPU lists (`pcp`).
//! Linux reference: `mm/page_alloc.c` (`__rmqueue_smallest`, `expand`, `__free_one_page`, pcp lists).
//!
//! Locks: per-CPU list -> zone (`IrqSpinLock`s; never klog, never the heap while held).

use crate::map::{self, MapRange, Plan, PlanError, DMA32_END_PFN};
use crate::meta::{self, Meta, State};
use crate::pcp::{self, PcpCpu};

mod check;
mod exact;
mod handle;
mod limit;
mod reclaim;
pub use exact::exact_order;
#[cfg(test)]
pub(crate) use exact::pieces;
pub use handle::{boot_buddy, BuddyFrames};
use crate::report::{BadFree, Reports};
use crate::{bitmap::HBitmap, FrameAlloc, FrameStats, Kind, Zone, KINDS, MAX_ORDER, PAGE};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use hal::percpu::PerCpu;
use ksync::IrqSpinLock;

pub const ORDERS: usize = MAX_ORDER as usize + 1;
pub(crate) const ZONES: usize = 2;
const ZONE_NAMES: [&str; ZONES] = ["dma32", "normal"];

pub(crate) fn zone_index(z: Zone) -> usize {
    match z {
        Zone::Dma32 => 0,
        Zone::Normal => 1,
    }
}

/// Where the metadata words live.
enum Blob {
    /// Carved from the pool at boot, reached through the direct map; never freed.
    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    Static(&'static [AtomicU64]),
    /// On the heap (host tests, staged allocators in ktests).
    Owned(Vec<AtomicU64>),
}

impl Blob {
    fn words(&self) -> &[AtomicU64] {
        match self {
            Blob::Static(s) => s,
            Blob::Owned(v) => v,
        }
    }
}

struct Region {
    start: u64,
    end: u64,
    zone: usize,
    meta_off: usize,
    maps: Vec<HBitmap>,
}

impl Region {
    fn bit(&self, pfn: u64, order: u32) -> u64 {
        (pfn >> order) - (self.start >> order)
    }
    fn holds(&self, pfn: u64, order: u32) -> bool {
        pfn >= self.start && pfn.checked_add(1 << order).is_some_and(|e| e <= self.end)
    }
}

#[derive(Clone, Copy)]
struct ZoneState {
    free_frames: u64,
    free_blocks: [u64; ORDERS],
}

/// Per-zone numbers (`hw_info memory`, fragmentation view, tests).
#[derive(Clone, Copy, Debug, Default)]
pub struct ZoneInfo {
    pub name: &'static str,
    pub managed_frames: u64,
    pub free_frames: u64,
    pub cached_frames: u64,
    pub free_blocks: [u64; ORDERS],
}

/// How a staged allocator stores its metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Storage {
    /// Boot: carve it from the pool (kernel: written through the direct map; host: same math, heap storage).
    Carve,
    /// Heap only; the pool is not touched (ktests over staged maps, host tests).
    Heap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildError {
    Plan(PlanError),
    /// `Storage::Carve` writes the managed memory: only the boot allocator may use it in the kernel.
    CarveOnlyAtBoot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseError {
    /// The range is not (entirely) a boot module that is still reserved.
    NotAModule,
}

/// The allocator itself; reached through a `BuddyFrames` handle or `boot_buddy()`.
pub struct Buddy {
    name: &'static str,
    blob: Blob,
    regions: Vec<Region>,
    zones: [IrqSpinLock<ZoneState>; ZONES],
    managed: [AtomicU64; ZONES],
    used: [AtomicU64; KINDS],
    cached: [AtomicU64; ZONES],
    /// Frames taken out of circulation after an inconsistent state (`quarantine`); 0 without a bug.
    leaked: AtomicU64,
    /// Frames holding this allocator's own metadata (counted as used by `Kind::Other`).
    meta_frames: u64,
    /// Where they are (physical, 0..0 when on the heap).
    meta_range: (u64, u64),
    /// Irq-safe: `free` (any context, also IRQ handlers via the heap) reads it on its error path.
    modules: IrqSpinLock<Vec<(u64, u64)>>,
    /// Bytes `release_module` gave back (FS-1 change request 5b): managed now, but not free in the input map.
    released_module_bytes: AtomicU64,
    /// The byte ranges `release_module` gave back (for the consistency ktests: free memory the input map still
    /// names as a module).
    released_ranges: IrqSpinLock<Vec<(u64, u64)>>,
    /// The map as given (for the consistency ktest and MCP views).
    input: Vec<MapRange>,
    pcp: PerCpu<PcpCpu>,
    own_reports: Reports,
    global_reports: bool,
    /// MM-9: the page cache's reclaimer, asked in the slow path (`reclaim.rs`).
    reclaimer: ksync::OnceRef<dyn crate::reclaim::Reclaim>,
    direct: crate::reclaim::DirectCounters,
    /// MM-9: free frames per zone that program and cache allocations leave to the kernel's own (the min
    /// watermark's share of the zone; 0 until a reclaimer is installed).
    reserve: [AtomicU64; ZONES],
    /// MM-9: reserve hits that skip the light reclaim round (one found nothing just before).
    skip: AtomicU64,
}

impl Buddy {
    fn empty(name: &'static str) -> Self {
        Self::assemble(name, Blob::Owned(Vec::new()), Vec::new(), (0, 0), false)
    }

    fn assemble(name: &'static str, blob: Blob, regions: Vec<Region>, carve: (u64, u64), global: bool) -> Self {
        let zs = ZoneState { free_frames: 0, free_blocks: [0; ORDERS] };
        let me = Buddy {
            name,
            blob,
            regions,
            zones: [IrqSpinLock::named("frames.dma32", zs), IrqSpinLock::named("frames.normal", zs)],
            managed: [const { AtomicU64::new(0) }; ZONES],
            used: [const { AtomicU64::new(0) }; KINDS],
            cached: [const { AtomicU64::new(0) }; ZONES],
            leaked: AtomicU64::new(0),
            meta_frames: carve.1 - carve.0,
            meta_range: (carve.0 * PAGE, carve.1 * PAGE),
            modules: IrqSpinLock::named("frames.modules", Vec::new()),
            released_module_bytes: AtomicU64::new(0),
            released_ranges: IrqSpinLock::named("frames.released", Vec::new()),
            input: Vec::new(),
            pcp: PerCpu::new(pcp::new_cpu),
            own_reports: Reports::new(false),
            global_reports: global,
            reclaimer: ksync::OnceRef::new(),
            direct: crate::reclaim::DirectCounters::new(),
            reserve: [const { AtomicU64::new(0) }; ZONES],
            skip: AtomicU64::new(0),
        };
        // Create the per-CPU lists now, while the heap may be used: later the allocator runs under the
        // heap's lock, and a lazy init there would re-enter the heap.
        let _ = me.pcp.get_for(0);
        me
    }

    fn build(name: &'static str, ranges: &[MapRange], storage: Storage, global: bool) -> Result<Self, BuildError> {
        let plan = map::plan(ranges, storage == Storage::Carve).map_err(BuildError::Plan)?;
        let blob = Self::make_blob(&plan);
        let mut regions = Vec::with_capacity(plan.regions.len());
        let mut off = 0usize;
        for &(s, e) in &plan.regions {
            let meta_off = off;
            off += meta::words_for(e - s);
            let mut maps = Vec::with_capacity(ORDERS);
            for k in 0..=MAX_ORDER {
                let bits = map::blocks(s, e, k);
                maps.push(HBitmap::new(bits, off));
                off += crate::bitmap::words_for(bits);
            }
            regions.push(Region { start: s, end: e, zone: if s < DMA32_END_PFN { 0 } else { 1 }, meta_off, maps });
        }
        let carve = plan.carve.unwrap_or((0, 0));
        let mut me = Self::assemble(name, blob, regions, carve, global);
        me.input = ranges.to_vec();
        *me.modules.lock() = plan.modules.clone();
        me.used[Kind::Other as usize].store(carve.1 - carve.0, Relaxed);
        let low = carve.1.min(DMA32_END_PFN) - carve.0.min(DMA32_END_PFN);
        me.managed[0].fetch_add(low, Relaxed);
        me.managed[1].fetch_add(carve.1 - carve.0 - low, Relaxed);
        for &(s, e) in &plan.free {
            me.add_free(s, e);
        }
        Ok(me)
    }

    fn make_blob(plan: &Plan) -> Blob {
        #[cfg(target_os = "none")]
        if let Some((s, e)) = plan.carve {
            let virt = hal::phys_to_virt(s * PAGE);
            // SAFETY: only the one-shot boot path gets here (`staged` refuses `Carve` in the kernel, and
            // `BuddyFrames::boot` refuses a second boot build before calling `build`), with the firmware map
            // of this boot before any frame was handed out (`from_memory_map`'s contract). [s, e) are free RAM
            // frames of that map that the plan removed from the pool, so nothing else owns them, now or later
            // (they are never freed); the direct map covers all RAM and `(e - s) * PAGE >= words * 8`
            // (map::plan). AtomicU64 has the alignment of a page-aligned u64.
            return unsafe {
                core::ptr::write_bytes(virt as *mut u8, 0, ((e - s) * PAGE) as usize);
                Blob::Static(core::slice::from_raw_parts(virt as *const AtomicU64, plan.words))
            };
        }
        Blob::Owned((0..plan.words).map(|_| AtomicU64::new(0)).collect())
    }

    fn log_init(&self) {
        let mb = |f: u64| (f * PAGE) >> 20;
        let z = [self.zone_info(0), self.zone_info(1)];
        // Computed first: a guard inside the event's arguments would live until the event is written, and
        // klog must never run under an allocator lock (R5).
        let modules_kb = (self.modules.lock().iter().map(|m| m.1 - m.0).sum::<u64>() * PAGE) >> 10;
        klog::event!(Info, "mm", "frames.init", allocator = self.name, total_mb = mb(z[0].managed_frames + z[1].managed_frames),
            free_mb = mb(z[0].free_frames + z[1].free_frames), dma32_mb = mb(z[0].managed_frames), normal_mb = mb(z[1].managed_frames),
            regions = self.regions.len(), meta_kb = (self.meta_frames * PAGE) >> 10,
            modules_kb = modules_kb, max_order = MAX_ORDER;
            "buddy allocator ready");
    }

    /// Physical range of the allocator's own tables when carved from the pool (else empty).
    pub fn meta_range(&self) -> (u64, u64) {
        self.meta_range
    }

    /// The memory map this allocator was built from.
    pub fn input_map(&self) -> &[MapRange] {
        &self.input
    }

    /// Number of regions (contiguous runs of pool memory).
    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    pub fn reports(&self) -> &Reports {
        if self.global_reports { &crate::report::GLOBAL } else { &self.own_reports }
    }

    fn blob(&self) -> &[AtomicU64] {
        self.blob.words()
    }

    fn region_of(&self, pfn: u64) -> Option<&Region> {
        let i = self.regions.partition_point(|r| r.start <= pfn).checked_sub(1)?;
        let r = &self.regions[i];
        (pfn < r.end).then_some(r)
    }

    fn zone_regions(&self, zone: usize) -> impl Iterator<Item = &Region> {
        self.regions.iter().filter(move |r| r.zone == zone)
    }

    fn get_meta(&self, r: &Region, pfn: u64) -> Meta {
        meta::get(&self.blob()[r.meta_off..], pfn - r.start)
    }
    fn set_meta(&self, r: &Region, pfn: u64, m: Meta) {
        meta::set(&self.blob()[r.meta_off..], pfn - r.start, m)
    }
    fn cas_meta(&self, r: &Region, pfn: u64, expect: Meta, new: Meta) -> Result<(), Meta> {
        meta::cas(&self.blob()[r.meta_off..], pfn - r.start, expect, new)
    }

    // ---- buddy core (zone lock held) ----

    fn insert_free(&self, r: &Region, zs: &mut ZoneState, pfn: u64, order: u32) {
        r.maps[order as usize].set(self.blob(), r.bit(pfn, order));
        self.set_meta(r, pfn, Meta::new(State::Free, order, 0));
        zs.free_blocks[order as usize] += 1;
        zs.free_frames += 1 << order;
    }

    fn remove_free(&self, r: &Region, zs: &mut ZoneState, pfn: u64, order: u32) {
        r.maps[order as usize].clear(self.blob(), r.bit(pfn, order));
        self.set_meta(r, pfn, Meta::NONE);
        zs.free_blocks[order as usize] -= 1;
        zs.free_frames -= 1 << order;
    }

    /// Puts a block whose head reads `NONE` back, merging with free buddies.
    fn free_block(&self, r: &Region, zs: &mut ZoneState, mut pfn: u64, mut order: u32) {
        while order < MAX_ORDER {
            let buddy = pfn ^ (1 << order);
            if !r.holds(buddy, order) || self.get_meta(r, buddy) != Meta::new(State::Free, order, 0) {
                break;
            }
            self.remove_free(r, zs, buddy, order);
            pfn = pfn.min(buddy);
            order += 1;
        }
        self.insert_free(r, zs, pfn, order);
    }

    /// Takes a free block of exactly `order` from the zone (splitting a bigger one); its head reads `NONE`.
    fn take_block(&self, zone: usize, zs: &mut ZoneState, order: u32) -> Option<(usize, u64)> {
        self.take_block_below(zone, zs, order, u64::MAX)
    }

    /// `take_block` for a block that ends at or below frame `limit_pfn` (DMA address limits). Each region's
    /// bitmap gives its lowest free block of an order, so a region whose lowest one ends above the limit has
    /// none below it at that order.
    fn take_block_below(&self, zone: usize, zs: &mut ZoneState, order: u32, limit_pfn: u64) -> Option<(usize, u64)> {
        for o in order..=MAX_ORDER {
            if zs.free_blocks[o as usize] == 0 {
                continue;
            }
            for (ri, r) in self.regions.iter().enumerate().filter(|(_, r)| r.zone == zone && r.start < limit_pfn) {
                let Some(b) = r.maps[o as usize].first(self.blob()) else { continue };
                let pfn = ((r.start >> o) + b) << o;
                if pfn + (1 << order) > limit_pfn {
                    continue;
                }
                self.remove_free(r, zs, pfn, o);
                let mut cur = o;
                while cur > order {
                    cur -= 1;
                    self.insert_free(r, zs, pfn + (1 << cur), cur);
                }
                return Some((ri, pfn));
            }
        }
        None
    }

    /// Adds free frames [s, e) (pfn) in maximal aligned blocks; they may span regions of both zones.
    fn add_free(&self, s: u64, e: u64) {
        for r in self.regions.iter().filter(|r| r.start < e && s < r.end) {
            let (mut p, end) = (s.max(r.start), e.min(r.end));
            let mut zs = self.zones[r.zone].lock();
            while p < end {
                let mut k = p.trailing_zeros().min(MAX_ORDER);
                while p + (1 << k) > end {
                    k -= 1;
                }
                self.free_block(r, &mut zs, p, k);
                p += 1 << k;
            }
            drop(zs);
            self.managed[r.zone].fetch_add(end - s.max(r.start), Relaxed);
        }
    }

    /// `deep`: may take frames of the reserve (MM-9: `below_reserve`).
    fn alloc_in(&self, zone: usize, order: u32, kind: Kind, deep: bool) -> Option<u64> {
        if order == 0
            && let Some(cpu) = self.pcp.find_for(hal::percpu::this_cpu())
        {
            return self.pcp_alloc(cpu, zone, kind, deep);
        }
        let mut zs = self.zones[zone].lock();
        if !deep && self.below_reserve(zone, &zs, kind) {
            return None;
        }
        let (ri, pfn) = self.take_block(zone, &mut zs, order)?;
        self.set_meta(&self.regions[ri], pfn, Meta::new(State::Alloc, order, kind as usize));
        Some(pfn)
    }

    // ---- per-CPU lists (order 0) ----

    fn pcp_alloc(&self, cpu: &PcpCpu, zone: usize, kind: Kind, deep: bool) -> Option<u64> {
        let mut list = cpu.lists[zone].lock();
        if list.is_empty() {
            let mut zs = self.zones[zone].lock();
            if !deep && self.below_reserve(zone, &zs, kind) {
                return None;
            }
            // INT-W14 (16-CPU pressure ktest): a refill never moves the reserve into a per-CPU list, where the
            // next hits of any kind would take it without asking (16 lists x BATCH is more than the reserve of a
            // small machine): it stops at the reserve; below it, a kernel kind or a deep allocation takes only
            // the frame it needs, program and cache memory go to direct reclaim first.
            let above = zs.free_frames.saturating_sub(self.reserve[zone].load(Relaxed));
            let n = match (pcp::BATCH as u64).min(above) as usize {
                0 if !deep && crate::reclaim::direct_allowed(kind) => return None,
                0 => 1,
                n => n,
            };
            for _ in 0..n {
                let Some((ri, pfn)) = self.take_block(zone, &mut zs, 0) else { break };
                self.set_meta(&self.regions[ri], pfn, Meta::new(State::Cached, 0, 0));
                list.push(pfn);
                self.cached[zone].fetch_add(1, Relaxed);
            }
        }
        while let Some(pfn) = list.pop() {
            self.cached[zone].fetch_sub(1, Relaxed);
            if let Some(r) = self.region_of(pfn)
                && self.cas_meta(r, pfn, Meta::new(State::Cached, 0, 0), Meta::new(State::Alloc, 0, kind as usize)).is_ok()
            {
                return Some(pfn);
            }
            // A listed frame outside every region or no longer `Cached` is a kernel bug elsewhere: never hand
            // it out, keep it out of use and count it (stats: leaked_bytes), then try the next one.
            self.quarantine(pfn);
        }
        None
    }

    /// Returns up to `n` frames from the cold end of a per-CPU list to the buddy lists.
    fn pcp_return(&self, list: &mut Vec<u64>, zone: usize, n: usize) -> u64 {
        let n = n.min(list.len());
        let mut zs = self.zones[zone].lock();
        for pfn in list.drain(..n) {
            self.cached[zone].fetch_sub(1, Relaxed);
            match self.region_of(pfn) {
                Some(r) if self.cas_meta(r, pfn, Meta::new(State::Cached, 0, 0), Meta::NONE).is_ok() => {
                    self.free_block(r, &mut zs, pfn, 0)
                }
                _ => self.quarantine(pfn),
            }
        }
        n as u64
    }

    /// A frame the per-CPU lists held whose state is not `Cached` any more (cannot happen without a bug):
    /// it leaves circulation and is counted, so `free + used + leaked == total` still holds.
    fn quarantine(&self, pfn: u64) {
        self.leaked.fetch_add(1, Relaxed);
        self.reports().corrupt(pfn * PAGE);
    }

    /// Creates every possible CPU's lists (allocates: call from the BSP before the APs start, outside the
    /// allocator). The allocation paths never create a list (`find_for`); a CPU without one uses the buddy lists.
    pub fn prepare_cpus(&self) {
        self.pcp.cover_possible();
    }

    /// Empties every CPU's lists into the buddy lists; returns the frames moved.
    pub fn drain_all(&self) -> u64 {
        let mut moved = 0;
        for cpu in self.pcp.iter_existing() {
            for (zone, l) in cpu.lists.iter().enumerate() {
                let mut list = l.lock();
                let n = list.len();
                moved += self.pcp_return(&mut list, zone, n);
            }
        }
        moved
    }

    // ---- free ----

    /// Validates a free and claims the block (its head becomes `NONE`); the recorded kind on success. `tag` is
    /// the owner tag the block must carry (`meta::TAG_NONE` for every caller outside the heap).
    fn claim(&self, phys: u64, order: u32, kind: Kind, tag: u8) -> Result<(&Region, u64, usize), BadFree> {
        if order > MAX_ORDER {
            return Err(BadFree::BadOrder);
        }
        if !phys.is_multiple_of(PAGE << order) {
            return Err(BadFree::Misaligned);
        }
        let pfn = phys / PAGE;
        let r = self.region_of(pfn).filter(|r| r.holds(pfn, order)).ok_or(BadFree::OutOfRange)?;
        loop {
            let m = self.get_meta(r, pfn);
            match m.state {
                // Module frames and the allocator's tables sit inside regions but were never handed out.
                State::None if self.modules.lock().iter().any(|m| m.0 <= pfn && pfn < m.1) => return Err(BadFree::OutOfRange),
                State::None if self.meta_range.0 <= phys && phys < self.meta_range.1 => return Err(BadFree::OutOfRange),
                State::None => return Err(BadFree::NotHead),
                State::Free | State::Cached => return Err(BadFree::DoubleFree),
                State::Alloc if m.order != order => return Err(BadFree::OrderMismatch),
                State::Alloc if m.tag != tag => return Err(BadFree::HeapOwned),
                State::Alloc => {
                    let new = if order == 0 { Meta::new(State::Cached, 0, 0) } else { Meta::NONE };
                    if self.cas_meta(r, pfn, m, new).is_ok() {
                        if m.kind != kind as usize {
                            self.reports().kind_mismatch(phys, order, kind as usize, m.kind);
                        }
                        return Ok((r, pfn, m.kind));
                    }
                }
            }
        }
    }

    /// Releases a boot module's frames (when a Reload no longer needs them, V2-PLAN §2.5). Returns bytes.
    /// Emits an event: call holding no frames/heap lock (an ordinary kernel context).
    pub fn release_module(&self, start: u64, end: u64) -> Result<u64, ReleaseError> {
        let (s, e) = (start.div_ceil(PAGE), end / PAGE);
        if e <= s {
            return Err(ReleaseError::NotAModule);
        }
        {
            let mut mods = self.modules.lock();
            let i = mods.iter().position(|m| m.0 <= s && e <= m.1).ok_or(ReleaseError::NotAModule)?;
            let m = mods.remove(i);
            if m.0 < s {
                mods.push((m.0, s));
            }
            if e < m.1 {
                mods.push((e, m.1));
            }
        }
        self.add_free(s, e);
        self.released_module_bytes.fetch_add((e - s) * PAGE, Relaxed);
        // Thread context (see the doc): the push may allocate; a failure only leaves the ktests' view short.
        let mut r = self.released_ranges.lock();
        if r.try_reserve(1).is_ok() {
            r.push((s * PAGE, e * PAGE));
        }
        drop(r);
        klog::event!(Info, "mm", "frames.module_released", allocator = self.name, start = klog::Hex(s * PAGE), kb = ((e - s) * PAGE) >> 10);
        Ok((e - s) * PAGE)
    }

    /// Bytes given back by `release_module` since the start (they count in `stats().total_bytes` but the input
    /// map still names them as a module).
    pub fn released_module_bytes(&self) -> u64 {
        self.released_module_bytes.load(Relaxed)
    }

    /// The byte ranges given back by `release_module` (start, end), oldest first.
    pub fn released_module_ranges(&self) -> Vec<(u64, u64)> {
        self.released_ranges.lock().clone()
    }

    /// Test hook: a stray write turns the most recently cached frame of CPU 0's list in `zone` into `NONE`
    /// (what a bug elsewhere could do); returns its address.
    #[cfg(test)]
    pub(crate) fn corrupt_cached_frame(&self, zone: usize) -> Option<u64> {
        let cpu = self.pcp.get_for(0)?;
        let pfn = *cpu.lists[zone].lock().last()?;
        self.set_meta(self.region_of(pfn)?, pfn, Meta::NONE);
        Some(pfn * PAGE)
    }

    pub fn zone_info(&self, zone: usize) -> ZoneInfo {
        let zs = *self.zones[zone].lock();
        ZoneInfo {
            name: ZONE_NAMES[zone],
            managed_frames: self.managed[zone].load(Relaxed),
            free_frames: zs.free_frames,
            cached_frames: self.cached[zone].load(Relaxed),
            free_blocks: zs.free_blocks,
        }
    }
}

impl Buddy {
    /// `FrameAlloc::free` of a block carrying owner tag `tag` (the heap's slabs and run pieces); false if
    /// refused (counted as a bad free, nothing changed).
    pub(crate) fn free_tagged(&self, phys: u64, order: u32, kind: Kind, tag: u8) -> bool {
        let (r, pfn, owner) = match self.claim(phys, order, kind, tag) {
            Ok(c) => c,
            Err(why) => {
                self.reports().bad_free(phys, order, kind as usize, why);
                return false;
            }
        };
        self.used[owner].fetch_sub(1 << order, Relaxed);
        if order == 0
            && let Some(cpu) = self.pcp.find_for(hal::percpu::this_cpu())
        {
            let mut list = cpu.lists[r.zone].lock();
            if list.len() < list.capacity() {
                list.push(pfn);
                self.cached[r.zone].fetch_add(1, Relaxed);
                if list.len() > pcp::HIGH {
                    self.pcp_return(&mut list, r.zone, pcp::BATCH);
                }
                return true;
            }
            drop(list);
        }
        // No list, or list full (cannot happen with HIGH < capacity): straight to the buddy lists.
        if order == 0 && self.cas_meta(r, pfn, Meta::new(State::Cached, 0, 0), Meta::NONE).is_err() {
            self.quarantine(pfn);
            return true;
        }
        let mut zs = self.zones[r.zone].lock();
        self.free_block(r, &mut zs, pfn, order);
        true
    }
}

impl Buddy {
    /// The allocation proper: the zones in order, then once more after the per-CPU lists were drained.
    /// `deep`: may take the reserve (always true in effect while no reclaimer is installed: the reserve is 0).
    #[inline(always)]
    fn grab(&self, order: u32, zone: Zone, kind: Kind, deep: bool) -> Option<u64> {
        let tries: &[usize] = match zone {
            Zone::Dma32 => &[0],
            Zone::Normal => &[1, 0],
        };
        for attempt in 0..2 {
            for &z in tries {
                if let Some(pfn) = self.alloc_in(z, order, kind, deep) {
                    if z != zone_index(zone) {
                        self.reports().fallback();
                    }
                    self.used[kind as usize].fetch_add(1 << order, Relaxed);
                    return Some(pfn * PAGE);
                }
            }
            if attempt == 0 {
                if self.drain_all() == 0 {
                    break;
                }
                self.reports().drain();
            }
        }
        None
    }
}

impl FrameAlloc for Buddy {
    fn alloc(&self, order: u32, zone: Zone, kind: Kind) -> Option<u64> {
        if order > MAX_ORDER {
            self.reports().alloc_fail(order, zone_index(zone));
            return None;
        }
        if let Some(p) = self.grab(order, zone, kind, false) {
            return Some(p);
        }
        // MM-9: only after both attempts and the drain failed (the fast path is unchanged).
        if let Some(p) = self.alloc_reclaiming(order, zone, kind) {
            return Some(p);
        }
        self.reports().alloc_fail(order, zone_index(zone));
        None
    }

    fn free(&self, phys: u64, order: u32, kind: Kind) {
        self.free_tagged(phys, order, kind, meta::TAG_NONE);
    }

    fn stats(&self) -> FrameStats {
        let mut s = FrameStats::default();
        for z in 0..ZONES {
            let i = self.zone_info(z);
            s.total_bytes += i.managed_frames * PAGE;
            s.free_bytes += (i.free_frames + i.cached_frames) * PAGE;
        }
        for (k, u) in self.used.iter().enumerate() {
            s.used_by_kind[k] = u.load(Relaxed) * PAGE;
        }
        s.leaked_bytes = self.leaked.load(Relaxed) * PAGE;
        s
    }
}
