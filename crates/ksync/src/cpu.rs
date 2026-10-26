//! Per-CPU bookkeeping of ksync (interrupt-mask depth, allocator locks held, re-entry flag, the boot thread's
//! held-lock list). It is reached without a lock and without allocating, because it runs inside every lock
//! operation, the allocator's own lock included.
//!
//! In the kernel: CPU 0's record is static; the records of later CPUs come from `prepare_cpus` (chunks that are
//! never freed, so a reference stays valid; no maximum). A CPU without a record (an AP started before
//! `prepare_cpus`) still works: its `IrqSpinLock` guards keep their own IF flag and the checker skips it.
//! On the host every thread is its own "CPU" (a thread-local record), so host tests really contend.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// True inside a hard-interrupt handler on this CPU (`hal::vectors::dispatch`); always false on the host.
pub(crate) fn in_hard_irq() -> bool {
    #[cfg(target_os = "none")]
    {
        hal::percpu::in_hard_irq()
    }
    #[cfg(not(target_os = "none"))]
    {
        false
    }
}

/// Whether the kernel heap can serve ksync's bookkeeping yet. The first locks of a boot (the heap's own seed
/// lock among them) are taken before the heap has memory; until the kernel calls `heap_ready`, ksync tracks
/// without allocating. On the host the heap always exists.
static HEAP_READY: AtomicBool = AtomicBool::new(cfg!(not(target_os = "none")));

/// Called by the kernel once the heap has its seed (desk, kernel/src/main.rs).
pub fn heap_ready() {
    HEAP_READY.store(true, Ordering::Release);
}

pub(crate) struct CpuSync {
    /// Inside ksync's own bookkeeping on this CPU: a lock taken meanwhile (by the allocator the bookkeeping
    /// calls, or by an NMI) is not tracked, so the bookkeeping never re-enters itself.
    pub busy: AtomicBool,
    /// `IrqSpinLock`s held on this CPU: the nesting depth (low bits) and whether interrupts were on before the
    /// outermost (`IRQ_WAS_ON`). One word, changed by compare-exchange, so an NMI that takes an `IrqSpinLock`
    /// between a release's reads and its write cannot make that release forget to turn interrupts back on.
    pub irq: AtomicU32,
    /// Locks of allocator classes (`order.rs`, `alloc`) held on this CPU: while non-zero ksync never allocates.
    pub alloc_held: AtomicU32,
    /// Spin locks (and `PreemptGuard`s) held on this CPU: while non-zero the running thread must not be
    /// preempted (SC-3's interrupt-exit hook asks `preempt_count`). Goes with the thread across a switch.
    pub preempt: AtomicU32,
    #[cfg(debug_assertions)]
    pub boot_held: crate::lockdep::imp::HeldCell,
    /// The held list of hard-interrupt context on this CPU (handlers do not nest), so locks an interrupt
    /// handler takes are not mixed into the interrupted thread's list.
    #[cfg(debug_assertions)]
    pub irq_held: crate::lockdep::imp::HeldCell,
}

impl CpuSync {
    pub const fn new() -> Self {
        CpuSync {
            busy: AtomicBool::new(false),
            irq: AtomicU32::new(0),
            alloc_held: AtomicU32::new(0),
            preempt: AtomicU32::new(0),
            #[cfg(debug_assertions)]
            boot_held: crate::lockdep::imp::HeldCell::new(),
            #[cfg(debug_assertions)]
            irq_held: crate::lockdep::imp::HeldCell::new(),
        }
    }
    /// Enters the bookkeeping section; false if this CPU is already inside it (then do nothing tracked).
    ///
    /// The caller has interrupts off (`IrqOff`) from before it looks up the record until after `leave`: then it
    /// cannot move to another CPU in between (once SC-3 preempts) and enters and leaves the same CPU's section.
    pub fn enter(&self) -> bool {
        !self.busy.swap(true, Ordering::Acquire)
    }
    pub fn leave(&self) {
        self.busy.store(false, Ordering::Release);
    }
    /// ksync may allocate (class nodes, edges, a longer held list) only when no allocator lock is held here
    /// and not in a hard-interrupt handler (no heap there: the interrupted code may be inside the allocator).
    pub fn may_alloc(&self) -> bool {
        HEAP_READY.load(Ordering::Acquire) && self.alloc_held.load(Ordering::Relaxed) == 0 && !in_hard_irq()
    }
}

#[cfg(target_os = "none")]
mod imp {
    use super::CpuSync;
    use alloc::boxed::Box;
    use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

    static BSP: CpuSync = CpuSync::new();

    /// CPUs [first, first + cpus.len()).
    struct Chunk {
        first: usize,
        cpus: Box<[CpuSync]>,
        next: AtomicPtr<Chunk>,
    }

    static CHUNKS: AtomicPtr<Chunk> = AtomicPtr::new(core::ptr::null_mut());
    /// CPUs that have a record (BSP included).
    static COVERED: AtomicUsize = AtomicUsize::new(1);

    pub fn with_cpu<R>(f: impl FnOnce(Option<&CpuSync>) -> R) -> R {
        let i = hal::percpu::this_cpu();
        if i == 0 {
            return f(Some(&BSP));
        }
        let mut p = CHUNKS.load(Ordering::Acquire);
        while !p.is_null() {
            // SAFETY: chunks are published with Release after being fully built and are never freed or changed.
            let c = unsafe { &*p };
            if i >= c.first && i < c.first + c.cpus.len() {
                return f(Some(&c.cpus[i - c.first]));
            }
            p = c.next.load(Ordering::Acquire);
        }
        f(None)
    }

    /// Makes sure CPUs 0..n have records. Allocates: call before those CPUs take any lock (CP-4, before the APs
    /// start), never from inside a lock.
    pub fn prepare_cpus(n: usize) {
        loop {
            let have = COVERED.load(Ordering::Acquire);
            if n <= have {
                return;
            }
            let cpus: Box<[CpuSync]> = (have..n).map(|_| CpuSync::new()).collect();
            let chunk = Box::into_raw(Box::new(Chunk { first: have, cpus, next: AtomicPtr::new(core::ptr::null_mut()) }));
            if COVERED.compare_exchange(have, n, Ordering::AcqRel, Ordering::Acquire).is_err() {
                // SAFETY: never published, still exclusively ours.
                drop(unsafe { Box::from_raw(chunk) });
                continue;
            }
            let mut head = CHUNKS.load(Ordering::Acquire);
            loop {
                // SAFETY: not yet published; we are the only writer of its `next`.
                unsafe { (*chunk).next.store(head, Ordering::Relaxed) };
                match CHUNKS.compare_exchange(head, chunk, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => return,
                    Err(h) => head = h,
                }
            }
        }
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    use super::CpuSync;
    std::thread_local! {
        static CPU: CpuSync = const { CpuSync::new() };
    }
    pub fn with_cpu<R>(f: impl FnOnce(Option<&CpuSync>) -> R) -> R {
        let mut f = Some(f);
        match CPU.try_with(|c| (f.take().expect("called once"))(Some(c))) {
            Ok(r) => r,
            // The thread is being torn down (a lock released by a thread-local destructor).
            Err(_) => (f.take().expect("not called yet"))(None),
        }
    }
    pub fn prepare_cpus(_n: usize) {}
}

pub(crate) use imp::with_cpu;
pub use imp::prepare_cpus;

/// Interrupts off for ksync's bookkeeping (restored on drop). Keeps the bookkeeping on one CPU: no interrupt,
/// so no preemption and no migration between finding this CPU's record and leaving its section.
pub(crate) struct IrqOff {
    _irq: hal::cpu::IrqGuard,
}

impl IrqOff {
    pub fn new() -> Self {
        IrqOff { _irq: hal::cpu::IrqGuard::new() }
    }
}

/// Atomically replaces `a` by `f(old)` and returns `old` (a compare-exchange loop).
pub(crate) fn update(a: &AtomicU32, f: impl Fn(u32) -> u32) -> u32 {
    let mut old = a.load(Ordering::Relaxed);
    loop {
        match a.compare_exchange_weak(old, f(old), Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return old,
            Err(now) => old = now,
        }
    }
}

/// `CpuSync::irq`: interrupts were on before the outermost `IrqSpinLock` of this CPU.
const IRQ_WAS_ON: u32 = 1 << 31;
const IRQ_DEPTH: u32 = IRQ_WAS_ON - 1;

/// How an `IrqSpinLock` guard gives the interrupt flag back.
#[derive(Clone, Copy)]
pub(crate) enum IrqSave {
    /// Counted in this CPU's depth: the outermost release restores IF, whatever the release order.
    Counted,
    /// No per-CPU record: the guard restores what it saw (LIFO only).
    Own(bool),
}

pub(crate) fn irq_save() -> IrqSave {
    let was = hal::cpu::interrupts_enabled();
    hal::cpu::disable_interrupts();
    with_cpu(|c| match c {
        Some(c) => {
            // Interrupts are off: only an NMI can change the word meanwhile, and it leaves it as it found it.
            update(&c.irq, |v| {
                let depth = v & IRQ_DEPTH;
                if depth == 0 { 1 | if was { IRQ_WAS_ON } else { 0 } } else { (v & IRQ_WAS_ON) | (depth + 1) }
            });
            IrqSave::Counted
        }
        None => IrqSave::Own(was),
    })
}

pub(crate) fn irq_restore(s: IrqSave) {
    let on = match s {
        IrqSave::Own(was) => was,
        IrqSave::Counted => with_cpu(|c| match c {
            Some(c) => {
                let old = update(&c.irq, |v| match v & IRQ_DEPTH {
                    // An unpaired release: leave the word alone.
                    0 => v,
                    1 => 0,
                    d => (v & IRQ_WAS_ON) | (d - 1),
                });
                old & IRQ_DEPTH == 1 && old & IRQ_WAS_ON != 0
            }
            None => false,
        }),
    };
    if on {
        hal::cpu::enable_interrupts();
    }
}

/// The running thread's `IrqSpinLock` state on this CPU, taken off the CPU for a thread switch (`sched`):
/// interrupts are per thread (hal saves RFLAGS), so the depth must go with the thread, not stay with the CPU
/// for the next one. Call with interrupts off, right before the switch; `put_irq_state` right after it, on
/// the CPU the thread then runs on. Normally 0 (a switch with an `IrqSpinLock` held is a bug the checker
/// reports), but then the next thread starts clean instead of running with interrupts off for good.
///
/// The preempt count (spin locks held) goes the same way: normally 0 at a switch too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrqState {
    irq: u32,
    preempt: u32,
}

pub fn take_irq_state() -> IrqState {
    with_cpu(|c| match c {
        Some(c) => IrqState { irq: c.irq.swap(0, Ordering::Relaxed), preempt: c.preempt.swap(0, Ordering::Relaxed) },
        None => IrqState { irq: 0, preempt: 0 },
    })
}

pub fn put_irq_state(s: IrqState) {
    with_cpu(|c| {
        if let Some(c) = c {
            c.irq.store(s.irq, Ordering::Relaxed);
            c.preempt.store(s.preempt, Ordering::Relaxed);
        }
    });
}

/// Counts one more preemption blocker on this CPU (a spin lock about to be taken). Interrupts are off for the
/// update, so the count cannot land on another CPU's record halfway.
pub(crate) fn preempt_inc() {
    let _irq = IrqOff::new();
    with_cpu(|c| {
        if let Some(c) = c {
            c.preempt.fetch_add(1, Ordering::Relaxed);
        }
    });
}

/// Undoes `preempt_inc` (after the lock is released); never below 0.
pub(crate) fn preempt_dec() {
    let _irq = IrqOff::new();
    with_cpu(|c| {
        if let Some(c) = c {
            update(&c.preempt, |n| n.saturating_sub(1));
        }
    });
}

/// Spin locks and `PreemptGuard`s held on this CPU (0: the running thread may be preempted, as far as ksync
/// knows; SC-3's interrupt-exit hook and `cond_resched` ask this).
pub fn preempt_count() -> u32 {
    let _irq = IrqOff::new();
    with_cpu(|c| c.map(|c| c.preempt.load(Ordering::Relaxed)).unwrap_or(0))
}

/// Keeps the running thread on this CPU and unpreempted while it lives (code that works on per-CPU data with
/// interrupts on). It does not stop voluntary switches: do not sleep or yield while holding one.
pub struct PreemptGuard {
    _not_send: crate::spin::NotSend,
}

impl PreemptGuard {
    pub fn new() -> Self {
        preempt_inc();
        PreemptGuard { _not_send: core::marker::PhantomData }
    }
}

impl Drop for PreemptGuard {
    fn drop(&mut self) {
        preempt_dec();
    }
}

/// Depth of `IrqSpinLock`s held on this CPU (tests, the `locks` tool).
pub fn irq_lock_depth() -> u32 {
    with_cpu(|c| c.map(|c| c.irq.load(Ordering::Relaxed) & IRQ_DEPTH).unwrap_or(0))
}
