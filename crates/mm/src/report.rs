//! The `mm.report` thread: turns the frame allocator's and the heap's counted troubles into events (B6).
//! mm-core cannot emit them itself (it runs under the heap's and klog's locks), so this Background thread
//! flushes them once a second, holding no lock. MM-6: the same for the TLB shootdown's counters (`tlb`).

const PERIOD_MS: u64 = 1000;

fn start() -> Result<(), &'static str> {
    mm_core::flush_reports();
    let h = mm_core::HEAP.stats();
    klog::event!(Info, "mm", "heap.ready", classes = h.classes.len(), magazine_cpus = h.magazine_cpus,
        slab_kb = h.slab_bytes >> 10, large_kb = h.large_bytes >> 10, requested_kb = h.requested_bytes >> 10,
        frames_kb = h.held_by_kind[mm_core::Kind::Heap as usize] >> 10, seed_kb = h.seed.total >> 10,
        seed_carved_kb = h.seed.carved >> 10; "slab heap with per-CPU magazines");
    sched::spawn("mm.report", sched::Class::Background, || loop {
        sched::sleep_ms(PERIOD_MS);
        mm_core::flush_reports();
        crate::tlb::report();
        crate::commit::report();
    })
    .map(|_| ())
    .map_err(|_| "mm.report thread not started")
}

kreg::initcall!(kreg::Level::Late, "mm.report", start);
