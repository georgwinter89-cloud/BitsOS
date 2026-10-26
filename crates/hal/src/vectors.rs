//! Interrupt vector allocation and handler registration (owner: CP; contract `C: IrqVector`). Nobody but CP
//! edits the IDT: drivers ask for a vector here and get their handler called.
//!
//! CP-3: a per-CPU vector allocator (Linux `arch/x86/kernel/apic/vector.c`, the "vector matrix" idea): every CPU
//! has its own 256-bit map of taken vectors and its own handler table, so the same vector number can serve
//! different devices on different CPUs. `alloc(handler, None)` picks the CPU with the most free vectors.
//! Vectors:
//! - 0x00-0x1F exceptions, 0x20-0x2F where the masked 8259s point (never handed out), both reserved;
//! - `FIRST_DYNAMIC..=LAST_DYNAMIC` (0x30-0xEF) for devices (MSI/MSI-X, the routed I/O APIC pins);
//! - `FIRST_SYSTEM..=LAST_SYSTEM` (0xF0-0xFE) for kernel-wide vectors with one handler on every CPU (LAPIC
//!   timer, IPIs), registered with `register_system`;
//! - 0xFF the LAPIC spurious vector.
//!
//! `dispatch` (hard-IRQ context, interrupts off) counts the delivery, calls the handler and then sends the EOI,
//! so a level-triggered source is acknowledged only after its handler had the chance to clear it. A vector
//! without a handler is counted in `spurious_count` and gets an EOI only if its ISR bit is set (a LAPIC
//! delivery); a vector that set none, e.g. an 8259 spurious IRQ 7/15 (0x27/0x2F) should LINT0 ever carry one,
//! must not retire some other in-service vector. The APIC spurious vector 0xFF is counted separately and gets no
//! EOI (SDM Vol. 3A 11.9: it sets no ISR bit).
//!
//! Handlers run with interrupts off on the interrupted thread's stack: no heap, no klog, no plain `SpinLock`
//! that thread code takes without masking interrupts; atomics and `IrqSpinLock` only. Work goes to a thread or
//! a poller. Since SC-3 handlers may wake threads (`WaitQueue::wake_*`, `Completion::complete`); since CP-5 the
//! interrupt exit may preempt the interrupted kernel thread (`set_irq_exit_hook`).
//!
//! Bit search uses `trailing_zeros`/`count_ones`, i.e. TZCNT/POPCNT of the x86-64-v3 baseline (R10).

use crate::idt::TrapFrame;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

pub type IrqHandler = fn(vector: u8);

pub const FIRST_DYNAMIC: u8 = 0x30;
pub const LAST_DYNAMIC: u8 = 0xEF;
/// Kernel-wide vectors, one handler for all CPUs (`register_system`).
pub const FIRST_SYSTEM: u8 = 0xF0;
pub const LAST_SYSTEM: u8 = 0xFE;
/// The LAPIC timer (CP-5); the LVT timer entry carries it while masked.
pub const LAPIC_TIMER: u8 = 0xF0;
/// CP-4's cross-CPU call and wake-up IPI (`smp::call_on`, `smp::wake`); its handler is hal's own.
pub const SMP_CALL: u8 = 0xF1;
/// The LAPIC spurious-interrupt vector (SVR); EDK2 leaves 0x0F there, an exception vector (spec/core.md §11.13).
pub const SPURIOUS_VECTOR: u8 = 0xFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorError {
    /// Every dynamic vector of the target CPU is taken (per CPU, so this is a property of x86, not a limit).
    Exhausted,
    /// `alloc(_, Some(cpu))` for a CPU that is not up.
    NoSuchCpu,
    /// `register_system` outside `FIRST_SYSTEM..=LAST_SYSTEM`, `reserve` outside the dynamic range.
    NotInRange,
    /// `register_system`/`reserve` on a vector that is already taken.
    Taken,
}

/// A vector on one CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vector {
    pub cpu: u32,
    pub vector: u8,
}

const WORDS: usize = 4;

/// Bits `lo..=hi` that fall into bitmap word `w`.
const fn range_mask(w: usize, lo: u8, hi: u8) -> u64 {
    let (start, end) = (w as u32 * 64, w as u32 * 64 + 63);
    let (lo, hi) = (lo as u32, hi as u32);
    if hi < start || lo > end {
        return 0;
    }
    let a = lo.saturating_sub(start);
    let b = if hi < end { hi - start } else { 63 };
    let upper = if b == 63 { u64::MAX } else { (1u64 << (b + 1)) - 1 };
    upper & !((1u64 << a) - 1)
}

/// Vectors no allocation may hand out: exceptions, the 8259 range, the system vectors and 0xFF.
const fn reserved_word(w: usize) -> u64 {
    range_mask(w, 0x00, FIRST_DYNAMIC - 1) | range_mask(w, FIRST_SYSTEM, 0xFF)
}

/// One CPU's vectors: which are taken, their handlers, and per-vector delivery counts. Taking and giving
/// back bits is crate-private: outside hal only `alloc`/`free`/`reserve` change ownership (one owner per vector).
pub struct CpuVectors {
    used: [AtomicU64; WORDS],
    handlers: [AtomicUsize; 256],
    counts: [AtomicU64; 256],
    unhandled: AtomicU64,
    apic_spurious: AtomicU64,
}

impl Default for CpuVectors {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuVectors {
    pub const fn new() -> Self {
        CpuVectors {
            used: [
                AtomicU64::new(reserved_word(0)),
                AtomicU64::new(reserved_word(1)),
                AtomicU64::new(reserved_word(2)),
                AtomicU64::new(reserved_word(3)),
            ],
            handlers: [const { AtomicUsize::new(0) }; 256],
            counts: [const { AtomicU64::new(0) }; 256],
            unhandled: AtomicU64::new(0),
            apic_spurious: AtomicU64::new(0),
        }
    }

    /// Takes the lowest free vector in `lo..=hi` (lock-free: one CAS per attempt on the word's bitmap).
    pub(crate) fn claim_lowest(&self, lo: u8, hi: u8) -> Option<u8> {
        for (w, word) in self.used.iter().enumerate() {
            let mask = range_mask(w, lo, hi);
            if mask == 0 {
                continue;
            }
            let mut cur = word.load(Ordering::Acquire);
            loop {
                let free = !cur & mask;
                if free == 0 {
                    break;
                }
                let bit = free.trailing_zeros();
                match word.compare_exchange_weak(cur, cur | 1 << bit, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => return Some((w as u32 * 64 + bit) as u8),
                    Err(now) => cur = now,
                }
            }
        }
        None
    }

    /// Takes exactly `v`; false if it was taken.
    pub(crate) fn claim(&self, v: u8) -> bool {
        let bit = 1u64 << (v % 64);
        self.used[v as usize / 64].fetch_or(bit, Ordering::AcqRel) & bit == 0
    }

    /// Gives `v` back; false if it was free.
    pub(crate) fn release(&self, v: u8) -> bool {
        let bit = 1u64 << (v % 64);
        self.used[v as usize / 64].fetch_and(!bit, Ordering::AcqRel) & bit != 0
    }

    #[cfg(test)]
    pub(crate) fn is_taken(&self, v: u8) -> bool {
        self.used[v as usize / 64].load(Ordering::Acquire) & (1u64 << (v % 64)) != 0
    }

    /// Taken vectors in the dynamic range (allocated + reserved by `reserve`).
    pub fn dynamic_taken(&self) -> u32 {
        self.used.iter().enumerate().map(|(w, u)| (u.load(Ordering::Acquire) & range_mask(w, FIRST_DYNAMIC, LAST_DYNAMIC)).count_ones()).sum()
    }

    fn handler(&self, v: u8) -> usize {
        self.handlers[v as usize].load(Ordering::Acquire)
    }
}

/// The BSP's table is static, so vectors exist before the heap; every further CPU gets a leaked one when it
/// is first asked for (CP-4), with no maximum.
static BSP_VECTORS: CpuVectors = CpuVectors::new();
/// Tables of CPUs 1.. as a leaked slice, replaced whole when it grows (old slices stay leaked: they change
/// only at CPU bring-up, so the waste is a few pointers per CPU).
static OTHERS: AtomicPtr<Box<[&'static CpuVectors]>> = AtomicPtr::new(core::ptr::null_mut());
static GROW_LOCK: AtomicBool = AtomicBool::new(false);
/// Deliveries on a CPU that has no table yet (only possible between an AP's `sti` and its first `alloc`).
static NO_TABLE: AtomicU64 = AtomicU64::new(0);

fn others() -> &'static [&'static CpuVectors] {
    let p = OTHERS.load(Ordering::Acquire);
    if p.is_null() {
        return &[];
    }
    // SAFETY: OTHERS only ever holds pointers from `Box::into_raw` of a fully built boxed slice in
    // `ensure_table`, published with Release; they are never freed.
    unsafe { &*p }
}

/// The table of `cpu`, if it exists. Lock-free (IRQ context).
pub fn table(cpu: usize) -> Option<&'static CpuVectors> {
    if cpu == 0 { Some(&BSP_VECTORS) } else { others().get(cpu - 1).copied() }
}

/// The table of `cpu`, created if missing (allocates; never from IRQ context).
fn ensure_table(cpu: usize) -> &'static CpuVectors {
    if let Some(t) = table(cpu) {
        return t;
    }
    // Interrupts off while waiting and holding: the holder only copies a few pointers and allocates, so the
    // wait is bounded (B4) and no preemption can park a holder. Never taken from hard-IRQ context (`alloc`
    // allocates).
    let _irq = crate::cpu::IrqGuard::new();
    while GROW_LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
    let old = others();
    let mut v: Vec<&'static CpuVectors> = old.to_vec();
    while v.len() < cpu {
        v.push(Box::leak(Box::new(CpuVectors::new())));
    }
    let t = v[cpu - 1];
    if v.len() > old.len() {
        OTHERS.store(Box::into_raw(Box::new(v.into_boxed_slice())), Ordering::Release);
    }
    GROW_LOCK.store(false, Ordering::Release);
    t
}

/// The CPU with the most free dynamic vectors among `n` CPUs (lowest index on a tie).
fn least_loaded(n: usize) -> usize {
    (0..n).min_by_key(|&c| table(c).map(|t| t.dynamic_taken()).unwrap_or(0)).unwrap_or(0)
}

/// Creates the vector table of `cpu` (CP-4, before that CPU starts: its first interrupt then finds a table).
pub fn prepare_cpu(cpu: usize) {
    let _ = ensure_table(cpu);
}

/// Allocates a vector on `cpu` (None = any, the allocator spreads) and registers `handler` for it.
pub fn alloc(handler: IrqHandler, cpu: Option<u32>) -> Result<Vector, VectorError> {
    let n = crate::percpu::cpu_count().max(1);
    let target = match cpu {
        Some(c) if (c as usize) < n => c as usize,
        Some(_) => return Err(VectorError::NoSuchCpu),
        None => least_loaded(n),
    };
    let t = ensure_table(target);
    let v = t.claim_lowest(FIRST_DYNAMIC, LAST_DYNAMIC).ok_or(VectorError::Exhausted)?;
    t.counts[v as usize].store(0, Ordering::Relaxed);
    t.handlers[v as usize].store(handler as usize, Ordering::Release);
    Ok(Vector { cpu: target as u32, vector: v })
}

/// Gives back a vector from `alloc`. The source must be masked first; an interrupt arriving later is counted
/// as unhandled. Ignored for vectors outside the dynamic range, vectors without a handler (free ones, and the
/// ones `reserve` keeps for the firmware: the LAPIC still delivers there) and a second `free`.
pub fn free(v: Vector) {
    if !(FIRST_DYNAMIC..=LAST_DYNAMIC).contains(&v.vector) {
        return;
    }
    if let Some(t) = table(v.cpu as usize)
        && t.handlers[v.vector as usize].swap(0, Ordering::AcqRel) != 0
    {
        t.release(v.vector);
    }
}

/// Marks a dynamic vector of `cpu` as taken without a handler, e.g. one the firmware left in an unmasked LVT
/// entry that CP leaves alone, so no device is ever given it. Deliveries count as unhandled (with EOI). A
/// reservation is for good: `free` does not release it.
pub fn reserve(cpu: u32, vector: u8) -> Result<(), VectorError> {
    if !(FIRST_DYNAMIC..=LAST_DYNAMIC).contains(&vector) {
        return Err(VectorError::NotInRange);
    }
    if cpu as usize >= crate::percpu::cpu_count().max(1) {
        return Err(VectorError::NoSuchCpu);
    }
    if ensure_table(cpu as usize).claim(vector) { Ok(()) } else { Err(VectorError::Taken) }
}

const SYSTEM_COUNT: usize = (LAST_SYSTEM - FIRST_SYSTEM + 1) as usize;
static SYSTEM: [AtomicUsize; SYSTEM_COUNT] = [const { AtomicUsize::new(0) }; SYSTEM_COUNT];

/// Registers the handler of a kernel-wide vector (0xF0-0xFE: LAPIC timer for CP-5, IPIs for SC/MM). The
/// same handler serves every CPU. One owner per vector.
pub fn register_system(vector: u8, handler: IrqHandler) -> Result<(), VectorError> {
    if !(FIRST_SYSTEM..=LAST_SYSTEM).contains(&vector) {
        return Err(VectorError::NotInRange);
    }
    SYSTEM[(vector - FIRST_SYSTEM) as usize]
        .compare_exchange(0, handler as usize, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| VectorError::Taken)
}

/// Removes a system handler (tests; a CPU-wide service that stops).
pub fn unregister_system(vector: u8) {
    if (FIRST_SYSTEM..=LAST_SYSTEM).contains(&vector) {
        SYSTEM[(vector - FIRST_SYSTEM) as usize].store(0, Ordering::Release);
    }
}

/// Whether `v` is currently allocated with a handler.
pub fn is_registered(v: Vector) -> bool {
    table(v.cpu as usize).is_some_and(|t| t.handler(v.vector) != 0)
}

/// Deliveries on `v` (counted from its last `alloc`; on 0xFF the APIC spurious ones).
pub fn count(v: Vector) -> u64 {
    table(v.cpu as usize).map(|t| t.counts[v.vector as usize].load(Ordering::Relaxed)).unwrap_or(0)
}

/// Per-CPU numbers for `hw_info` and the Mini-PC check.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VectorStats {
    pub cpu: u32,
    /// Taken dynamic vectors (allocated or reserved).
    pub dynamic_taken: u32,
    /// Deliveries on any vector >= 0x20 (APIC spurious included).
    pub delivered: u64,
    /// Deliveries on a vector without a handler (EOI sent).
    pub unhandled: u64,
    /// Deliveries on the LAPIC spurious vector 0xFF (no EOI).
    pub apic_spurious: u64,
}

pub fn stats(cpu: u32) -> Option<VectorStats> {
    let t = table(cpu as usize)?;
    Some(VectorStats {
        cpu,
        dynamic_taken: t.dynamic_taken(),
        delivered: t.counts.iter().map(|c| c.load(Ordering::Relaxed)).sum(),
        unhandled: t.unhandled.load(Ordering::Relaxed),
        apic_spurious: t.apic_spurious.load(Ordering::Relaxed),
    })
}

fn all_tables() -> impl Iterator<Item = &'static CpuVectors> {
    core::iter::once(&BSP_VECTORS).chain(others().iter().copied())
}

/// Number of interrupts that arrived on a vector without a handler (every CPU).
pub fn spurious_count() -> usize {
    (all_tables().map(|t| t.unhandled.load(Ordering::Relaxed)).sum::<u64>() + NO_TABLE.load(Ordering::Relaxed)) as usize
}

/// Number of LAPIC spurious interrupts (vector 0xFF, every CPU).
pub fn apic_spurious_count() -> u64 {
    all_tables().map(|t| t.apic_spurious.load(Ordering::Relaxed)).sum()
}

/// Called by the IDT stubs for vectors >= 32 (interrupts off).
pub(crate) fn dispatch(vector: u8, _frame: &mut TrapFrame) {
    let Some(t) = table(crate::percpu::this_cpu()) else {
        NO_TABLE.fetch_add(1, Ordering::Relaxed);
        if vector != SPURIOUS_VECTOR {
            eoi_if_in_service(vector);
        }
        return;
    };
    t.counts[vector as usize].fetch_add(1, Ordering::Relaxed);
    if vector == SPURIOUS_VECTOR {
        // No ISR bit was set, so no EOI (SDM Vol. 3A 11.9).
        t.apic_spurious.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let h = if vector >= FIRST_SYSTEM { SYSTEM[(vector - FIRST_SYSTEM) as usize].load(Ordering::Acquire) } else { t.handler(vector) };
    if h == 0 {
        t.unhandled.fetch_add(1, Ordering::Relaxed);
        eoi_if_in_service(vector);
        return;
    }
    // SAFETY: only `alloc` and `register_system` store non-zero values into the handler tables, always a
    // valid `IrqHandler` (fn pointer cast to usize).
    let f: IrqHandler = unsafe { core::mem::transmute::<usize, IrqHandler>(h) };
    crate::percpu::irq_enter();
    f(vector);
    crate::percpu::irq_exit();
    crate::apic::eoi();
}

/// The scheduler's interrupt-exit hook (`sched::irq_exit_preempt`), installed by CP-5's `platform` (0: none).
static IRQ_EXIT_HOOK: AtomicUsize = AtomicUsize::new(0);
/// Interrupts from kernel mode after which the hook switched to another thread.
static IRQ_EXIT_SWITCHES: AtomicU64 = AtomicU64::new(0);

/// Installs (or removes) the interrupt-exit hook (CP-5; SC-3's `sched::irq_exit_preempt`). It runs after every
/// hardware interrupt that arrived in **kernel mode** was handled and acknowledged (EOI sent, this CPU's IRQ
/// depth back to 0), with interrupts off, on the interrupted thread's stack; its argument is IF of the
/// interrupted RFLAGS. It may switch to another thread and returns when the interrupted one runs again
/// (possibly on another CPU), returning whether it switched. Interrupts from user mode do not call it: `entry`
/// goes back to the user loop, whose own hook (`entry::set_user_irq_hook`) is the preemption point there.
pub fn set_irq_exit_hook(f: Option<fn(bool) -> bool>) {
    IRQ_EXIT_HOOK.store(f.map_or(0, |f| f as usize), Ordering::Release);
}

/// Whether an interrupt-exit hook is installed.
pub fn irq_exit_hook_installed() -> bool {
    IRQ_EXIT_HOOK.load(Ordering::Acquire) != 0
}

/// How often the interrupt-exit hook switched threads (every CPU).
pub fn irq_exit_switches() -> u64 {
    IRQ_EXIT_SWITCHES.load(Ordering::Relaxed)
}

/// Called by `idt::hal_kernel_trap` after `dispatch` for an interrupt from kernel mode.
pub(crate) fn irq_exit_from_kernel(frame: &TrapFrame) {
    const RFLAGS_IF: u64 = 1 << 9;
    let h = IRQ_EXIT_HOOK.load(Ordering::Acquire);
    // Only at the outermost level: a nested handler's exit must not switch the stack an outer one still uses.
    if h == 0 || crate::percpu::in_hard_irq() || frame.cs & 3 != 0 {
        return;
    }
    // SAFETY: only `set_irq_exit_hook` stores non-zero values, always a valid `fn(bool) -> bool` cast to usize.
    let f: fn(bool) -> bool = unsafe { core::mem::transmute::<usize, fn(bool) -> bool>(h) };
    if f(frame.rflags & RFLAGS_IF != 0) {
        IRQ_EXIT_SWITCHES.fetch_add(1, Ordering::Relaxed);
    }
}

/// EOI only for a vector the LAPIC put in service: an EOI always retires the highest ISR bit, which for a
/// vector that set none (8259 spurious IRQ 7/15, a software `int`) would be some other interrupt.
fn eoi_if_in_service(vector: u8) {
    if crate::apic::isr_bit(vector) {
        crate::apic::eoi();
    }
}

struct Poller {
    name: &'static str,
    func: fn(),
    next: *mut Poller,
}

static POLLERS: AtomicPtr<Poller> = AtomicPtr::new(core::ptr::null_mut());

/// Registers a polling function (every driver's `poll()` path that panic/netpoll and early bring-up use, and
/// deferred work of hard-IRQ handlers). Pollers are never removed.
pub fn register_poller(name: &'static str, func: fn()) {
    let node = Box::into_raw(Box::new(Poller { name, func, next: core::ptr::null_mut() }));
    let mut head = POLLERS.load(Ordering::Acquire);
    loop {
        // SAFETY: node is ours until published by the successful CAS.
        unsafe { (*node).next = head };
        match POLLERS.compare_exchange(head, node, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(h) => head = h,
        }
    }
}

/// Calls every registered poller once.
pub fn poll_all() {
    let mut p = POLLERS.load(Ordering::Acquire);
    while !p.is_null() {
        // SAFETY: nodes are leaked boxes, never freed, linked once before publication.
        let n = unsafe { &*p };
        (n.func)();
        p = n.next;
    }
}

/// Names of the registered pollers (for `hw_info`).
pub fn pollers() -> impl Iterator<Item = &'static str> {
    let mut p = POLLERS.load(Ordering::Acquire);
    core::iter::from_fn(move || {
        if p.is_null() {
            return None;
        }
        // SAFETY: as in poll_all.
        let n: &'static Poller = unsafe { &*p };
        p = n.next;
        Some(n.name)
    })
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::sync::Arc;

    #[test]
    fn masks() {
        assert_eq!(range_mask(0, 0, 63), u64::MAX);
        assert_eq!(range_mask(0, 0x30, 0xEF), u64::MAX << 48);
        assert_eq!(range_mask(3, 0x30, 0xEF), (1u64 << 48) - 1);
        assert_eq!(range_mask(1, 0x30, 0xEF), u64::MAX);
        assert_eq!(range_mask(3, 0xFF, 0xFF), 1 << 63);
        assert_eq!(range_mask(2, 0x00, 0x2F), 0);
        assert_eq!(reserved_word(0), (1u64 << 48) - 1);
        assert_eq!(reserved_word(3), 0xFFFFu64 << 48);
        assert_eq!(reserved_word(1) | reserved_word(2), 0);
    }

    #[test]
    fn fresh_table_has_192_dynamic_vectors() {
        let t = CpuVectors::new();
        assert_eq!(t.dynamic_taken(), 0);
        for v in [0u8, 0x1F, 0x20, 0x2F, 0xF0, 0xFE, 0xFF] {
            assert!(t.is_taken(v), "{v:#x} reserved");
        }
        let mut got = Vec::new();
        while let Some(v) = t.claim_lowest(FIRST_DYNAMIC, LAST_DYNAMIC) {
            got.push(v);
        }
        assert_eq!(got.len(), 192);
        assert_eq!(got[0], 0x30);
        assert_eq!(*got.last().unwrap(), 0xEF);
        assert!(got.windows(2).all(|w| w[1] == w[0] + 1), "lowest first, no gaps, no doubles");
        assert_eq!(t.dynamic_taken(), 192);
        assert!(t.release(0x77));
        assert!(!t.release(0x77), "double release is reported");
        assert_eq!(t.claim_lowest(FIRST_DYNAMIC, LAST_DYNAMIC), Some(0x77));
    }

    #[test]
    fn claim_exact() {
        let t = CpuVectors::new();
        assert!(t.claim(0x40));
        assert!(!t.claim(0x40));
        assert_eq!(t.claim_lowest(0x40, 0x41), Some(0x41));
        assert_eq!(t.claim_lowest(0x40, 0x41), None);
        assert_eq!(t.dynamic_taken(), 2);
    }

    #[test]
    fn claims_race_without_doubles() {
        let t = Arc::new(CpuVectors::new());
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let t = t.clone();
                std::thread::spawn(move || {
                    let mut mine = Vec::new();
                    while let Some(v) = t.claim_lowest(FIRST_DYNAMIC, LAST_DYNAMIC) {
                        mine.push(v);
                    }
                    mine
                })
            })
            .collect();
        let mut all: Vec<u8> = hs.into_iter().flat_map(|h| h.join().unwrap()).collect();
        all.sort_unstable();
        assert_eq!(all.len(), 192);
        all.dedup();
        assert_eq!(all.len(), 192, "no vector handed out twice");
    }

    fn h(_: u8) {}

    #[test]
    fn api_on_the_host() {
        // PC-3 change request 5 (INT-A1): the hal::msi host tests allocate from the same global BSP table in
        // parallel threads; serialised with them so `a` cannot be taken again between `free` and the check.
        let _serial = crate::msi::tests::serial();
        // The host build has one CPU (percpu::cpu_count() == 1).
        let a = alloc(h, None).unwrap();
        let b = alloc(h, Some(0)).unwrap();
        assert_eq!((a.cpu, b.cpu), (0, 0));
        assert_ne!(a.vector, b.vector);
        assert!(is_registered(a));
        assert_eq!(alloc(h, Some(1)), Err(VectorError::NoSuchCpu));
        free(a);
        assert!(!is_registered(a));
        free(a);
        free(Vector { cpu: 0, vector: SPURIOUS_VECTOR });
        assert!(table(0).unwrap().is_taken(SPURIOUS_VECTOR), "0xFF cannot be freed");
        assert_eq!(register_system(0x80, h), Err(VectorError::NotInRange));
        assert_eq!(register_system(0xFD, h), Ok(()));
        assert_eq!(register_system(0xFD, h), Err(VectorError::Taken));
        unregister_system(0xFD);
        assert_eq!(reserve(0, 0x20), Err(VectorError::NotInRange));
        assert_eq!(reserve(3, 0x50), Err(VectorError::NoSuchCpu));
        free(b);
        assert_eq!(stats(0).unwrap().cpu, 0);
        // A reservation survives `free`: the firmware's LVT still delivers there.
        let r = (FIRST_DYNAMIC..=LAST_DYNAMIC).rev().find(|&v| !table(0).unwrap().is_taken(v)).unwrap();
        assert_eq!(reserve(0, r), Ok(()));
        assert_eq!(reserve(0, r), Err(VectorError::Taken));
        free(Vector { cpu: 0, vector: r });
        assert!(table(0).unwrap().is_taken(r), "free released a reserved vector");
        assert!(!is_registered(Vector { cpu: 0, vector: r }));
        assert!(stats(5).is_none());
    }
}
