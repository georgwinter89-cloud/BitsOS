//! ktests of the buddy frame allocator (MM-1), on the live boot allocator and on staged maps (the Mini-PC's
//! map shape, built in the kernel with heap-held tables). The heap's own ktests are in `heap_ktest`.

use alloc::string::String;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};
use mm_core::map::{normalize, MapRange, RangeUse, LOW_RESERVED};
use mm_core::{boot_buddy, Buddy, BuddyFrames, FrameAlloc, Kind, Storage, Zone, MAX_ORDER, PAGE};

const GIB: u64 = 1 << 30;

fn buddy() -> Result<&'static Buddy, String> {
    boot_buddy().ok_or_else(|| "the kernel did not install the buddy allocator".into())
}

type Ranges = Vec<(u64, u64)>;

/// Free and not-free ranges of a map, normalized (not-free includes the first MiB).
fn split_map(m: &[MapRange]) -> (Ranges, Ranges) {
    let free = normalize(m.iter().filter(|r| r.usage == RangeUse::Free).map(|r| (r.start, r.end)).collect());
    let mut other: Vec<(u64, u64)> = m.iter().filter(|r| r.usage != RangeUse::Free).map(|r| (r.start, r.end)).collect();
    other.push((0, LOW_RESERVED));
    (free, normalize(other))
}

fn overlaps(v: &[(u64, u64)], s: u64, e: u64) -> bool {
    let i = v.partition_point(|r| r.1 <= s);
    i < v.len() && v[i].0 < e
}

fn inside(v: &[(u64, u64)], s: u64, e: u64) -> bool {
    let i = v.partition_point(|r| r.1 <= s);
    i < v.len() && v[i].0 <= s && e <= v[i].1
}

/// Is [p, p + size) memory the map calls free (whole pages, nothing reserved touching it)?
fn map_says_free(m: &[MapRange], p: u64, size: u64) -> bool {
    let (free, other) = split_map(m);
    inside(&free, p, p + size) && !overlaps(&other, p, p + size)
}

/// `map_says_free`, with the ranges `release_module` gave back counted as free (FS-1 5b, INT-A1): page by page,
/// so a block may straddle a released initramfs and the free memory next to it.
fn free_or_released(m: &[MapRange], released: &[(u64, u64)], p: u64, size: u64) -> bool {
    let (free, other) = split_map(m);
    let rel = normalize(released.to_vec());
    (p..p + size).step_by(PAGE as usize).all(|q| (inside(&free, q, q + PAGE) && !overlaps(&other, q, q + PAGE)) || inside(&rel, q, q + PAGE))
}

fn take_all(b: &dyn FrameAlloc, zone: Zone) -> Vec<(u64, u32)> {
    let mut got = Vec::new();
    for order in (0..=MAX_ORDER).rev() {
        while let Some(p) = b.alloc(order, zone, Kind::Other) {
            got.push((p, order));
        }
    }
    got
}

fn map_consistent() -> TestResult {
    // SC-3: other CPUs allocate concurrently; the counters are compared with them held (sched::run_exclusive).
    sched::run_exclusive(10_000_000_000, map_consistent_quiet).map_err(|e| alloc::format!("{e:?}"))?
}

fn map_consistent_quiet() -> TestResult {
    let b = buddy()?;
    b.check()?;
    let m = b.input_map();
    let (free, other) = split_map(m);
    // Count whole pages page by page (a different walk than map::plan's interval arithmetic).
    let mut pages = 0u64;
    for &(s, e) in &free {
        let mut p = s.div_ceil(PAGE) * PAGE;
        while p + PAGE <= e {
            if !overlaps(&other, p, p + PAGE) {
                pages += 1;
            }
            p += PAGE;
        }
    }
    let s = b.stats();
    // FS-1 5b: a released initramfs module is managed memory the input map still names as a module.
    let released = b.released_module_bytes();
    kassert!(s.total_bytes == pages * PAGE + released, "total {} != map {} + released modules {} bytes", s.total_bytes, pages * PAGE, released);
    kassert!(s.free_bytes + s.used_by_kind.iter().sum::<u64>() == s.total_bytes, "free + used != total");
    kassert!(s.leaked_bytes == 0);
    let (ms, me) = b.meta_range();
    kassert!(me > ms && map_says_free(m, ms, me - ms), "tables at {ms:#x}..{me:#x} not in free memory");
    let z = (b.zone_info(0), b.zone_info(1));
    klog::event!(Info, "mm", "ktest.frames", total_mb = s.total_bytes >> 20, free_mb = s.free_bytes >> 20, dma32_mb = (z.0.managed_frames * PAGE) >> 20,
        normal_mb = (z.1.managed_frames * PAGE) >> 20, meta_kb = (me - ms) >> 10, regions = b.region_count());
    Ok(())
}
ktest!("mm.frames.map_consistent", Safe, map_consistent);

fn all_orders() -> TestResult {
    // SC-3: other CPUs allocate concurrently; the counters are compared with them held (sched::run_exclusive).
    sched::run_exclusive(10_000_000_000, all_orders_quiet).map_err(|e| alloc::format!("{e:?}"))?
}

fn all_orders_quiet() -> TestResult {
    let b = buddy()?;
    let m = b.input_map();
    let released = b.released_module_ranges();
    let mut got = 0;
    for order in 0..=MAX_ORDER {
        for zone in [Zone::Normal, Zone::Dma32] {
            let Some(p) = b.alloc(order, zone, Kind::Driver) else {
                // Big blocks may not exist in a small or fragmented machine; small ones must.
                kassert!(order > 10, "order {order} {zone:?} failed");
                continue;
            };
            let size = PAGE << order;
            kassert!(p % size == 0, "order {order} at {p:#x} not aligned");
            kassert!(free_or_released(m, &released, p, size), "order {order} block {p:#x} outside free memory");
            kassert!(zone != Zone::Dma32 || p + size <= 4 * GIB, "DMA32 block above 4 GiB");
            b.free(p, order, Kind::Driver);
            got += 1;
        }
    }
    kassert!(got >= 22, "only {got} allocations");
    b.check()?;
    Ok(())
}
ktest!("mm.frames.all_orders", Safe, all_orders);

fn cycles() -> TestResult {
    // SC-3: other CPUs allocate concurrently; the counters are compared with them held (sched::run_exclusive).
    sched::run_exclusive(10_000_000_000, cycles_quiet).map_err(|e| alloc::format!("{e:?}"))?
}

fn cycles_quiet() -> TestResult {
    let b = buddy()?;
    let mut live: Vec<(u64, u32)> = Vec::with_capacity(4096);
    let mut sorted: Vec<(u64, u32)> = Vec::with_capacity(4096);
    b.drain_all();
    let before = b.stats();
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut rnd = move |n: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % n
    };
    for _ in 0..20_000 {
        if live.is_empty() || (live.len() < 4096 && rnd(2) == 0) {
            let order = if rnd(3) == 0 { rnd(7) as u32 } else { 0 };
            let zone = if rnd(4) == 0 { Zone::Dma32 } else { Zone::Normal };
            if let Some(p) = b.alloc(order, zone, Kind::Other) {
                live.push((p, order));
            }
        } else {
            let (p, o) = live.swap_remove(rnd(live.len() as u64) as usize);
            b.free(p, o, Kind::Other);
        }
    }
    sorted.extend_from_slice(&live);
    sorted.sort_unstable();
    for w in sorted.windows(2) {
        kassert!(w[0].0 + (PAGE << w[0].1) <= w[1].0, "live blocks overlap at {:#x}", w[1].0);
    }
    for (p, o) in live.drain(..) {
        b.free(p, o, Kind::Other);
    }
    b.drain_all();
    let after = b.stats();
    kassert!(after.free_bytes == before.free_bytes, "free {} -> {}", before.free_bytes, after.free_bytes);
    kassert!(after.used_by_kind == before.used_by_kind, "used_by_kind changed");
    b.check()?;
    Ok(())
}
ktest!("mm.frames.cycles", Safe, cycles);

fn dma32_below_4g() -> TestResult {
    let b = buddy()?;
    let mut got = Vec::new();
    for order in (0..=10).chain(core::iter::repeat_n(0, 300)) {
        let p = b.alloc(order, Zone::Dma32, Kind::Dma).ok_or("DMA32 allocation failed")?;
        got.push((p, order));
    }
    let bad = got.iter().find(|&&(p, o)| p + (PAGE << o) > 4 * GIB).copied();
    for &(p, o) in &got {
        b.free(p, o, Kind::Dma);
    }
    kassert!(bad.is_none(), "DMA32 block {bad:x?} reaches above 4 GiB");
    let buf = crate::dma::DmaBuffer::alloc(64 << 10, crate::dma::DmaConstraints::default()).map_err(|e| alloc::format!("{e:?}"))?;
    kassert!(buf.device_addr() + (64 << 10) <= 4 * GIB);
    Ok(())
}
ktest!("mm.frames.dma32_below_4g", Safe, dma32_below_4g);

fn normal_above_4g() -> TestResult {
    let b = buddy()?;
    if b.zone_info(1).managed_frames == 0 {
        return ktest::skip("no RAM above 4 GiB (QEMU -m below ~3.5G); host/tests/mm runs this with 6G");
    }
    for order in [0, 9] {
        let p = b.alloc(order, Zone::Normal, Kind::Other).ok_or("NORMAL allocation failed")?;
        b.free(p, order, Kind::Other);
        kassert!(p >= 4 * GIB, "NORMAL order {order} at {p:#x}: should prefer memory above 4 GiB");
    }
    Ok(())
}
ktest!("mm.frames.normal_above_4g", Safe, normal_above_4g);

fn pcp_drain() -> TestResult {
    // SC-3: other CPUs allocate concurrently; the counters are compared with them held (sched::run_exclusive).
    sched::run_exclusive(10_000_000_000, pcp_drain_quiet).map_err(|e| alloc::format!("{e:?}"))?
}

fn pcp_drain_quiet() -> TestResult {
    let b = buddy()?;
    let frames: Vec<u64> = (0..100).filter_map(|_| b.alloc(0, Zone::Normal, Kind::Other)).collect();
    kassert!(frames.len() == 100);
    for &f in &frames {
        b.free(f, 0, Kind::Other);
    }
    kassert!(b.zone_info(0).cached_frames + b.zone_info(1).cached_frames > 0, "single frames go through the per-CPU list");
    kassert!(b.drain_all() > 0);
    kassert!(b.zone_info(0).cached_frames + b.zone_info(1).cached_frames == 0);
    Ok(())
}
ktest!("mm.frames.pcp_drain", Safe, pcp_drain);

/// The Mini-PC's map built in the kernel (tables on the heap, nothing written to the staged addresses).
fn staged_minipc() -> TestResult {
    let mut r = mm_core::fixtures::typed(&mm_core::fixtures::MINIPC);
    r.push(MapRange::new(0x1_0000_0000, 0x1_0080_0000, RangeUse::Reserved)); // the heap seed
    let b = BuddyFrames::staged("minipc", &r, Storage::Heap).map_err(|e| alloc::format!("{e:?}"))?;
    let mb = b.stats().total_bytes >> 20;
    kassert!((24_300..=24_450).contains(&mb), "about 24,400 MB usable expected, got {mb} MB");
    let free = b.stats().free_bytes;
    let mut all = take_all(&b, Zone::Normal);
    all.extend(take_all(&b, Zone::Dma32));
    kassert!(all.iter().map(|&(_, o)| PAGE << o).sum::<u64>() == free, "not everything handed out once");
    let bad = all.iter().find(|&&(p, o)| !map_says_free(&r, p, PAGE << o)).copied();
    kassert!(bad.is_none(), "block {bad:x?} in reserved/module memory");
    for &(p, o) in &all {
        b.free(p, o, Kind::Other);
    }
    b.check()?;
    kassert!(b.reports().bad_frees.load(core::sync::atomic::Ordering::Relaxed) == 0);
    Ok(())
}
ktest!("mm.frames.staged_minipc", Safe, staged_minipc);

fn bad_free_refused() -> TestResult {
    let r = [
        MapRange::new(0x10_0000, 0x80_0000, RangeUse::Free),
        MapRange::new(0x40_0000, 0x40_1000, RangeUse::Reserved),
        MapRange::new(0x60_0000, 0x70_0000, RangeUse::Module),
    ];
    let b = BuddyFrames::staged("bad-free", &r, Storage::Heap).map_err(|e| alloc::format!("{e:?}"))?;
    let p0 = b.alloc(0, Zone::Dma32, Kind::User).ok_or("alloc")?;
    let p3 = b.alloc(3, Zone::Dma32, Kind::User).ok_or("alloc")?;
    let before = b.stats();
    use mm_core::BadFree::*;
    for (phys, order, why) in [(0x40_0000, 0, OutOfRange), (0x60_0000, 0, OutOfRange), (0x1000, 0, OutOfRange), (p3 + PAGE, 0, NotHead), (p3, 2, OrderMismatch), (p3 + PAGE, 3, Misaligned)] {
        b.free(phys, order, Kind::User);
        kassert!(b.reports().last_bad_free() == Some((phys, why)), "{phys:#x} order {order}: {:?}", b.reports().last_bad_free());
    }
    kassert!(b.stats().free_bytes == before.free_bytes && b.stats().used_by_kind == before.used_by_kind, "a refused free changed something");
    b.free(p0, 0, Kind::User);
    b.free(p0, 0, Kind::User);
    kassert!(b.reports().last_bad_free() == Some((p0, DoubleFree)));
    b.free(p3, 3, Kind::User);
    b.free(p3, 3, Kind::User);
    kassert!(b.reports().last_bad_free() == Some((p3, DoubleFree)));
    kassert!(b.reports().bad_frees.load(core::sync::atomic::Ordering::Relaxed) == 8);
    b.check()?;
    Ok(())
}
ktest!("mm.frames.bad_free_refused", Safe, bad_free_refused);

/// A process thread leaves its root loaded when it ends; dropping that address space must not free the
/// tables the CPU still walks (found by MM-1: with reuse, the next process got the same frame as its root).
/// The CPU then runs on the shared kernel root. Not the root loaded at the start: a kernel thread may hold a
/// process's root lazily (MM-6), as the MCP ktest thread on the Mini-PC does after init ran there (D5, INT-A1d).
fn drop_active_root() -> TestResult {
    let kernel = crate::kernel_root();
    kassert!(kernel != 0, "no kernel root recorded");
    let a = crate::AddressSpace::new().map_err(|e| alloc::format!("{e:?}"))?;
    a.map_fixed(0x40_0000, 4096, crate::Prot::rw(), crate::VmaKind::Anonymous).map_err(|e| alloc::format!("{e:?}"))?;
    let root = a.root();
    a.activate();
    drop(a);
    kassert!(hal::paging::current_root() != root, "the freed root is still loaded");
    kassert!(hal::paging::current_root() == kernel, "back on the kernel root");
    let b = crate::AddressSpace::new().map_err(|e| alloc::format!("{e:?}"))?;
    b.map_fixed(0x40_0000, 4096, crate::Prot::rw(), crate::VmaKind::Anonymous).map_err(|e| alloc::format!("{e:?}"))?;
    b.copy_to_user(0x40_0000, b"ok").map_err(|e| alloc::format!("{e:?}"))?;
    Ok(())
}
ktest!("mm.as.drop_active_root", Safe, drop_active_root);
