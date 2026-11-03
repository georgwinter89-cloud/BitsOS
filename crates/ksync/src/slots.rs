//! The statistics of one lock class, kept per CPU (PF-2): every CPU counts in its own cache-line slot, so
//! taking a lock writes no line another CPU writes; readers (`stats`) sum the slots (the `*_max` fields
//! combine with max), as Linux's `lock_stats` sums its per-CPU `cpu_lock_stats` (linux-6.6/kernel/locking/
//! lockdep.c:244, :294). Unlike Linux there is no fixed class bound (R1): each class owns a list of slot
//! chunks that covers CPUs [0, covered); a CPU outside it (no chunk yet: before `prepare_cpus`, an
//! allocation that could not be made) counts in the class's `shared` slot, so no count is ever lost.
//!
//! Chunks are allocated only where allocation is allowed (`prepare_cpus`, a class's creation, a lock
//! instance's first resolution with `may_alloc`), never on the lock path, and never freed (classes live
//! forever). The counters stay atomic: a thread can move to another CPU between reading its index and
//! counting, so two threads may share a slot for a moment.

use core::alloc::Layout;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering, fence};

/// One CPU's counters of one class. Times are TSC ticks.
#[repr(align(64))]
pub(crate) struct Slot {
    pub acquired: AtomicU64,
    pub contended: AtomicU64,
    pub wait_ticks: AtomicU64,
    pub wait_max: AtomicU64,
    pub hold_ticks: AtomicU64,
    pub hold_max: AtomicU64,
    pub try_failed: AtomicU64,
    pub timeouts: AtomicU64,
}

impl Slot {
    const fn new() -> Self {
        Slot {
            acquired: AtomicU64::new(0),
            contended: AtomicU64::new(0),
            wait_ticks: AtomicU64::new(0),
            wait_max: AtomicU64::new(0),
            hold_ticks: AtomicU64::new(0),
            hold_max: AtomicU64::new(0),
            try_failed: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
        }
    }
}

/// The summed counters of a class.
#[derive(Clone, Copy, Default)]
pub(crate) struct Sums {
    pub acquired: u64,
    pub contended: u64,
    pub wait_ticks: u64,
    pub wait_max: u64,
    pub hold_ticks: u64,
    pub hold_max: u64,
    pub try_failed: u64,
    pub timeouts: u64,
}

impl Sums {
    fn add(&mut self, s: &Slot) {
        let r = Ordering::Relaxed;
        self.acquired = self.acquired.wrapping_add(s.acquired.load(r));
        self.contended = self.contended.wrapping_add(s.contended.load(r));
        self.wait_ticks = self.wait_ticks.wrapping_add(s.wait_ticks.load(r));
        self.wait_max = self.wait_max.max(s.wait_max.load(r));
        self.hold_ticks = self.hold_ticks.wrapping_add(s.hold_ticks.load(r));
        self.hold_max = self.hold_max.max(s.hold_max.load(r));
        self.try_failed = self.try_failed.wrapping_add(s.try_failed.load(r));
        self.timeouts = self.timeouts.wrapping_add(s.timeouts.load(r));
    }
}

/// Slots of CPUs [first, first + len). Immutable once published.
struct Chunk {
    first: usize,
    len: usize,
    slots: *mut Slot,
    next: *mut Chunk,
}

pub(crate) struct Stats {
    /// CPUs without a slot.
    shared: Slot,
    /// Newest (highest CPUs) first.
    chunks: AtomicPtr<Chunk>,
    /// CPUs [0, covered) have a slot or get one as soon as its chunk is pushed.
    covered: AtomicUsize,
}

/// How many CPUs every class should cover (raised by `prepare_cpus`).
static TARGET: AtomicUsize = AtomicUsize::new(0);

/// The CPU count classes should cover now.
pub(crate) fn target() -> usize {
    TARGET.load(Ordering::SeqCst).max(crate::cpu::cpu_slots_seen())
}

/// Raises the target and covers every existing class (allocates: callers are `prepare_cpus`).
pub(crate) fn cover_all(n: usize) {
    TARGET.fetch_max(n, Ordering::SeqCst);
    // Pairs with the fence in `class::resolve` after a new class is published: either that class is in the
    // list walked here, or its creator reads the raised target.
    fence(Ordering::SeqCst);
    let t = target();
    crate::class::for_each(|c| c.stats.cover(t));
}

impl Stats {
    pub(crate) const fn new() -> Self {
        Stats { shared: Slot::new(), chunks: AtomicPtr::new(core::ptr::null_mut()), covered: AtomicUsize::new(0) }
    }

    /// The slot of the CPU this runs on (lock path: no allocation, no lock).
    #[inline]
    pub(crate) fn here(&self) -> &Slot {
        self.slot(crate::cpu::cpu_index())
    }

    fn slot(&self, cpu: usize) -> &Slot {
        let mut p = self.chunks.load(Ordering::Acquire);
        while !p.is_null() {
            // SAFETY: chunks are published with Release after being fully built and are never changed. A chunk
            // reachable from a published `Stats` is never freed; `free_unpublished` frees only the chunks of a
            // class node that lost the race to publish, which no other CPU ever reached.
            let c = unsafe { &*p };
            let i = cpu.wrapping_sub(c.first);
            if i < c.len {
                // SAFETY: `i < len`, and `slots` holds `len` initialised slots for as long as the chunk lives.
                return unsafe { &*c.slots.add(i) };
            }
            p = c.next;
        }
        &self.shared
    }

    pub(crate) fn covered(&self) -> usize {
        self.covered.load(Ordering::Acquire)
    }

    /// Makes CPUs [0, n) have slots. Allocates: only where ksync may allocate. A failed allocation leaves the
    /// rest on the shared slot (tried again next time).
    pub(crate) fn cover(&self, n: usize) {
        loop {
            let have = self.covered.load(Ordering::Acquire);
            if n <= have {
                return;
            }
            // At least doubling: few chunks to walk however the CPU count grows (host threads grow one by one).
            let end = n.max(have.saturating_mul(2));
            let len = end - have;
            let Ok(sl) = Layout::array::<Slot>(len) else { return };
            let hl = Layout::new::<Chunk>();
            // SAFETY: `len > 0`, so a non-zero-sized layout. A null result is handled (no `handle_alloc_error`,
            // which would panic). All-zero bytes are valid `AtomicU64`s, i.e. valid fresh slots.
            let slots = unsafe { alloc::alloc::alloc_zeroed(sl) } as *mut Slot;
            if slots.is_null() {
                return;
            }
            // SAFETY: a non-zero-sized layout; null is handled below.
            let ch = unsafe { alloc::alloc::alloc(hl) } as *mut Chunk;
            if ch.is_null() {
                // SAFETY: allocated just above with `sl`, never published.
                unsafe { alloc::alloc::dealloc(slots as *mut u8, sl) };
                return;
            }
            if self.covered.compare_exchange(have, end, Ordering::AcqRel, Ordering::Acquire).is_err() {
                // Another CPU covered first: give ours back and look again.
                // SAFETY: both allocated above with these layouts and never published.
                unsafe {
                    alloc::alloc::dealloc(ch as *mut u8, hl);
                    alloc::alloc::dealloc(slots as *mut u8, sl);
                }
                continue;
            }
            let mut head = self.chunks.load(Ordering::Acquire);
            loop {
                // SAFETY: `ch` is ours and unpublished; `Chunk` has no destructor, so overwriting is fine.
                unsafe { ch.write(Chunk { first: have, len, slots, next: head }) };
                match self.chunks.compare_exchange(head, ch, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => return,
                    Err(h) => head = h,
                }
            }
        }
    }

    /// Frees the chunks of a `Stats` nobody else ever saw (a class node that lost the race to publish).
    ///
    /// # Safety
    /// `self` was never published: no other CPU holds a reference to it or to its slots.
    pub(crate) unsafe fn free_unpublished(&self) {
        let mut p = self.chunks.swap(core::ptr::null_mut(), Ordering::Acquire);
        while !p.is_null() {
            // SAFETY: by the caller's promise the chunks are ours alone; each was allocated in `cover` with
            // these layouts (the slot layout was valid then, so it is now).
            unsafe {
                let c = p.read();
                let sl = Layout::array::<Slot>(c.len).unwrap_unchecked();
                alloc::alloc::dealloc(c.slots as *mut u8, sl);
                alloc::alloc::dealloc(p as *mut u8, Layout::new::<Chunk>());
                p = c.next;
            }
        }
        self.covered.store(0, Ordering::Release);
    }

    /// Every slot: `None` for the shared one, `Some(cpu)` for a CPU's own.
    pub(crate) fn for_each_slot(&self, mut f: impl FnMut(Option<usize>, &Slot)) {
        f(None, &self.shared);
        let mut p = self.chunks.load(Ordering::Acquire);
        while !p.is_null() {
            // SAFETY: as in `slot`.
            let c = unsafe { &*p };
            for i in 0..c.len {
                // SAFETY: as in `slot`.
                f(Some(c.first + i), unsafe { &*c.slots.add(i) });
            }
            p = c.next;
        }
    }

    pub(crate) fn sums(&self) -> Sums {
        let mut s = Sums::default();
        self.for_each_slot(|_, x| s.add(x));
        s
    }
}
