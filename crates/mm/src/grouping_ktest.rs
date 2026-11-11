//! W35 MM-1 ktests of grouping by mobility: on a staged allocator (heap-held tables, the pool untouched) the kinds
//! land in pageblocks of their type and the fallbacks follow Linux 7.x; on the boot allocator the per-type counts
//! agree with the lists and the kernel's own memory sits in Unmovable pageblocks.

use alloc::vec::Vec;
use core::sync::atomic::Ordering::Relaxed;
use ktest::{kassert, ktest, TestResult};
use mm_core::map::{MapRange, RangeUse};
use mm_core::{BuddyFrames, FrameAlloc, Kind, MigrateType, Storage, Zone, ORDERS, PAGE};

const MIB: u64 = 1 << 20;

fn staged(start: u64, end: u64) -> Result<BuddyFrames, alloc::string::String> {
    BuddyFrames::staged("grouping", &[MapRange::new(start, end, RangeUse::Free)], Storage::Heap).map_err(|e| alloc::format!("{e:?}"))
}

/// 64 MiB, a mix of user and kernel frames, half freed, a new mix: no frame outside its type, nothing mixed.
fn staged_mix() -> TestResult {
    let b = staged(16 * MIB, 80 * MIB)?;
    let mut held: Vec<(u64, Kind)> = Vec::new();
    held.try_reserve_exact(18_000).map_err(|_| "no room for the list")?;
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut coin = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x & 1 == 0
    };
    for _ in 0..12_000 {
        let k = if coin() { Kind::User } else { Kind::Heap };
        held.push((b.alloc(0, Zone::Normal, k).ok_or("staged allocator empty")?, k));
    }
    let mut kept: Vec<(u64, Kind)> = Vec::new();
    kept.try_reserve_exact(18_000).map_err(|_| "no room for the list")?;
    for (p, k) in held {
        if coin() { b.free(p, 0, k) } else { kept.push((p, k)) }
    }
    for _ in 0..6000 {
        let k = if coin() { Kind::PageCache } else { Kind::PageTable };
        kept.push((b.alloc(0, Zone::Normal, k).ok_or("staged allocator empty")?, k));
    }
    let wrong = kept.iter().filter(|&&(p, k)| b.pageblock_type(p) != Some(MigrateType::of(k))).count();
    kassert!(wrong == 0, "{wrong} frames in a pageblock of another type");
    let r = b.reports();
    kassert!(r.steals.load(Relaxed) == 0 && r.whole_blocks.load(Relaxed) == 0, "a fallback with free pageblocks left");
    for (p, k) in kept {
        b.free(p, 0, k);
    }
    b.check()?;
    Ok(())
}
ktest!("mm.frames.grouping.staged_mix", Safe, staged_mix);

/// Two full Movable pageblocks: a kernel frame claims the one at least half free, steals from the other.
fn staged_fallbacks() -> TestResult {
    let (a, bb) = (2 * MIB, 4 * MIB);
    let b = staged(a, 6 * MIB)?;
    let mut n = 0;
    while b.alloc(0, Zone::Normal, Kind::User).is_some() {
        n += 1;
    }
    kassert!(n == 1024, "{n} frames in two pageblocks");
    for i in 0..300 {
        b.free(a + i * PAGE, 0, Kind::User);
    }
    for i in 0..100 {
        b.free(bb + i * 4 * PAGE, 0, Kind::User);
    }
    b.drain_all();
    let p = b.alloc(0, Zone::Normal, Kind::Driver).ok_or("no frame")?;
    kassert!((a..bb).contains(&p), "{p:#x} not from the block with the most free frames");
    kassert!(b.pageblock_type(a) == Some(MigrateType::Unmovable) && b.pageblock_type(bb) == Some(MigrateType::Movable));
    for _ in 0..299 {
        let q = b.alloc(0, Zone::Normal, Kind::Driver).ok_or("no frame")?;
        kassert!((a..bb).contains(&q), "{q:#x} left the claimed block");
    }
    let s = b.alloc(0, Zone::Normal, Kind::Driver).ok_or("no frame")?;
    kassert!((bb..bb + 2 * MIB).contains(&s) && b.pageblock_type(bb) == Some(MigrateType::Movable), "a steal keeps the type");
    let r = b.reports();
    kassert!((r.whole_blocks.load(Relaxed), r.steals.load(Relaxed)) == (1, 1));
    b.check()?;
    Ok(())
}
ktest!("mm.frames.grouping.staged_fallbacks", Safe, staged_fallbacks);

/// Whole free pageblocks of a staged allocator (free blocks of the pageblock order and up, in pageblocks).
fn free_pageblocks(b: &BuddyFrames) -> u64 {
    let pb = mm_core::PAGEBLOCK_ORDER as usize;
    (0..2).map(|z| b.zone_info(z).free_blocks.iter().enumerate().skip(pb).map(|(o, &n)| n << (o - pb)).sum::<u64>()).sum()
}

/// The Mini-PC's "no free 2 MiB block": user and kernel frames interleaved in time, the user frames freed. Grouped,
/// every pageblock without a kernel frame is whole again; with the kernel frames freed too, all 32 are.
fn fragment_repair() -> TestResult {
    let b = staged(16 * MIB, 80 * MIB)?;
    let mut users: Vec<u64> = Vec::new();
    let mut heap: Vec<u64> = Vec::new();
    users.try_reserve_exact(6000).map_err(|_| "no room for the list")?;
    heap.try_reserve_exact(6000).map_err(|_| "no room for the list")?;
    for i in 0..12_000 {
        if i % 2 == 0 {
            users.push(b.alloc(0, Zone::Normal, Kind::User).ok_or("staged allocator empty")?);
        } else {
            heap.push(b.alloc(0, Zone::Normal, Kind::Heap).ok_or("staged allocator empty")?);
        }
    }
    let unmovable = b.zone_info(0).pageblocks[MigrateType::Unmovable as usize] + b.zone_info(1).pageblocks[MigrateType::Unmovable as usize];
    kassert!(unmovable <= 13, "6000 heap frames took {unmovable} pageblocks");
    for p in users {
        b.free(p, 0, Kind::User);
    }
    b.drain_all();
    let whole = free_pageblocks(&b);
    kassert!(whole == 32 - unmovable, "{whole} whole pageblocks, want {}", 32 - unmovable);
    for p in heap {
        b.free(p, 0, Kind::Heap);
    }
    b.drain_all();
    let whole = free_pageblocks(&b);
    kassert!(whole == 32, "{whole} whole pageblocks after every free");
    b.check()?;
    Ok(())
}
ktest!("mm.frames.grouping.fragment_repair", Safe, fragment_repair);

/// The boot allocator: per-type free counts add up to the per-order ones, every pageblock has a type, and the
/// kernel's own memory has made Unmovable pageblocks while user memory and the cache keep Movable ones.
fn boot_counts() -> TestResult {
    let b = mm_core::boot_buddy().ok_or("no boot allocator")?;
    let (mut unmovable, mut movable) = (0, 0);
    for z in 0..2 {
        let i = b.zone_info(z);
        for o in 0..ORDERS {
            let sum: u64 = i.free_by_type.iter().map(|t| t[o]).sum();
            kassert!(sum == i.free_blocks[o], "zone {z} order {o}: {sum} by type, {} in all", i.free_blocks[o]);
        }
        unmovable += i.pageblocks[MigrateType::Unmovable as usize];
        movable += i.pageblocks[MigrateType::Movable as usize];
    }
    kassert!(unmovable > 0 && movable > 0, "pageblocks: {unmovable} unmovable, {movable} movable");
    let h = b.alloc(0, Zone::Normal, Kind::Heap).ok_or("no frame")?;
    let t = b.pageblock_type(h);
    b.free(h, 0, Kind::Heap);
    kassert!(t.is_some(), "a frame outside every region");
    Ok(())
}
ktest!("mm.frames.grouping.boot_counts", Safe, boot_counts);
