//! The `mm.report` thread: turns the frame allocator's and the heap's counted troubles into events (B6).
//! mm-core cannot emit them itself (it runs under the heap's and klog's locks), so this Background thread
//! flushes them once a second, holding no lock. MM-6: the same for the TLB shootdown's counters (`tlb`). It also
//! gives back the heap's deferred scattered blocks (`scatter`), woken for them.

const PERIOD_MS: u64 = 1000;

fn start() -> Result<(), &'static str> {
    mm_core::flush_reports();
    let h = mm_core::HEAP.stats();
    klog::event!(Info, "mm", "heap.ready", classes = h.classes.len(), magazine_cpus = h.magazine_cpus,
        slab_kb = h.slab_bytes >> 10, large_kb = h.large_bytes >> 10, requested_kb = h.requested_bytes >> 10,
        frames_kb = h.held_by_kind[mm_core::Kind::Heap as usize] >> 10, seed_kb = h.seed.total >> 10,
        seed_carved_kb = h.seed.carved >> 10; "slab heap with per-CPU magazines");
    sched::spawn("mm.report", sched::Class::Background, || {
        let period = PERIOD_MS * 1_000_000;
        let mut last = time::now_ns();
        loop {
            // A heap free deferred under a spin lock or with interrupts off wakes the thread at once: under memory
            // pressure its frames should not wait for the next period.
            crate::scatter::wait_kick(period.saturating_sub(time::now_ns().saturating_sub(last)));
            mm_core::HEAP.drain_scattered();
            if time::now_ns().saturating_sub(last) < period {
                continue;
            }
            last = time::now_ns();
            mm_core::flush_reports();
            crate::tlb::report();
            crate::commit::report();
        }
    })
    .map(|_| ())
    .map_err(|_| "mm.report thread not started")
}

kreg::initcall!(kreg::Level::Late, "mm.report", start);
