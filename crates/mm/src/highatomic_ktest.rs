//! ktests of the HighAtomic reserve (`mm_core::buddy::highatomic`): on a staged allocator a block for a caller
//! under a spin lock comes out of it when the rest is in pieces, and nobody who may wait gets it; the boot allocator
//! made its reserve and keeps it out of the usable free memory.

use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};
use mm_core::map::{MapRange, RangeUse};
use mm_core::{BuddyFrames, FrameAlloc, Kind, MigrateType, Storage, Zone, PAGE, PAGEBLOCK_FRAMES};

const MIB: u64 = 1 << 20;
const HA: usize = MigrateType::HighAtomic as usize;

/// 64 MiB with a reserve of two pageblocks, the rest fragmented to single free frames: an order-3 kernel block
/// fails where the caller may wait (nothing to compact with) and comes out of the reserve under a spin lock.
fn staged_atomic() -> TestResult {
    let b = BuddyFrames::staged("highatomic", &[MapRange::new(16 * MIB, 80 * MIB, RangeUse::Free)], Storage::Heap).map_err(|e| alloc::format!("{e:?}"))?;
    kassert!(b.reserve_highatomic_in(0, 2) == 2, "no reserve made");
    let mut held: Vec<u64> = Vec::new();
    held.try_reserve_exact(32 * PAGEBLOCK_FRAMES as usize).map_err(|_| "no room for the list")?;
    while let Some(p) = b.alloc(0, Zone::Normal, Kind::Other) {
        held.push(p);
    }
    kassert!(held.len() as u64 == 30 * PAGEBLOCK_FRAMES, "{} frames outside the reserve, want {}", held.len(), 30 * PAGEBLOCK_FRAMES);
    for p in held.iter().step_by(2) {
        b.free(*p, 0, Kind::Other);
    }
    b.drain_all();
    kassert!(b.has_free_block(0) && !b.has_free_block(1), "not fragmented");
    kassert!(mm_core::reclaim::context_allows(), "the test's context may not reclaim");
    kassert!(b.alloc(3, Zone::Normal, Kind::Other).is_none(), "a caller that may wait took the reserve");
    let lock = ksync::SpinLock::named("mm.ktest.highatomic", ());
    let (block, user) = {
        let _held = lock.lock();
        (b.alloc(3, Zone::Normal, Kind::Other), b.alloc(3, Zone::Normal, Kind::User))
    };
    let p = block.ok_or("no order-3 block under a spin lock")?;
    kassert!(b.pageblock_type(p) == Some(MigrateType::HighAtomic), "the block at {p:#x} is not from the reserve");
    kassert!(user.is_none(), "program memory took the reserve");
    kassert!(b.zone_info(0).free_highatomic == 2 * PAGEBLOCK_FRAMES - 8, "the reserve's free count");
    b.free(p, 3, Kind::Other);
    let i = b.zone_info(0);
    kassert!(i.free_highatomic == 2 * PAGEBLOCK_FRAMES && i.free_by_type[HA][mm_core::PAGEBLOCK_ORDER as usize - 1] == 4, "the block did not merge back into the reserve's halves");
    kassert!(!b.has_free_block(1), "the reserve reads as a free block");
    for p in held.iter().skip(1).step_by(2) {
        b.free(*p, 0, Kind::Other);
    }
    b.check()?;
    Ok(())
}
ktest!("mm.frames.highatomic.staged_atomic", Safe, staged_atomic);

/// The boot allocator: every zone large enough has its reserve (made at boot, sized by the zone then), typed, in
/// blocks below the pageblock order, counted, and left out of what the watermarks and MemAvailable go by.
fn boot_reserve() -> TestResult {
    let b = mm_core::boot_buddy().ok_or("no boot allocator")?;
    let (mut blocks, mut free) = (0, 0);
    for z in 0..2 {
        let i = b.zone_info(z);
        let most = mm_core::buddy::reserve_pageblocks(i.managed_frames);
        kassert!(i.pageblocks[HA] <= most && (most == 0 || i.pageblocks[HA] > 0), "zone {z}: {} pageblocks, at most {most}", i.pageblocks[HA]);
        kassert!(i.free_highatomic <= i.pageblocks[HA] * PAGEBLOCK_FRAMES && i.free_highatomic <= i.free_frames, "zone {z}: {} free", i.free_highatomic);
        kassert!(i.free_by_type[HA][mm_core::PAGEBLOCK_ORDER as usize..].iter().all(|&n| n == 0), "zone {z}: a free pageblock in the reserve");
        blocks += i.pageblocks[HA];
        free += i.free_highatomic;
    }
    kassert!(blocks > 0, "no reserve at all");
    let counted = klog::metrics::snapshot().find(|(n, _)| *n == "mm.frames.pageblock_highatomic").map_or(0, |c| c.1);
    kassert!(counted == blocks, "counter {counted}, pageblocks {blocks}");
    let s = b.stats();
    kassert!(s.highatomic_free_bytes == free * PAGE && s.usable_free_bytes() == s.free_bytes - free * PAGE, "stats: {} reserved of {} free", s.highatomic_free_bytes, s.free_bytes);
    Ok(())
}
ktest!("mm.frames.highatomic.boot_reserve", Safe, boot_reserve);
