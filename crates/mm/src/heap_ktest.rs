//! ktests of the slab heap (MM-3) on the live kernel heap: every class and alignment up to 4 KiB, a random
//! stress run with interleaved frees, Vec growth through every class into page runs, Arc churn across
//! threads (frees on other threads than the allocation), refused bad frees, slabs and page runs going back,
//! a named `SlabCache` with its own kind, and the statistics against the frame allocator's.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use ktest::{kassert, ktest, TestResult};
use mm_core::{Kind, SlabBox, SlabCache, HEAP};

fn frames(kind: Kind) -> Result<u64, String> {
    Ok(mm_core::frames().ok_or("no frame allocator")?.stats().used_by_kind[kind as usize])
}

fn check() -> TestResult {
    sched::run_exclusive(10_000_000_000, quiet_check).map_err(|e| alloc::format!("{e:?}"))?
}

fn quiet_check() -> TestResult {
    HEAP.check().map_err(|e| alloc::format!("heap check: {e:?}"))
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

macro_rules! aligned {
    ($($name:ident = $a:literal),*) => {$(
        #[repr(C, align($a))]
        struct $name([u8; 40]);
    )*};
}
aligned!(A16 = 16, A64 = 64, A256 = 256, A1k = 1024, A2k = 2048, A4k = 4096);

/// `n` values of `T`, checks the alignment and that every value survives the others' allocation.
fn aligned_run<T>(make: impl Fn(u8) -> T, bytes: impl Fn(&T) -> &[u8; 40], align: usize) -> TestResult {
    let boxes: Vec<Box<T>> = (0..300).map(|i| Box::new(make(i as u8))).collect();
    let vecs: Vec<Vec<T>> = (1..40).map(|n| (0..n).map(|i| make(i as u8)).collect()).collect();
    for (i, b) in boxes.iter().enumerate() {
        kassert!((&**b as *const T as usize).is_multiple_of(align), "box misaligned for {align}");
        kassert!(bytes(b).iter().all(|&x| x == i as u8), "box {i} overwritten");
    }
    for v in &vecs {
        kassert!((v.as_ptr() as usize).is_multiple_of(align), "vec misaligned for {align}");
        kassert!(v.iter().enumerate().all(|(i, t)| bytes(t)[39] == i as u8));
    }
    Ok(())
}

fn classes_and_alignments() -> TestResult {
    let mut blocks: Vec<Box<[u8]>> = Vec::new();
    for size in (1..=4200).step_by(3).chain([8, 16, 32, 64, 96, 128, 192, 256, 512, 1024, 2048, 2049, 4096, 4097]) {
        blocks.push(vec![size as u8; size].into_boxed_slice());
    }
    kassert!(blocks.iter().all(|b| b.iter().all(|&x| x == b.len() as u8)), "a block was overwritten");
    aligned_run(|t| A16([t; 40]), |a| &a.0, 16)?;
    aligned_run(|t| A64([t; 40]), |a| &a.0, 64)?;
    aligned_run(|t| A256([t; 40]), |a| &a.0, 256)?;
    aligned_run(|t| A1k([t; 40]), |a| &a.0, 1024)?;
    aligned_run(|t| A2k([t; 40]), |a| &a.0, 2048)?;
    aligned_run(|t| A4k([t; 40]), |a| &a.0, 4096)?;
    drop(blocks);
    check()
}
ktest!("mm.heap.classes", Safe, classes_and_alignments);

fn stress_sizes() -> TestResult {
    let mut rng = Rng(0x5EED_0000_0003_0001);
    let mut live: Vec<Box<[u8]>> = Vec::new();
    for step in 0..40_000u32 {
        if live.is_empty() || rng.below(100) < 55 {
            let size = match rng.below(20) {
                0 => 2049 + rng.below(60_000) as usize,
                1..=4 => 257 + rng.below(1792) as usize,
                _ => 1 + rng.below(256) as usize,
            };
            live.push(vec![(size ^ step as usize) as u8; size].into_boxed_slice());
        } else {
            let b = live.swap_remove(rng.below(live.len() as u64) as usize);
            let tag = b[0];
            kassert!(b.iter().all(|&x| x == tag), "step {step}: a block of {} bytes changed", b.len());
        }
        if step % 10_000 == 0 {
            check()?;
        }
    }
    drop(live);
    check()
}
ktest!("mm.heap.stress_sizes", Safe, stress_sizes);

fn vec_growth() -> TestResult {
    let mut v: Vec<u64> = Vec::new();
    for i in 0..1_000_000u64 {
        v.push(i);
    }
    kassert!(v.iter().sum::<u64>() == 499_999_500_000);
    let mut s = String::new();
    for i in 0..20_000 {
        s.push_str(if i % 2 == 0 { "ab" } else { "cde" });
    }
    kassert!(s.len() == 50_000);
    let mut vs: Vec<Vec<u32>> = (0..64).map(|_| Vec::new()).collect();
    for i in 0..50_000u32 {
        vs[(i % 64) as usize].push(i);
    }
    kassert!(vs.iter().map(|v| v.len()).sum::<usize>() == 50_000);
    v.truncate(10);
    v.shrink_to_fit();
    kassert!(v == (0..10).collect::<Vec<u64>>());
    drop((v, s, vs));
    check()
}
ktest!("mm.heap.vec_growth", Safe, vec_growth);

static PAYLOADS: AtomicUsize = AtomicUsize::new(0);

struct Payload {
    tag: u8,
    data: Vec<u8>,
}
impl Payload {
    fn new(tag: u8, len: usize) -> Payload {
        PAYLOADS.fetch_add(1, Ordering::Relaxed);
        Payload { tag, data: vec![tag; len] }
    }
    fn intact(&self) -> bool {
        self.data.iter().all(|&x| x == self.tag)
    }
}
impl Drop for Payload {
    fn drop(&mut self) {
        PAYLOADS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Four threads make Arcs of varying size, clone them, hand some to a shared pool and drop what others
/// handed in: most objects are freed on another thread than the one that allocated them.
fn arc_churn() -> TestResult {
    let pool: Arc<ksync::Mutex<Vec<Arc<Payload>>>> = Arc::new(ksync::Mutex::new(Vec::new()));
    let broken = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for t in 0..4u64 {
        let (pool, broken) = (pool.clone(), broken.clone());
        let h = sched::spawn("ktest.arc", sched::Class::Normal, move || {
            let mut rng = Rng(0xA5C0_0000_0000_0001 ^ (t << 32));
            let mut mine: Vec<Arc<Payload>> = Vec::new();
            for i in 0..6000u32 {
                let p = Arc::new(Payload::new(rng.next() as u8, rng.below(600) as usize));
                for _ in 0..rng.below(3) {
                    mine.push(p.clone());
                }
                match rng.below(4) {
                    0 => pool.lock().push(p),
                    1 => {
                        if let Some(q) = pool.lock().pop()
                            && !q.intact()
                        {
                            broken.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    _ => mine.push(p),
                }
                if mine.len() > 200 {
                    let q = mine.swap_remove(rng.below(mine.len() as u64) as usize);
                    if !q.intact() {
                        broken.fetch_add(1, Ordering::Relaxed);
                    }
                }
                if i % 64 == 0 {
                    sched::yield_now();
                }
            }
        });
        handles.push(h.map_err(|e| alloc::format!("spawn: {e:?}"))?);
    }
    for h in &handles {
        kassert!(h.join_timeout(60_000_000_000), "an Arc thread did not finish within 60 s");
    }
    kassert!(broken.load(Ordering::Relaxed) == 0, "{} payloads changed while shared", broken.load(Ordering::Relaxed));
    pool.lock().clear();
    kassert!(PAYLOADS.load(Ordering::Relaxed) == 0, "{} payloads never dropped", PAYLOADS.load(Ordering::Relaxed));
    check()
}
ktest!("mm.heap.arc_churn", Safe, arc_churn);

fn bad_frees() -> TestResult {
    // SC-3: the probe compares the heap's numbers before and after; other CPUs allocate concurrently, so they are
    // held meanwhile (sched::run_exclusive), as in the other consistency tests.
    sched::run_exclusive(10_000_000_000, bad_frees_quiet).map_err(|e| alloc::format!("{e:?}"))?
}

fn bad_frees_quiet() -> TestResult {
    let refused = HEAP.probe_bad_frees()?;
    kassert!(refused == mm_core::PROBE_REFUSALS, "{refused} bad frees refused, {} expected", mm_core::PROBE_REFUSALS);
    quiet_check()
}
ktest!("mm.heap.bad_frees_refused", Safe, bad_frees);

/// Many small blocks, then a big one: their frames go back (slabs after a shrink, page runs at once and
/// exactly).
fn slabs_return() -> TestResult {
    // SC-3: other CPUs allocate concurrently; the counters are compared with them held (sched::run_exclusive).
    sched::run_exclusive(10_000_000_000, slabs_return_quiet).map_err(|e| alloc::format!("{e:?}"))?
}

fn slabs_return_quiet() -> TestResult {
    HEAP.shrink();
    let before = frames(Kind::Heap)?;
    let mut boxes: Vec<Box<[u8; 1024]>> = Vec::with_capacity(16_000);
    for i in 0..16_000 {
        boxes.push(Box::new([i as u8; 1024]));
    }
    // Same reason as for `big` below: the boxes must exist while the frames are counted, in every build.
    core::hint::black_box(&mut boxes);
    let full = frames(Kind::Heap)?;
    // The seed's free memory serves slabs first (up to 8 MiB), the rest comes from the buddy allocator.
    kassert!(full >= before + (7 << 20), "16 MB of 1 KiB blocks took only {} bytes of frames", full - before);
    kassert!(boxes.iter().enumerate().all(|(i, b)| b[1023] == i as u8), "contents survive");
    drop(boxes);
    HEAP.shrink();
    let after = frames(Kind::Heap)?;
    kassert!(after <= before + (64 << 10), "slabs not returned: heap frames {before} -> {full} -> {after}");
    // The release build lets LLVM drop an allocation nobody reads (`__rust_alloc` may be elided): without the
    // black_box the 3 MiB block never reached the heap and the count stayed at `after`. black_box makes the
    // pointer escape, so the block is really allocated while the frames are counted.
    let big = core::hint::black_box(vec![7u8; 3 << 20]);
    let held = frames(Kind::Heap)?;
    kassert!(held == after + (3 << 20), "3 MiB = exactly 768 pages: heap frames {after} -> {held}");
    kassert!(big[0] == 7 && big[(3 << 20) - 1] == 7, "big block contents");
    drop(big);
    kassert!(frames(Kind::Heap)? == after, "a big block's frames go back on drop");
    quiet_check()
}
ktest!("mm.heap.slabs_return", Safe, slabs_return);

static KTEST_OBJS: SlabCache = SlabCache::new("ktest.obj", 200, 64, Kind::Driver);

#[repr(C, align(64))]
struct Obj {
    id: u64,
    body: [u8; 180],
}

fn named_cache() -> TestResult {
    let before = frames(Kind::Driver)?;
    let objs: Vec<SlabBox<Obj>> = (0..5000u64)
        .map(|id| SlabBox::new(&KTEST_OBJS, Obj { id, body: [id as u8; 180] }).map_err(drop))
        .collect::<Result<_, _>>()
        .map_err(|_| "SlabBox::new failed")?;
    kassert!(objs.iter().all(|o| (&**o as *const Obj as usize).is_multiple_of(64)));
    kassert!(objs.iter().enumerate().all(|(i, o)| o.id == i as u64 && o.body[179] == i as u8));
    let st = KTEST_OBJS.stats();
    kassert!(st.live == 5000 && st.object_size == 256, "{st:?}");
    let held = frames(Kind::Driver)? - before;
    // Slabs come from the seed's free memory first, then from the buddy allocator (charged to Driver).
    kassert!(held <= st.slabs * st.slab_bytes as u64, "Driver frames {held} > slabs {}", st.slabs * st.slab_bytes as u64);
    let h = HEAP.stats();
    kassert!(h.live_by_kind[Kind::Driver as usize] >= 5000 * 256);
    KTEST_OBJS.check().map_err(|e| alloc::format!("{e:?}"))?;
    let back = objs.into_iter().map(SlabBox::into_inner).map(|o| o.id).sum::<u64>();
    kassert!(back == (0..5000).sum::<u64>());
    KTEST_OBJS.shrink();
    kassert!(frames(Kind::Driver)? == before, "named cache slabs not returned");
    kassert!(SlabBox::new(&KTEST_OBJS, [0u8; 300]).is_err(), "a T bigger than the object is refused");
    check()
}
ktest!("mm.heap.named_cache", Safe, named_cache);

fn stats_consistent() -> TestResult {
    // SC-3: other CPUs allocate concurrently; the counters are compared with them held (sched::run_exclusive).
    sched::run_exclusive(10_000_000_000, stats_consistent_quiet).map_err(|e| alloc::format!("{e:?}"))?
}

fn stats_consistent_quiet() -> TestResult {
    HEAP.shrink();
    quiet_check()?;
    let s = HEAP.stats();
    let heap_frames = frames(Kind::Heap)?;
    kassert!(s.held_by_kind[Kind::Heap as usize] == heap_frames, "heap holds {} bytes, frame allocator says {heap_frames}", s.held_by_kind[Kind::Heap as usize]);
    kassert!(s.requested_bytes <= s.live_bytes + s.large_bytes, "requested {} > live {} + large {}", s.requested_bytes, s.live_bytes, s.large_bytes);
    kassert!(s.magazine_cpus >= 1, "no per-CPU magazines");
    kassert!(s.seed.total == 8 << 20 && s.seed.carved <= s.seed.total && s.seed.free <= s.seed.carved);
    kassert!(s.magazine_objects.iter().sum::<u64>() == 0, "shrink left objects in magazines");
    klog::event!(Info, "mm", "ktest.heap", slab_kb = s.slab_bytes >> 10, live_kb = s.live_bytes >> 10, large_kb = s.large_bytes >> 10,
        requested_kb = s.requested_bytes >> 10, frames_kb = heap_frames >> 10, seed_carved_kb = s.seed.carved >> 10,
        seed_free_kb = s.seed.free >> 10, magazine_cpus = s.magazine_cpus, cpus = hal::percpu::cpu_count());
    Ok(())
}
ktest!("mm.heap.stats_consistent", Safe, stats_consistent);

/// Speed of the paths (judged on the Mini-PC only; here a generous functional bound): a 64-byte Box made and
/// dropped (magazine hit), 64 KiB of 64-byte boxes made then dropped (refills and flushes through the
/// cache), and a 16 KiB block (an exact page run from the buddy allocator).
fn speed() -> TestResult {
    let pairs = 100_000u64;
    let t0 = time::now_ns();
    for i in 0..pairs {
        let b = Box::new([i as u8; 64]);
        core::hint::black_box(&b);
    }
    let hit_ns = (time::now_ns() - t0) / pairs;
    let t1 = time::now_ns();
    for _ in 0..100 {
        let v: Vec<Box<[u8; 64]>> = (0..1024).map(|i| Box::new([i as u8; 64])).collect();
        core::hint::black_box(&v);
    }
    let batch_ns = (time::now_ns() - t1) / (100 * 1024);
    let t2 = time::now_ns();
    for i in 0..10_000u64 {
        let v = vec![i as u8; 16 << 10];
        core::hint::black_box(&v);
    }
    let run_ns = (time::now_ns() - t2) / 10_000;
    klog::event!(Info, "mm", "ktest.heap_speed", magazine_pair_ns = hit_ns, cache_pair_ns = batch_ns, page_run_16k_ns = run_ns;
        "Mini-PC: magazine pair should be well below 100 ns");
    kassert!(hit_ns < 100_000 && batch_ns < 100_000 && run_ns < 1_000_000, "heap paths far too slow even for QEMU");
    check()
}
ktest!("mm.heap.speed", Safe, speed);
