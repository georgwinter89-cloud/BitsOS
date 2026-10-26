//! The vmap allocator (MM-2): device windows with explicit PAT types (WB, WC, UC-, UC) in
//! [VMAP_BASE, CPU_AREA_BASE), one unmapped guard page below each, 2 MiB pages where phys, length and the
//! MTRRs allow, unmapping that frees page-table frames. `hal::paging::map_device_window` reaches it through the
//! device-mapper hook, so `hal::mmio` can use it without depending on `mm`.
//!
//! Unmapping shoots the range down on every CPU (MM-6, `hal::paging::unmap_kernel`). Only when a CPU does not
//! confirm that shootdown is a window kept out of use (quarantined); `release_quarantined` retries the queue,
//! and the `mm.report` thread calls it once a second while anything is queued.
//!
//! MM-5a: `reserve`/`release` hand out plain kernel-virtual ranges (address space with a guard page below,
//! nothing mapped) for code that maps its own pages there, kernel stacks first; crates below `mm` reach them
//! through `hal::paging::reserve_kernel_va`/`release_kernel_va`. `release` unmaps what is left and frees the
//! page tables that became empty once every CPU confirmed the shootdown; otherwise it quarantines the range
//! like a window.
//!
//! Locking: one sleeping `ksync::Mutex` guards the windows. Mapping and dropping a window may wait for it,
//! so neither happens in interrupt context or in a no-wait section of a KernelRt/Interactive thread (B4).
//! The free space itself sits under a short `SpinLock` ("mm.vmap.space", taken under "mm.vmap" and alone by
//! `reserve`), so a reservation never sleeps and may be made under a caller's spin lock. Events are formatted
//! after the locks are released.

use crate::mtrr::Mtrrs;
use crate::vaspace::{Reservation, VaSpace};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use hal::paging::{self, CacheType, PageFlags, PageSize, UnmapStats, VmapError};
use ksync::{Mutex, SpinLock};

const M2: u64 = 2 << 20;
/// Unmapped bytes below every window.
pub const GUARD: u64 = 4096;

struct Window {
    res: Reservation,
    phys: u64,
    cache: CacheType,
    pages_2m: u64,
    pages_4k: u64,
}

struct State {
    windows: BTreeMap<u64, Window>,
    /// RAM, sorted and merged: device windows must never alias it.
    ram: Vec<(u64, u64)>,
    mtrrs: Mtrrs,
    /// Windows dropped (or half-mapped) while they could not be unmapped; still mapped, range not reusable.
    quarantine: Vec<Window>,
    mapped_2m: u64,
    mapped_4k: u64,
}

static VMAP: Mutex<Option<State>> = Mutex::named("mm.vmap", None);
static SPACE: SpinLock<Option<VaSpace>> = SpinLock::named("mm.vmap.space", None);

/// Plain reservations (`reserve`): live ones sorted by body address, and released ones that could not be
/// unmapped. Sorted Vecs with room reserved before each change, so an empty kernel heap fails the call
/// instead of aborting the kernel under the caller's spin lock (R2).
struct Kva {
    live: Vec<Reservation>,
    quarantine: Vec<Reservation>,
}
static KVA: SpinLock<Kva> = SpinLock::named("mm.vmap.kva", Kva { live: Vec::new(), quarantine: Vec::new() });

klog::counter!(VMAP_MAPS, "mm.vmap.maps");
klog::counter!(VMAP_UNMAPS, "mm.vmap.unmaps");
klog::counter!(VMAP_QUARANTINED, "mm.vmap.quarantined");

/// Installs the allocator over [lo, hi) with `taken` ranges (mappings that exist already) carved out, and
/// registers it as hal's device mapper.
pub(crate) fn init(lo: u64, hi: u64, taken: &[(u64, u64)], ram: Vec<(u64, u64)>, mtrrs: Mtrrs) {
    let mut space = VaSpace::new(lo, hi);
    let mut kept = 0u64;
    for &(s, e) in taken {
        let (s, e) = (s.max(lo), e.min(hi));
        if e > s && space.reserve(s, e) {
            kept += e - s;
        }
    }
    *SPACE.lock() = Some(space);
    *VMAP.lock() = Some(State { windows: BTreeMap::new(), ram, mtrrs, quarantine: Vec::new(), mapped_2m: 0, mapped_4k: 0 });
    if !paging::set_device_mapper(mapper) {
        klog::event!(Error, "mm", "vmap.mapper_taken"; "hal's device mapper was installed already; map_device_window does not use this allocator");
    }
    if !paging::set_kernel_va(reserve, release) {
        klog::event!(Error, "mm", "vmap.kva_taken"; "hal's kernel-VA hook was installed already");
    }
    klog::event!(Info, "mm", "vmap.ready", lo = klog::Hex(lo), hi = klog::Hex(hi), adopted_kb = kept >> 10);
}

/// A device window owned by the caller; dropping it unmaps the window. Drop it only where waiting is allowed
/// (not in interrupt context, not in a B4 no-wait section): the drop takes the vmap mutex.
#[derive(Debug)]
pub struct Vmap {
    virt: u64,
    body: u64,
    len: u64,
    phys: u64,
    cache: CacheType,
}

impl Vmap {
    /// Virtual address of the `phys` the window was asked for.
    pub fn virt(&self) -> u64 {
        self.virt
    }
    pub fn phys(&self) -> u64 {
        self.phys
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn cache(&self) -> CacheType {
        self.cache
    }
    /// The guard page right below the window (never mapped).
    pub fn guard_below(&self) -> u64 {
        self.body - GUARD
    }
    /// The first page after the window (a neighbour's guard page or free space).
    pub fn page_after(&self) -> u64 {
        (self.virt + self.len + 0xFFF) & !0xFFF
    }
}

impl Drop for Vmap {
    fn drop(&mut self) {
        unmap(self.body);
    }
}

fn overlaps(v: &[(u64, u64)], s: u64, e: u64) -> bool {
    let i = v.partition_point(|r| r.1 <= s);
    i < v.len() && v[i].0 < e
}

/// What `map_device` reports once the lock is released.
enum MapEvent {
    RefusedRam,
    NoSpace { span: u64, free: u64 },
    MapFailed { error: paging::MapError, cleanup: Option<paging::MapError> },
    Mapped { virt: u64, pages_2m: u64, pages_4k: u64 },
}

/// Maps `len` bytes of device memory at `phys` with `cache`. Refuses RAM: a second mapping of RAM with
/// another type than the direct map's would be undefined (DMA buffers with UC/WC are MM-4's, which retypes the
/// direct map instead). May wait for the vmap mutex (see the module comment).
pub fn map_device(phys: u64, len: u64, cache: CacheType) -> Result<Vmap, VmapError> {
    let end = phys.checked_add(len).filter(|_| len != 0).ok_or(VmapError::BadRange)?;
    let (first, last) = (phys & !0xFFF, end.checked_add(0xFFF).ok_or(VmapError::BadRange)? & !0xFFF);
    if last > 1 << 52 {
        return Err(VmapError::BadRange);
    }
    let (r, ev) = {
        let mut g = VMAP.lock();
        let st = g.as_mut().ok_or(VmapError::NotReady)?;
        let root = paging::kernel_root().ok_or(VmapError::NotReady)?;
        map_locked(st, root, phys, first, last, cache)
    };
    match ev {
        MapEvent::RefusedRam => klog::event!(Warn, "mm", "vmap.refused_ram", phys = klog::Hex(phys), len = len; "device window over RAM refused"),
        MapEvent::NoSpace { span, free } => klog::event!(Warn, "mm", "vmap.no_space", len = span, free = free),
        MapEvent::MapFailed { error, cleanup: None } => {
            klog::event!(Warn, "mm", "vmap.map_failed", phys = klog::Hex(phys), len = len, error = alloc::format!("{error:?}"))
        }
        MapEvent::MapFailed { error, cleanup: Some(c) } => {
            klog::event!(Error, "mm", "vmap.map_failed", phys = klog::Hex(phys), len = len, error = alloc::format!("{error:?}"),
                cleanup = alloc::format!("{c:?}"); "half-mapped range quarantined")
        }
        MapEvent::Mapped { virt, pages_2m, pages_4k } => {
            VMAP_MAPS.inc();
            klog::event!(Debug, "mm", "vmap.map", phys = klog::Hex(phys), len = len, virt = klog::Hex(virt),
                cache = alloc::format!("{cache:?}"), pages_2m = pages_2m, pages_4k = pages_4k);
        }
    }
    r.map(|body| Vmap { virt: body + (phys - first), body, len, phys, cache })
}

/// The body of `map_device` under the lock; returns the window's body address.
fn map_locked(st: &mut State, root: u64, phys: u64, first: u64, last: u64, cache: CacheType) -> (Result<u64, VmapError>, MapEvent) {
    if overlaps(&st.ram, first, last) {
        return (Err(VmapError::OverlapsRam), MapEvent::RefusedRam);
    }
    // 2 MiB pages need virt == phys modulo 2 MiB: reserve from a 2 MiB-aligned base plus phys's offset.
    let large = last - first >= M2;
    let lead = if large { first & (M2 - 1) } else { 0 };
    let span = last - first + lead;
    let res = SPACE.lock().as_mut().and_then(|sp| sp.alloc(span, if large { M2 } else { 4096 }, GUARD));
    let Some(res) = res else {
        return (Err(VmapError::NoVirtualSpace), MapEvent::NoSpace { span, free: space_free().0 });
    };
    let body = res.body + lead;
    let flags = PageFlags::kernel_data().with_cache(cache);
    let flags = PageFlags(flags.0 | PageFlags::GLOBAL);
    let mut a = first;
    let (mut n2m, mut n4k) = (0, 0);
    while a < last {
        let v = body + (a - first);
        let big = a.is_multiple_of(M2) && last - a >= M2 && st.mtrrs.block_type(a, M2).is_some();
        let size = if big { PageSize::Size2M } else { PageSize::Size4K };
        if let Err(e) = paging::map_kernel(root, v, a, size, flags) {
            // Whatever got mapped must go before the range is handed out again; if it cannot (a CPU did not
            // confirm the shootdown, or the walk refuses), the range stays out of use in the quarantine.
            let cleanup = paging::unmap_kernel(res.body, res.end - res.body, &mut |_| {}).err();
            if cleanup.is_some() {
                st.mapped_2m += n2m;
                st.mapped_4k += n4k;
                st.quarantine.push(Window { res, phys, cache, pages_2m: n2m, pages_4k: n4k });
            } else {
                give_back(&res);
            }
            let r = if e == paging::MapError::NoMemory { VmapError::NoMemory } else { VmapError::Map(e) };
            return (Err(r), MapEvent::MapFailed { error: e, cleanup });
        }
        if big {
            n2m += 1;
        } else {
            n4k += 1;
        }
        a += size.bytes();
    }
    st.mapped_2m += n2m;
    st.mapped_4k += n4k;
    st.windows.insert(body, Window { res, phys, cache, pages_2m: n2m, pages_4k: n4k });
    (Ok(body), MapEvent::Mapped { virt: body + (phys - first), pages_2m: n2m, pages_4k: n4k })
}

/// Unmaps a window's whole reservation and gives the range back; the window again on failure.
fn unmap_window(st: &mut State, w: Window) -> Result<(Window, UnmapStats), (Window, paging::MapError)> {
    match paging::unmap_kernel(w.res.body, w.res.end - w.res.body, &mut |_| {}) {
        Ok(stat) => {
            give_back(&w.res);
            st.mapped_2m -= w.pages_2m;
            st.mapped_4k -= w.pages_4k;
            Ok((w, stat))
        }
        Err(e) => Err((w, e)),
    }
}

fn unmap(body: u64) {
    let r = {
        let mut g = VMAP.lock();
        let Some(st) = g.as_mut() else { return };
        let Some(w) = st.windows.remove(&body) else { return };
        let r = unmap_window(st, w);
        r.map_err(|(w, e)| {
            let len = w.res.end - w.res.body;
            st.quarantine.push(w);
            (len, e)
        })
    };
    match r {
        Ok((w, stat)) => {
            VMAP_UNMAPS.inc();
            klog::event!(Debug, "mm", "vmap.unmap", virt = klog::Hex(body), phys = klog::Hex(w.phys), len = w.res.end - w.res.body,
                cache = alloc::format!("{:?}", w.cache), tables_freed = stat.tables_freed);
        }
        Err((len, paging::MapError::NeedsShootdown)) => {
            VMAP_QUARANTINED.inc();
            klog::event!(Warn, "mm", "vmap.quarantined", virt = klog::Hex(body), len = len;
                "a CPU did not confirm the TLB shootdown: range kept out of use, retried by mm.report");
        }
        Err((len, e)) => {
            VMAP_QUARANTINED.inc();
            klog::event!(Error, "mm", "vmap.unmap_failed", virt = klog::Hex(body), len = len, error = alloc::format!("{e:?}"); "range kept out of use");
        }
    }
}

/// Why `check_aperture` refuses a device range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApertureRefusal {
    NotReady,
    /// The range touches RAM: a program mapping would alias the direct map, maybe with another memory type.
    Ram,
    /// A kernel device window maps part of the range with another memory type (Linux's memtype tracking
    /// refuses the same conflict, arch/x86/mm/pat/memtype.c `reserve_pfn_range`).
    TypeConflict(CacheType),
}

/// MM-5b device mappings: may programs map [first, last) (page-aligned device memory) with `cache`? Not over RAM,
/// and not with another type than a kernel window (live or quarantined) that maps part of it.
pub(crate) fn check_aperture(first: u64, last: u64, cache: CacheType) -> Result<(), ApertureRefusal> {
    let g = VMAP.lock();
    let st = g.as_ref().ok_or(ApertureRefusal::NotReady)?;
    if overlaps(&st.ram, first, last) {
        return Err(ApertureRefusal::Ram);
    }
    let span = |w: &Window| {
        let s = w.phys & !0xFFF;
        (s, s + w.pages_2m * M2 + w.pages_4k * 4096)
    };
    for w in st.windows.values().chain(st.quarantine.iter()) {
        let (s, e) = span(w);
        if s < last && first < e && w.cache != cache {
            return Err(ApertureRefusal::TypeConflict(w.cache));
        }
    }
    Ok(())
}

/// Unmaps every quarantined window and returns its range to the allocator; returns how many were released.
/// Those whose shootdown is still not confirmed stay queued. Safe to call at any time (`mm.report` does).
pub fn release_quarantined() -> usize {
    let (released, left) = {
        let mut g = VMAP.lock();
        let Some(st) = g.as_mut() else { return 0 };
        let mut released = 0;
        for w in core::mem::take(&mut st.quarantine) {
            match unmap_window(st, w) {
                Ok(_) => released += 1,
                Err((w, _)) => st.quarantine.push(w),
            }
        }
        (released, st.quarantine.len())
    };
    let (kva_released, kva_left, kva_lost) = {
        // Retried in place (no allocation); the lock is not held across the unmaps.
        let mut held = core::mem::take(&mut KVA.lock().quarantine);
        let before = held.len();
        held.retain(|r| {
            let ok = paging::unmap_kernel(r.body, r.end - r.body, &mut |_| {}).is_ok();
            if ok {
                give_back(r);
            }
            !ok
        });
        let n = before - held.len();
        let mut k = KVA.lock();
        let mut lost = 0;
        if k.quarantine.is_empty() {
            k.quarantine = held;
        } else {
            for r in held {
                if !kva_quarantine(&mut k, r) {
                    lost += 1;
                }
            }
        }
        (n, k.quarantine.len(), lost)
    };
    if kva_lost > 0 {
        klog::event!(Warn, "mm", "vmap.kva_lost", reservations = kva_lost; "no heap to keep quarantined ranges listed; they stay out of use for good");
    }
    if released + kva_released > 0 {
        klog::event!(Info, "mm", "vmap.quarantine_released", windows = released, left = left, reservations = kva_released, reservations_left = kva_left);
    }
    released + kva_released
}

/// hal's device-mapper hook: the window and its owner (dropping the owner unmaps).
fn mapper(phys: u64, len: u64, cache: CacheType) -> Result<(u64, Box<dyn core::any::Any + Send + Sync>), VmapError> {
    let v = map_device(phys, len, cache)?;
    Ok((v.virt(), Box::new(v)))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VmapStats {
    pub windows: usize,
    pub free_bytes: u64,
    pub free_ranges: usize,
    pub quarantined_bytes: u64,
    pub quarantined_windows: usize,
    pub pages_2m: u64,
    pub pages_4k: u64,
}

pub fn stats() -> Option<VmapStats> {
    let g = VMAP.lock();
    let st = g.as_ref()?;
    Some(VmapStats {
        windows: st.windows.len(),
        free_bytes: space_free().0,
        free_ranges: space_free().1,
        quarantined_bytes: st.quarantine.iter().map(|w| w.res.end - w.res.start).sum(),
        quarantined_windows: st.quarantine.len(),
        pages_2m: st.mapped_2m,
        pages_4k: st.mapped_4k,
    })
}

/// Whether `virt` lies in free vmap space (nothing mapped there by the allocator, no guard).
pub fn is_free(virt: u64) -> bool {
    SPACE.lock().as_ref().is_some_and(|sp| sp.is_free(virt))
}

/// Returns a reservation's range to the free space; without heap for the bookkeeping it stays out of use
/// (counted; callers report their own events).
fn give_back(r: &Reservation) {
    if let Some(sp) = SPACE.lock().as_mut()
        && !sp.release(r.start, r.end)
    {
        RANGES_LOST.inc();
    }
}

/// Free bytes and free ranges of the vmap space.
fn space_free() -> (u64, usize) {
    SPACE.lock().as_ref().map_or((0, 0), |sp| (sp.free_bytes(), sp.free_ranges()))
}

klog::counter!(KVA_RESERVES, "mm.vmap.kva_reserves");
klog::counter!(RANGES_LOST, "mm.vmap.ranges_lost");

/// Why `reserve` failed (reported after the locks are released).
enum KvaFail {
    NotReady,
    NoHeap,
    NoSpace,
}

/// `len` bytes (rounded up to pages) of vmap address space with an unmapped guard page below, nothing
/// mapped; returns the first usable address. Never sleeps (see the module comment); None when the space is
/// used up or the kernel heap has no room for the bookkeeping.
pub fn reserve(len: u64) -> Option<u64> {
    let len = len.checked_add(0xFFF)? & !0xFFF;
    let r = {
        let mut k = KVA.lock();
        if k.live.try_reserve(1).is_err() {
            Err(KvaFail::NoHeap)
        } else {
            let mut sp = SPACE.lock();
            match sp.as_mut() {
                None => Err(KvaFail::NotReady),
                Some(sp) => match sp.alloc(len, 4096, GUARD) {
                    None => Err(KvaFail::NoSpace),
                    Some(r) => {
                        let i = k.live.partition_point(|x| x.body < r.body);
                        k.live.insert(i, r);
                        Ok(r.body)
                    }
                },
            }
        }
    };
    match r {
        Ok(body) => {
            KVA_RESERVES.inc();
            Some(body)
        }
        Err(KvaFail::NotReady) => None,
        Err(KvaFail::NoHeap) => {
            klog::event!(Warn, "mm", "vmap.kva_no_heap", len = len; "no heap for the reservation list; reservation refused");
            None
        }
        Err(KvaFail::NoSpace) => {
            klog::event!(Warn, "mm", "vmap.kva_no_space", len = len, free = space_free().0);
            None
        }
    }
}

/// Keeps a released range out of use; false when even the quarantine list has no room (the range is then
/// lost for good, counted).
fn kva_quarantine(k: &mut Kva, r: Reservation) -> bool {
    if k.quarantine.try_reserve(1).is_ok() {
        k.quarantine.push(r);
        true
    } else {
        RANGES_LOST.inc();
        false
    }
}

/// Gives back a `reserve`d range (`len` as reserved): unmaps whatever is still mapped in it, frees the page
/// tables that became empty and makes the range reusable. False when the range is kept out of use instead:
/// a CPU did not confirm the TLB shootdown (`release_quarantined` retries), or `base`/`len` is not a live
/// reservation.
pub fn release(base: u64, len: u64) -> bool {
    let len = len.saturating_add(0xFFF) & !0xFFF;
    let r = {
        let mut k = KVA.lock();
        match k.live.binary_search_by_key(&base, |x| x.body) {
            Ok(i) if k.live[i].end - k.live[i].body == len => Some(k.live.remove(i)),
            _ => None,
        }
    };
    let Some(r) = r else {
        klog::event!(Warn, "mm", "vmap.kva_bad_release", base = klog::Hex(base), len = len; "not a live reservation; nothing changed");
        return false;
    };
    match paging::unmap_kernel(r.body, r.end - r.body, &mut |_| {}) {
        Ok(stat) => {
            give_back(&r);
            klog::event!(Debug, "mm", "vmap.kva_release", base = klog::Hex(base), len = len, leaves = stat.leaves, tables_freed = stat.tables_freed);
            true
        }
        Err(e) => {
            let kept = kva_quarantine(&mut KVA.lock(), r);
            VMAP_QUARANTINED.inc();
            klog::event!(Info, "mm", "vmap.kva_quarantined", base = klog::Hex(base), len = len, error = alloc::format!("{e:?}"), listed = kept;
                "kept out of use: the TLB shootdown was not confirmed; retried by mm.report");
            false
        }
    }
}

/// Live plain reservations and quarantined ones.
pub fn kva_counts() -> (usize, usize) {
    let k = KVA.lock();
    (k.live.len(), k.quarantine.len())
}
