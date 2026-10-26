//! The lock-order checker (debug builds; Linux `kernel/locking/lockdep.c` in small). Each thread keeps the list
//! of locks it holds (in the kernel: an extension of its hal context, so sleeping `Mutex` holders keep theirs;
//! before the scheduler and on the host: the per-CPU/per-thread record). Before a lock is taken it is checked
//! against every held lock:
//!
//! - `Order`: the order table (`order.rs`) puts the new lock outside a held one;
//! - `Inversion`: two classes the table leaves unordered were taken both ways round (A then B somewhere, B
//!   then A elsewhere): the classic ABBA deadlock, found even if it never happened;
//! - `SameClass`: a second lock of a class already held (two instances nested: the order between them is
//!   undefined);
//! - `Recursive`: the very lock is already held by this thread (a certain deadlock for every kind here);
//! - `MutexUnderSpin`: a sleeping `Mutex` taken while a spin lock is held;
//! - `SleepUnderSpin`: a wait (`WaitQueue`, `sleep`) while a spin lock is held;
//! - `IrqUnsafe` (SC-3): a lock other than an `IrqSpinLock` taken in a hard-interrupt handler (a thread holding
//!   it with interrupts on would deadlock against the handler on its CPU).
//!
//! Hard-interrupt handlers keep their own per-CPU held list (handlers do not nest), so the locks a handler takes
//! are checked against each other, not against the locks of the thread it interrupted.
//!
//! A violation is counted and kept per (kind, held class, new class); it never panics. The scheduler turns new
//! ones into `klog` events (`report`), ktests check `counts().unexpected`; `expect_violations` marks the ones a
//! test injects on purpose. `try_lock` is not checked (it cannot deadlock) but the lock counts as held.
//! Release builds keep only the API (`enabled()` is false, counts stay 0).

pub use api::*;

/// How a lock is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockKind {
    Spin,
    IrqSpin,
    Read,
    Write,
    Mutex,
}

impl LockKind {
    pub fn spins(self) -> bool {
        !matches!(self, LockKind::Mutex)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViolationKind {
    Order,
    Inversion,
    SameClass,
    Recursive,
    MutexUnderSpin,
    SleepUnderSpin,
    IrqUnsafe,
}

impl ViolationKind {
    pub fn name(self) -> &'static str {
        match self {
            ViolationKind::Order => "order",
            ViolationKind::Inversion => "inversion",
            ViolationKind::SameClass => "same_class",
            ViolationKind::Recursive => "recursive",
            ViolationKind::MutexUnderSpin => "mutex_under_spin",
            ViolationKind::SleepUnderSpin => "sleep_under_spin",
            ViolationKind::IrqUnsafe => "irq_unsafe",
        }
    }
}

/// One kind of violation as reported.
#[derive(Clone, Debug)]
pub struct Violation {
    pub kind: ViolationKind,
    /// The class held (for `SleepUnderSpin`: the spin lock held while waiting).
    pub held: &'static str,
    /// The class being taken (`(sleep)` for `SleepUnderSpin`).
    pub taking: &'static str,
    pub held_level: u16,
    pub taking_level: u16,
    pub count: u64,
    /// How many of them were injected by a test (`expect_violations`).
    pub injected: u64,
    /// Thread id that hit it first.
    pub first_thread: u64,
    /// New since the last `report` (only in `report`'s callback).
    pub new: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub total: u64,
    /// Not injected by a test: each is a bug (a ktest failure).
    pub unexpected: u64,
    pub injected: u64,
    /// Violations that could not get their own record (no allocation possible at that moment).
    pub unrecorded: u64,
    /// Acquisitions that went untracked because the held list could not grow (allocator lock held).
    pub untracked: u64,
}

#[cfg(debug_assertions)]
mod api {
    pub use super::imp::{counts, enabled, expect_violations, held_count, list, might_sleep, new_thread_state, prepare_irq_context, report};
}

#[cfg(not(debug_assertions))]
mod api {
    use super::{Counts, Violation};
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    pub fn enabled() -> bool {
        false
    }
    pub fn counts() -> Counts {
        Counts::default()
    }
    pub fn list() -> Vec<Violation> {
        Vec::new()
    }
    pub fn report(_f: impl FnMut(&Violation)) {}
    pub fn expect_violations<R>(f: impl FnOnce() -> R) -> (R, u64) {
        (f(), 0)
    }
    pub fn held_count() -> usize {
        0
    }
    pub fn new_thread_state() -> Option<Box<dyn core::any::Any + Send + Sync>> {
        None
    }
    pub fn might_sleep() {}
    pub fn prepare_irq_context() {}
}

#[cfg(debug_assertions)]
pub(crate) mod imp {
    use super::{Counts, LockKind, Violation, ViolationKind};
    use crate::class::LockClass;
    use crate::cpu::{CpuSync, with_cpu};
    use crate::order;
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

    #[derive(Clone, Copy)]
    struct HeldEntry {
        addr: usize,
        class: &'static LockClass,
        kind: LockKind,
    }

    pub(crate) struct HeldStack {
        v: Vec<HeldEntry>,
        /// > 0 inside `expect_violations` on this thread.
        expecting: u32,
        /// Violations recorded on this thread while expecting.
        injected: u64,
    }

    /// A held list, used only by the thread (or CPU) it belongs to and only inside ksync's bookkeeping section
    /// of the CPU it runs on (`CpuSync::enter`), which is entered with interrupts off and left on the same CPU
    /// before they come back on. So there is never more than one `&mut` to it: the thread cannot be switched
    /// out or moved while it holds one, and anything nested on that CPU (an NMI) finds the section taken.
    pub(crate) struct HeldCell(UnsafeCell<HeldStack>);

    // SAFETY: see the type's comment: all access goes through `with_held`, which hands out the `&mut` only
    // inside the per-CPU bookkeeping section (interrupts off, so the thread is not switched or migrated while
    // it is inside); a thread's list is used by that thread (and by handlers nested on its CPU, which find the
    // section taken and skip), never by two CPUs at once.
    unsafe impl Sync for HeldCell {}

    impl HeldCell {
        pub const fn new() -> Self {
            HeldCell(UnsafeCell::new(HeldStack { v: Vec::new(), expecting: 0, injected: 0 }))
        }
    }

    /// Per-thread state for a new kernel thread (`KernelContext::with_ext`); sched attaches it at spawn.
    pub fn new_thread_state() -> Option<Box<dyn core::any::Any + Send + Sync>> {
        prepare();
        let c = HeldCell::new();
        // SAFETY: not shared yet.
        let _ = unsafe { &mut *c.0.get() }.v.try_reserve(16);
        Some(Box::new(c))
    }

    /// Runs `f` on the running thread's held list (in a hard-interrupt handler: this CPU's interrupt list).
    /// Must be called inside `cpu`'s bookkeeping section.
    fn with_held<R>(cpu: &CpuSync, f: impl FnOnce(&mut HeldStack) -> R) -> R {
        #[cfg(target_os = "none")]
        {
            if crate::cpu::in_hard_irq() {
                // SAFETY: this CPU's interrupt list, inside its bookkeeping section; handlers do not nest, and an
                // NMI finds the section taken (HeldCell's rule).
                return f(unsafe { &mut *cpu.irq_held.0.get() });
            }
            let mut f = Some(f);
            let r = hal::switch::with_current_ext(|ext| {
                let cell = ext.and_then(|e| e.downcast_ref::<HeldCell>())?;
                let f = f.take()?;
                // SAFETY: the running thread's list, inside this CPU's bookkeeping section (HeldCell's rule).
                Some(f(unsafe { &mut *cell.0.get() }))
            });
            if let Some(r) = r {
                return r;
            }
            if let Some(f) = f.take() {
                // SAFETY: the CPU's own list (no thread context yet), inside its bookkeeping section.
                return f(unsafe { &mut *cpu.boot_held.0.get() });
            }
            unreachable!("f is taken only when it ran")
        }
        #[cfg(not(target_os = "none"))]
        {
            // SAFETY: the host thread's own record, inside its bookkeeping section.
            f(unsafe { &mut *cpu.boot_held.0.get() })
        }
    }

    /// Runs `f` inside this CPU's bookkeeping section, or not at all (None) if it is taken or missing.
    /// Interrupts are off throughout (`IrqOff`), so the thread stays on this CPU from the record lookup to
    /// `leave` (no interrupt, no preemption, no migration) and no interrupt handler can interleave on it.
    fn section<R>(f: impl FnOnce(&CpuSync) -> R) -> Option<R> {
        let _irq = crate::cpu::IrqOff::new();
        with_cpu(|cpu| {
            let cpu = cpu?;
            if !cpu.enter() {
                return None;
            }
            let r = f(cpu);
            cpu.leave();
            Some(r)
        })
    }

    pub fn enabled() -> bool {
        true
    }

    // ---- edges between unordered classes

    pub(crate) struct Edge {
        to: &'static LockClass,
        next: AtomicPtr<Edge>,
    }

    fn has_edge(from: &LockClass, to: &LockClass) -> bool {
        let mut p = from.edges.load(Ordering::Acquire);
        while !p.is_null() {
            // SAFETY: edges are published fully built and never freed.
            let e = unsafe { &*p };
            if core::ptr::eq(e.to, to) {
                return true;
            }
            p = e.next.load(Ordering::Acquire);
        }
        false
    }

    fn add_edge(cpu: &CpuSync, from: &'static LockClass, to: &'static LockClass) {
        if has_edge(from, to) || !cpu.may_alloc() {
            return;
        }
        let Some(node) = try_box(Edge { to, next: AtomicPtr::new(core::ptr::null_mut()) }) else { return };
        let mut head = from.edges.load(Ordering::Acquire);
        loop {
            // SAFETY: unpublished, ours alone.
            unsafe { (*node).next.store(head, Ordering::Relaxed) };
            match from.edges.compare_exchange(head, node, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(h) => head = h,
            }
        }
        // A duplicate edge from a race is harmless (has_edge only asks "any").
    }

    /// A leaked heap node, or None if the heap cannot serve now (no panic inside a lock operation).
    fn try_box<T>(v: T) -> Option<*mut T> {
        let layout = core::alloc::Layout::new::<T>();
        // SAFETY: a non-zero-sized layout (Edge and VNode are never empty); null is checked.
        let p = unsafe { alloc::alloc::alloc(layout) } as *mut T;
        if p.is_null() {
            return None;
        }
        // SAFETY: fresh allocation of T's layout.
        unsafe { p.write(v) };
        Some(p)
    }

    // ---- violation records

    struct VNode {
        kind: ViolationKind,
        held: &'static LockClass,
        taking: &'static LockClass,
        first_thread: u64,
        count: AtomicU64,
        injected: AtomicU64,
        reported: AtomicU64,
        next: AtomicPtr<VNode>,
    }

    static VIOLATIONS: AtomicPtr<VNode> = AtomicPtr::new(core::ptr::null_mut());
    static TOTAL: AtomicU64 = AtomicU64::new(0);
    static INJECTED: AtomicU64 = AtomicU64::new(0);
    static UNRECORDED: AtomicU64 = AtomicU64::new(0);
    static UNTRACKED: AtomicU64 = AtomicU64::new(0);

    /// Pseudo class for `SleepUnderSpin`.
    static SLEEP: LockClass = LockClass::fixed("(sleep)", order::SUBSYSTEM, false);

    fn for_each_node(mut f: impl FnMut(&'static VNode)) {
        let mut p = VIOLATIONS.load(Ordering::Acquire);
        while !p.is_null() {
            // SAFETY: nodes are published fully built and never freed.
            let n = unsafe { &*p };
            f(n);
            p = n.next.load(Ordering::Acquire);
        }
    }

    fn record(cpu: &CpuSync, hs: &mut HeldStack, kind: ViolationKind, held: &'static LockClass, taking: &'static LockClass) {
        let injected = hs.expecting > 0;
        TOTAL.fetch_add(1, Ordering::Relaxed);
        if injected {
            INJECTED.fetch_add(1, Ordering::Relaxed);
            hs.injected += 1;
        }
        let mut found = None;
        for_each_node(|n| {
            if found.is_none() && n.kind == kind && core::ptr::eq(n.held, held) && core::ptr::eq(n.taking, taking) {
                found = Some(n);
            }
        });
        let node = match found {
            Some(n) => n,
            None => {
                // Under an allocator lock (heap/frames) a record comes from the spares made earlier.
                let new = if cpu.may_alloc() { try_box(VNode::empty()) } else { take_spare() };
                let Some(p) = new else {
                    UNRECORDED.fetch_add(1, Ordering::Relaxed);
                    return;
                };
                // SAFETY: `p` is ours alone (fresh or popped off the spare list, never published).
                unsafe {
                    (*p).kind = kind;
                    (*p).held = held;
                    (*p).taking = taking;
                    (*p).first_thread = crate::thread_id();
                }
                let mut head = VIOLATIONS.load(Ordering::Acquire);
                loop {
                    // SAFETY: unpublished, ours alone.
                    unsafe { (*p).next.store(head, Ordering::Relaxed) };
                    match VIOLATIONS.compare_exchange(head, p, Ordering::AcqRel, Ordering::Acquire) {
                        Ok(_) => break,
                        Err(h) => head = h,
                    }
                }
                // SAFETY: published now, never freed.
                unsafe { &*p }
            }
        };
        node.count.fetch_add(1, Ordering::Relaxed);
        if injected {
            node.injected.fetch_add(1, Ordering::Relaxed);
        }
        top_up_spares(cpu);
    }

    impl VNode {
        fn empty() -> VNode {
            VNode {
                kind: ViolationKind::Order,
                held: &SLEEP,
                taking: &SLEEP,
                first_thread: 0,
                count: AtomicU64::new(0),
                injected: AtomicU64::new(0),
                reported: AtomicU64::new(0),
                next: AtomicPtr::new(core::ptr::null_mut()),
            }
        }
    }

    /// Spare records for violations found while an allocator lock is held (no allocation possible then). A
    /// lock-free stack: nodes are only ever pushed fresh and popped once (never pushed back), so there is no
    /// ABA. Topped up wherever allocating is allowed.
    static SPARES: AtomicPtr<VNode> = AtomicPtr::new(core::ptr::null_mut());
    static SPARE_COUNT: AtomicU64 = AtomicU64::new(0);
    const SPARE_TARGET: u64 = 4;

    fn take_spare() -> Option<*mut VNode> {
        let mut head = SPARES.load(Ordering::Acquire);
        while !head.is_null() {
            // SAFETY: spare nodes are leaked allocations, never freed; reading `next` of one that another CPU
            // popped meanwhile is still a valid read, and the CAS below then fails.
            let next = unsafe { (*head).next.load(Ordering::Acquire) };
            match SPARES.compare_exchange(head, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    SPARE_COUNT.fetch_sub(1, Ordering::Relaxed);
                    // SAFETY: popped: ours alone now.
                    unsafe { (*head).next.store(core::ptr::null_mut(), Ordering::Relaxed) };
                    return Some(head);
                }
                Err(h) => head = h,
            }
        }
        None
    }

    fn top_up_spares(cpu: &CpuSync) {
        while cpu.may_alloc() && SPARE_COUNT.load(Ordering::Relaxed) < SPARE_TARGET {
            let Some(p) = try_box(VNode::empty()) else { return };
            SPARE_COUNT.fetch_add(1, Ordering::Relaxed);
            let mut head = SPARES.load(Ordering::Acquire);
            loop {
                // SAFETY: fresh, unpublished.
                unsafe { (*p).next.store(head, Ordering::Relaxed) };
                match SPARES.compare_exchange(head, p, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => break,
                    Err(h) => head = h,
                }
            }
        }
    }

    /// Makes the spare violation records (sched::init and new threads; also after every record).
    pub(crate) fn prepare() {
        let _ = section(top_up_spares);
    }

    /// Room in this CPU's interrupt held list (it never grows inside a handler). Thread context, once per CPU.
    pub fn prepare_irq_context() {
        let _ = section(|cpu| {
            // SAFETY: this CPU's interrupt list, inside its bookkeeping section, from thread context: no handler
            // runs on this CPU meanwhile (interrupts are off in the section).
            let hs = unsafe { &mut *cpu.irq_held.0.get() };
            if hs.v.capacity() < 16 && cpu.may_alloc() {
                let _ = hs.v.try_reserve(16 - hs.v.len());
            }
            top_up_spares(cpu);
        });
    }

    // ---- the checks

    /// Before taking a lock of `class` at `addr` (inside the bookkeeping section).
    pub(crate) fn check(cpu: &CpuSync, class: &'static LockClass, addr: usize, kind: LockKind) {
        if SPARE_COUNT.load(Ordering::Relaxed) < SPARE_TARGET {
            top_up_spares(cpu);
        }
        with_held(cpu, |hs| {
            if crate::cpu::in_hard_irq() && kind != LockKind::IrqSpin {
                record(cpu, hs, ViolationKind::IrqUnsafe, class, class);
            }
            // Room for the entry `push` adds after the lock is taken (when it may be an allocator lock, which
            // forbids growing then).
            if hs.v.len() == hs.v.capacity() && cpu.may_alloc() {
                let _ = hs.v.try_reserve(hs.v.capacity().max(8));
            }
            let n = hs.v.len();
            for i in 0..n {
                let h = hs.v[i];
                if h.addr == addr {
                    record(cpu, hs, ViolationKind::Recursive, h.class, class);
                    continue;
                }
                if kind == LockKind::Mutex && h.kind.spins() {
                    record(cpu, hs, ViolationKind::MutexUnderSpin, h.class, class);
                }
                if h.class.unchecked || class.unchecked {
                    continue;
                }
                if core::ptr::eq(h.class, class) {
                    record(cpu, hs, ViolationKind::SameClass, h.class, class);
                } else if class.level < h.class.level {
                    record(cpu, hs, ViolationKind::Order, h.class, class);
                } else if class.level == h.class.level {
                    // Unordered by the table: learn the order and catch the reverse.
                    if has_edge(class, h.class) {
                        record(cpu, hs, ViolationKind::Inversion, h.class, class);
                    }
                    add_edge(cpu, h.class, class);
                }
            }
        });
    }

    /// After taking it: put it on the held list. False if it could not be tracked.
    pub(crate) fn push(cpu: &CpuSync, class: &'static LockClass, addr: usize, kind: LockKind) -> bool {
        with_held(cpu, |hs| {
            if hs.v.len() == hs.v.capacity() {
                // Growing allocates: only when no allocator lock is held here (the one just taken included).
                if !cpu.may_alloc() || hs.v.try_reserve(hs.v.capacity().max(8)).is_err() {
                    UNTRACKED.fetch_add(1, Ordering::Relaxed);
                    return false;
                }
            }
            hs.v.push(HeldEntry { addr, class, kind });
            true
        })
    }

    /// After releasing it (inside the bookkeeping section). Release order may differ from acquisition order.
    pub(crate) fn pop(cpu: &CpuSync, addr: usize) {
        with_held(cpu, |hs| {
            if let Some(i) = hs.v.iter().rposition(|h| h.addr == addr) {
                hs.v.remove(i);
            }
        });
    }

    pub(crate) fn tracked_push(class: &'static LockClass, addr: usize, kind: LockKind) -> bool {
        section(|cpu| push(cpu, class, addr, kind)).unwrap_or(false)
    }

    pub(crate) fn tracked_pop(addr: usize) {
        let _ = section(|cpu| pop(cpu, addr));
    }

    /// The calling thread is about to wait: a spin lock held now is a bug (R5).
    pub fn might_sleep() {
        let _ = section(|cpu| {
            with_held(cpu, |hs| {
                let n = hs.v.len();
                for i in 0..n {
                    let h = hs.v[i];
                    if h.kind.spins() {
                        record(cpu, hs, ViolationKind::SleepUnderSpin, h.class, &SLEEP);
                    }
                }
            })
        });
    }

    /// Runs `f` with violations on this thread counted as injected (tests). Returns how many were recorded.
    pub fn expect_violations<R>(f: impl FnOnce() -> R) -> (R, u64) {
        let before = section(|cpu| {
            with_held(cpu, |hs| {
                hs.expecting += 1;
                hs.injected
            })
        });
        let r = f();
        let after = section(|cpu| {
            with_held(cpu, |hs| {
                hs.expecting = hs.expecting.saturating_sub(1);
                hs.injected
            })
        });
        let n = match (before, after) {
            (Some(b), Some(a)) => a.saturating_sub(b),
            _ => 0,
        };
        (r, n)
    }

    /// Locks the calling thread holds (tracked ones).
    pub fn held_count() -> usize {
        section(|cpu| with_held(cpu, |hs| hs.v.len())).unwrap_or(0)
    }

    pub fn counts() -> Counts {
        let total = TOTAL.load(Ordering::Relaxed);
        let injected = INJECTED.load(Ordering::Relaxed);
        Counts {
            total,
            unexpected: total.saturating_sub(injected),
            injected,
            unrecorded: UNRECORDED.load(Ordering::Relaxed),
            untracked: UNTRACKED.load(Ordering::Relaxed),
        }
    }

    fn info(n: &VNode, new: u64) -> Violation {
        Violation {
            kind: n.kind,
            held: n.held.name,
            taking: n.taking.name,
            held_level: n.held.level,
            taking_level: n.taking.level,
            count: n.count.load(Ordering::Relaxed),
            injected: n.injected.load(Ordering::Relaxed),
            first_thread: n.first_thread,
            new,
        }
    }

    /// Every violation record (allocates: call where that is fine).
    pub fn list() -> Vec<Violation> {
        let mut v = Vec::new();
        for_each_node(|n| v.push(info(n, 0)));
        v
    }

    /// Calls `f` for each record with violations not reported before (the scheduler emits the events). Call
    /// from a normal thread with no lock held.
    pub fn report(mut f: impl FnMut(&Violation)) {
        for_each_node(|n| {
            let c = n.count.load(Ordering::Relaxed);
            let r = n.reported.swap(c, Ordering::AcqRel);
            if c > r {
                f(&info(n, c - r));
            }
        });
    }
}
