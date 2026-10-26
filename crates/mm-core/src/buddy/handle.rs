//! `BuddyFrames`: the handle the kernel installs (the boot allocator, also `boot_buddy()`) or a staged
//! allocator over a test map, and their constructors.

use super::{Buddy, BuildError, Storage};
use crate::map::{self, MapRange, RangeUse};
use crate::{FrameAlloc, FrameStats, Kind, Zone};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering::AcqRel};
use ksync::OnceRef;

enum Holder {
    Boot(&'static Buddy),
    Own(Box<Buddy>),
}

/// A buddy allocator handle: the boot allocator (also reachable as `boot_buddy()`) or a staged one.
pub struct BuddyFrames(Holder);

static BOOT: OnceRef<Buddy> = OnceRef::new();
/// Set by the first boot build, before it writes anything: the carve (zeroing the tables' frames through the
/// direct map) happens at most once per boot, so a later call can never wipe memory the live allocator owns.
static CARVED: AtomicBool = AtomicBool::new(false);

/// The boot allocator, for zone numbers, draining, module release and the consistency check.
pub fn boot_buddy() -> Option<&'static Buddy> {
    BOOT.get()
}

impl core::ops::Deref for BuddyFrames {
    type Target = Buddy;
    fn deref(&self) -> &Buddy {
        match &self.0 {
            Holder::Boot(b) => b,
            Holder::Own(b) => b,
        }
    }
}

impl FrameAlloc for BuddyFrames {
    fn alloc(&self, order: u32, zone: Zone, kind: Kind) -> Option<u64> {
        (**self).alloc(order, zone, kind)
    }
    fn free(&self, phys: u64, order: u32, kind: Kind) {
        (**self).free(phys, order, kind)
    }
    fn stats(&self) -> FrameStats {
        (**self).stats()
    }
}

impl BuddyFrames {
    /// The boot allocator from free byte ranges (the G0 constructor `kernel/` calls; ranges below 1 MiB and
    /// the heap seed are never used). Prefer `from_memory_map`, which also knows the boot modules; `new` goes
    /// once the desk switched `kernel/` over (change request MM-1).
    ///
    /// Safe only because it is one-shot: the first call (the kernel's boot path, before any other crate runs)
    /// carves the tables; every later call is refused before anything is written and returns the first
    /// allocator (event `frames.second_boot_allocator`).
    pub fn new(free: Vec<(u64, u64)>) -> Self {
        let mut ranges: Vec<MapRange> = free.into_iter().map(|(s, e)| MapRange::new(s, e, RangeUse::Free)).collect();
        let seed = crate::HEAP.seed_range();
        ranges.push(MapRange::new(seed.0, seed.1, RangeUse::Reserved));
        Self::boot(&ranges)
    }

    /// The boot allocator from the BootInfo memory map. `reserved`: ranges the kernel took for itself before
    /// (the heap seed chunk). One-shot like `new`.
    ///
    /// # Safety
    /// `map` is the firmware memory map of this boot as bitsboot handed it over, the call happens once,
    /// before any frame is handed out, and every range the map calls free (conventional, loader,
    /// boot-services after hand-over) minus `reserved` is RAM nobody uses and the direct map covers: the
    /// allocator zeroes its tables there and hands the rest out.
    pub unsafe fn from_memory_map(map: &bootinfo::MemoryMap<'_>, reserved: &[(u64, u64)]) -> Self {
        let mut ranges = map::from_bootinfo(map);
        ranges.extend(reserved.iter().map(|&(s, e)| MapRange::new(s, e, RangeUse::Reserved)));
        let seed = crate::HEAP.seed_range();
        ranges.push(MapRange::new(seed.0, seed.1, RangeUse::Reserved));
        Self::boot(&ranges)
    }

    fn boot(ranges: &[MapRange]) -> Self {
        if CARVED.swap(true, AcqRel) {
            klog::event!(Error, "mm", "frames.second_boot_allocator"; "refused before writing anything; the first boot allocator stays");
            let first = BOOT.get().unwrap_or_else(|| Box::leak(Box::new(Buddy::empty("frames"))));
            return BuddyFrames(Holder::Boot(first));
        }
        let me = match Buddy::build("frames", ranges, Storage::Carve, true) {
            Ok(b) => b,
            Err(e) => {
                klog::event!(Error, "mm", "frames.no_metadata_room", error = alloc::format!("{e:?}"); "no free run holds the allocator's tables; no frames");
                Buddy::empty("frames")
            }
        };
        me.log_init();
        let me: &'static Buddy = Box::leak(Box::new(me));
        let _ = BOOT.set(me);
        BuddyFrames(Holder::Boot(me))
    }

    /// An allocator over `ranges`. With `Storage::Carve` in the kernel the free memory named by `ranges` is
    /// written (metadata), so only the boot path uses it; `Storage::Heap` never touches the managed memory.
    pub fn staged(name: &'static str, ranges: &[MapRange], storage: Storage) -> Result<Self, BuildError> {
        if cfg!(target_os = "none") && storage == Storage::Carve {
            return Err(BuildError::CarveOnlyAtBoot);
        }
        Buddy::build(name, ranges, storage, false).map(|b| BuddyFrames(Holder::Own(Box::new(b))))
    }

}

