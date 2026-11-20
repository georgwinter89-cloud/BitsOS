//! ktests of the heap's scattered large blocks (`scatter`) on purposely fragmented memory: no 4 MiB block is
//! free, so a 32 MiB Vec only exists through the fallback; it is written and read in full, grows by realloc and
//! goes back; a free with interrupts off waits on the deferred list and is drained soon after.

use alloc::string::String;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};
use mm_core::{Kind, ScatterStats, Zone, HEAP, HUGE_ORDER, PAGE};

const MIB: usize = 1 << 20;
/// 4 MiB: the smallest block the tests ask for (order 10) and above.
const ORDER: u32 = 10;

/// Every free block of order 10 or more taken, split, and all but its first frame given back: until the drop no
/// 4 MiB run is free anywhere, while the machine keeps its 2 MiB blocks (THP, DMA buffers, kernel stacks) and
/// loses only 4 KiB per 4 MiB.
struct Fragmented {
    heads: Vec<u64>,
}

impl Fragmented {
    fn new() -> Result<Fragmented, String> {
        let f = mm_core::frames().ok_or("no frame allocator")?;
        let b = mm_core::boot_buddy().ok_or("no buddy allocator")?;
        let mut heads = Vec::new();
        heads.try_reserve(1 << 16).map_err(|_| "no heap for the block list")?;
        while heads.len() < heads.capacity() {
            let Some(p) = f.alloc_light(ORDER, Zone::Normal, Kind::User) else { break };
            if !mm_core::split_user_block(p, ORDER) {
                f.free(p, ORDER, Kind::User);
                return Err(String::from("a fresh 4 MiB block did not split"));
            }
            for i in 1..1u64 << ORDER {
                f.free(p + i * PAGE, 0, Kind::User);
            }
            heads.push(p);
        }
        // Into the buddy lists, where they merge up to 2 MiB.
        b.drain_all();
        Ok(Fragmented { heads })
    }

    /// Bytes given back.
    fn loose_bytes(&self) -> usize {
        self.heads.len() * (4 * MIB - PAGE as usize)
    }
}

impl Drop for Fragmented {
    fn drop(&mut self) {
        if let Some(f) = mm_core::frames() {
            for &p in &self.heads {
                f.free(p, 0, Kind::User);
            }
        }
        if let Some(b) = mm_core::boot_buddy() {
            b.drain_all();
        }
    }
}

/// After the drop the 4 MiB blocks are back.
fn whole_again() -> TestResult {
    let f = mm_core::frames().ok_or("no frame allocator")?;
    let p = f.alloc_light(ORDER, Zone::Normal, Kind::User).ok_or("no 4 MiB block free after the fragmenter went")?;
    f.free(p, ORDER, Kind::User);
    let q = f.alloc_light(HUGE_ORDER, Zone::Normal, Kind::User).ok_or("no 2 MiB block free after the fragmenter went")?;
    f.free(q, HUGE_ORDER, Kind::User);
    Ok(())
}

fn scattered(p: *const u8) -> bool {
    let v = p as u64;
    bootinfo::layout::virt_to_phys(v).is_none() && (bootinfo::layout::VMAP_BASE..bootinfo::layout::VMAP_END).contains(&v)
}

fn stats() -> ScatterStats {
    HEAP.stats().scatter
}

fn metric(name: &str) -> u64 {
    klog::metrics::snapshot().find(|m| m.0 == name).map_or(0, |m| m.1)
}

fn pattern(i: usize) -> u8 {
    (i ^ (i >> 12).wrapping_mul(31)) as u8
}

/// Fills [from, len) of `v` (its capacity) with the pattern and checks [0, len).
fn fill_and_check(v: &mut Vec<u8>, from: usize, len: usize) -> TestResult {
    v.resize(len, 0);
    for (i, b) in v.iter_mut().enumerate().skip(from) {
        *b = pattern(i);
    }
    let bad = v.iter().enumerate().position(|(i, &b)| b != pattern(i));
    kassert!(bad.is_none(), "byte {bad:?} of {len} lost its value");
    Ok(())
}

fn scatter() -> TestResult {
    let (s0, m0) = (stats(), metric("mm.heap.scatter_allocs"));
    let (pages0, frees0) = (metric("mm.heap.scatter_pages"), metric("mm.heap.scatter_frees"));
    let frag = Fragmented::new()?;
    kassert!(frag.loose_bytes() >= 128 * MIB, "only {} MiB of single frames to work with", frag.loose_bytes() / MIB);
    let mut v: Vec<u8> = Vec::new();
    v.try_reserve_exact(32 * MIB).map_err(|_| "no 32 MiB block although single frames are free")?;
    kassert!(scattered(v.as_ptr()), "the 32 MiB block {:p} is not in the vmap area", v.as_ptr());
    let s1 = stats();
    // Other threads may scatter meanwhile: the numbers move at least by this test's blocks.
    kassert!(s1.allocs > s0.allocs && s1.held_bytes >= s0.held_bytes + (32 * MIB) as u64, "{s0:?} -> {s1:?}");
    fill_and_check(&mut v, 0, 32 * MIB)?;
    v.try_reserve_exact(32 * MIB).map_err(|_| "the block did not grow to 64 MiB")?;
    kassert!(scattered(v.as_ptr()), "the grown block is not in the vmap area");
    fill_and_check(&mut v, 32 * MIB, 64 * MIB)?;
    let s2 = stats();
    kassert!(s2.allocs >= s1.allocs + 1 && s2.frees > s1.frees, "realloc: {s1:?} -> {s2:?}");
    kassert!(s2.held_bytes < s1.held_bytes + (64 * MIB) as u64, "realloc kept the old block: {s1:?} -> {s2:?}");
    drop(v);
    let s3 = stats();
    kassert!(s3.frees > s2.frees && s3.held_bytes < s2.held_bytes, "free: {s2:?} -> {s3:?}");
    kassert!(metric("mm.heap.scatter_allocs") >= m0 + 2 && metric("mm.heap.scatter_frees") >= frees0 + 2, "counters did not move");
    kassert!(metric("mm.heap.scatter_pages") >= pages0 + (96 * MIB) as u64 / PAGE, "mm.heap.scatter_pages did not count the pages");
    drop(frag);
    whole_again()?;
    sched::run_exclusive(10_000_000_000, || HEAP.check().map_err(|e| alloc::format!("heap check: {e:?}"))).map_err(|e| alloc::format!("{e:?}"))?
}
ktest!("mm.heap.scatter", Safe, scatter);

fn scatter_deferred() -> TestResult {
    let (s0, d0) = (stats(), metric("mm.heap.scatter_deferred"));
    let frag = Fragmented::new()?;
    let mut w: Vec<u8> = Vec::new();
    w.try_reserve_exact(4 * MIB).map_err(|_| "no 4 MiB block although single frames are free")?;
    kassert!(scattered(w.as_ptr()), "the 4 MiB block is not in the vmap area");
    w.resize(4 * MIB, 0x5A);
    let frees = stats().frees;
    {
        let _off = hal::cpu::IrqGuard::new();
        drop(w);
    }
    // mm.report may have drained it already (the kick), so only the count of deferred frees is certain.
    let inside = stats();
    kassert!(inside.deferred > s0.deferred, "not deferred: {s0:?} -> {inside:?}");
    kassert!(metric("mm.heap.scatter_deferred") > d0, "mm.heap.scatter_deferred did not count it");
    let t0 = time::now_ns();
    let drained = |s: &ScatterStats| s.frees > frees && s.waiting_bytes == 0;
    while !drained(&stats()) && time::now_ns() - t0 < 2_000_000_000 {
        sched::sleep_ms(1);
    }
    let (s1, ms) = (stats(), (time::now_ns() - t0) / 1_000_000);
    kassert!(drained(&s1), "not drained in {ms} ms: {s1:?}");
    // The kick wakes mm.report at once; its 1 s period alone would be too slow under memory pressure.
    kassert!(ms < 500, "drained only after {ms} ms");
    drop(frag);
    whole_again()?;
    klog::event!(Info, "mm", "ktest.scatter_deferred", drained_ms = ms);
    Ok(())
}
ktest!("mm.heap.scatter_deferred", Safe, scatter_deferred);

/// A scattered block too large to be kept costs one shootdown round when it goes (frames and range together,
/// `hal::paging::unmap_kernel_frames`); before, two (Mini-PC 12.11.2026: 15 interrupts per block on sixteen CPUs).
/// Its frames come back: the frame allocator holds as many heap frames as before the blocks.
fn scatter_one_round() -> TestResult {
    const BLOCKS: u64 = 16;
    let frag = Fragmented::new()?;
    kassert!(frag.loose_bytes() >= 64 * MIB, "only {} MiB of single frames to work with", frag.loose_bytes() / MIB);
    let heap_frames = || mm_core::frames().map_or(0, |f| f.stats().used_by_kind[Kind::Heap as usize]);
    let (h0, k0) = (heap_frames(), metric("mm.heap.scatter_kept"));
    let mut rounds = 0;
    for i in 0..BLOCKS {
        let mut v: Vec<u8> = Vec::new();
        v.try_reserve_exact(4 * MIB).map_err(|_| alloc::format!("block {i}: no 4 MiB block although single frames are free"))?;
        kassert!(scattered(v.as_ptr()), "block {i} {:p} is not in the vmap area", v.as_ptr());
        v.resize(4 * MIB, 0x5A);
        // Only the free's own window is counted (not the allocation and the 4 MiB of writes).
        let r0 = hal::paging::tlb::stats().kernel;
        drop(v);
        rounds += hal::paging::tlb::stats().kernel - r0;
    }
    let (h1, k1) = (heap_frames(), metric("mm.heap.scatter_kept"));
    // Another thread's kernel shootdown inside one of the sixteen short windows (a stack reaped) is the slack: two.
    kassert!(rounds <= BLOCKS + 2, "{rounds} kernel shootdown rounds for {BLOCKS} freed blocks");
    kassert!(k1 == k0, "{} frames kept out of use", k1 - k0);
    // Bytes (used_by_kind); 64 frames of slack for other threads' heap growth meanwhile.
    kassert!(h1 <= h0 + 64 * PAGE, "heap bytes {h0} -> {h1}: the blocks' frames did not come back");
    drop(frag);
    whole_again()
}
ktest!("mm.heap.scatter_one_round", Safe, scatter_one_round);
