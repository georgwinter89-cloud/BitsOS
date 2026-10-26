//! Kernel threads at the bottom (owner: SC, item SC-1; spec/core.md §9.2, Linux `__switch_to_asm`,
//! `finish_task_switch`): the context switch, kernel stacks with an unmapped guard page, and the per-CPU record
//! of what runs where.
//!
//! - A thread's context (`KernelContext`) owns its kernel stack, its XSAVE area (`xsave::FpuState`) and its
//!   CR3. `switch_to` saves the running thread's user FPU/SIMD state, CR3 and callee-saved registers and
//!   resumes the target; the address space, the FPU state and the user GS base (which `swapgs` leaves in
//!   IA32_KERNEL_GS_BASE while the thread is in the kernel) are part of every switch, so two user threads
//!   alternate, and a thread moves between CPUs, without help from the process code. (The user FS base is
//!   `UserContext::fs_base`, written by `execute` on every entry to user mode.)
//! - The running context of each CPU and the one just left are held here as `Arc`s: a context cannot be freed
//!   while it runs, and a dying thread's stack is retired by `finish` (on the next thread, right after the
//!   switch) and unmapped and freed by `reap_stacks` once every CPU passed a quiescent point (the SC-3
//!   paragraph below), never while it is still in use.
//! - Stacks (`KernelStack`) live in vmap space with an unmapped guard page below. An overflow faults on the
//!   guard; `current_stack`/`guard_hit` let the scheduler's #PF/#DF hooks name the thread.
//!
//! Interrupt rule: `switch_to` runs with interrupts off, and an interrupt handler never switches; the one place
//! that may is the scheduler's interrupt-exit hook (`sched::irq_exit_preempt`), called after the handler and the
//! EOI on the interrupted thread's stack. Only thread stacks switch: `switch_to` refuses (`WrongStack`) when it
//! runs on one of this CPU's IST stacks (#DF, NMI, #MC hooks) or outside the running context's known stack. Each thread keeps its own IF across a switch (RFLAGS is part of the
//! context), and threads move between CPUs (SC-3): hal's per-CPU records are always this CPU's.
//! IF=0 does not keep out NMIs and faults, so the per-CPU record is guarded by its own `busy` flag: a hook that
//! re-enters hal during a switch gets `SwitchError::Reentered` and changes nothing.
//!
//! SC-2: a context may carry one extension (`with_ext`), per-thread data of a crate below the scheduler that
//! must find "the running thread" without a lock (ksync's held-lock list for the lock-order checker).
//! `with_current_ext` hands it out lock-free; it lives as long as the context.
//!
//! SC-3 (SMP): threads move between CPUs, and a freed kernel stack is unmapped with a local `invlpg` only (MM-6
//! brings the general TLB shootdown). So a stack is never freed right away: `finish` (and a context's drop)
//! **retires** it onto a lock-free list; `reap_stacks` unmaps retired stacks and frees them only once every CPU
//! passed a **quiescent point** after the unmap (`request_sync` / `sync_passed`): each CPU flushes its whole TLB
//! (global entries included) there when it sees a newer generation. Quiescent points are every switch, the
//! scheduler's idle loop and its reschedule IPI (`quiescent_point`). Interrupt handlers never nest, so a CPU at a
//! quiescent point also finished every interrupt handler it ran before: the scheduler uses the same generations
//! as a grace period for thread records an interrupt-time wake-up may still hold.

use crate::idt::TrapFrame;
use crate::paging::{self, PageFlags};
use crate::percpu::PerCpu;
use crate::xsave::FpuState;
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

/// What a scheduler hands to `switch_to`: anything that holds a `KernelContext` (sched's `Thread`).
pub trait Switchable: Send + Sync {
    fn context(&self) -> &KernelContext;
    /// The owner as `Any`, so a scheduler gets its own type back from `current_owner`.
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>;
}

/// Where a kernel stack lives: [guard, bottom) is unmapped, [bottom, top) is the stack.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StackBounds {
    pub guard: u64,
    pub bottom: u64,
    pub top: u64,
}

impl StackBounds {
    /// True if `addr` lies in the guard range (all zero bounds = unknown, never a hit).
    pub fn in_guard(&self, addr: u64) -> bool {
        self.guard != 0 && addr >= self.guard && addr < self.bottom
    }
    pub fn contains(&self, addr: u64) -> bool {
        addr >= self.bottom && addr < self.top
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackError {
    NoMemory,
    Map(paging::MapError),
    /// Part of the range is already mapped (somebody else's memory, or a guard that would not guard).
    InUse,
    /// The range is not inside the kernel's vmap area, or empty.
    BadRange,
    /// Mapping failed part-way (`map`: `None` = out of frames, else the paging error). The pages mapped so far
    /// were present, so other CPUs may have cached them: they are retired like a dropped stack, and hal gives the
    /// range back through `release` once `reap_stacks` freed them. The caller must not reuse or release it.
    Retired(Option<paging::MapError>),
}

/// A kernel stack mapped by hal: fresh frames at [bottom, top), nothing mapped in the guard below. Dropping it
/// (or the context that owns it) **retires** it: it is unmapped and its frames and range given back by
/// `reap_stacks`, after every CPU passed a quiescent point (module doc), never right away.
pub struct KernelStack {
    inner: Option<Box<StackInner>>,
}

/// What a stack is made of; moves through the retire lists by its `next` link.
struct StackInner {
    bounds: StackBounds,
    free_frame: fn(u64),
    release: fn(u64),
    /// The frames mapped at [bottom, top), in order (kept for freeing; unmapping never allocates).
    frames: alloc::vec::Vec<u64>,
    /// Sync generation requested after the unmap (0 = still mapped).
    sync_gen: u64,
    next: AtomicPtr<StackInner>,
}

impl KernelStack {
    /// Maps a stack of `pages` pages above `guard_pages` unmapped ones, starting at `base`. The whole range
    /// must be unmapped kernel vmap space (checked). `alloc_frame` must hand out frames nobody else uses (mm-core's
    /// allocator: the paging soundness rule), `alloc_table` frames for page tables; `free_frame` takes a stack
    /// frame back, `release(base)` the range, both once the retired stack is reaped. On `Retired` the pages mapped
    /// so far are retired and hal releases the range later (see `StackError::Retired`); on every other error
    /// nothing was ever mapped and the range stays the caller's. Callers serialise `map` calls that may create
    /// the same page table (sched's stack pool lock): `paging::map_4k` itself does not.
    pub fn map(
        base: u64,
        guard_pages: u64,
        pages: u64,
        alloc_frame: &mut dyn FnMut() -> Option<u64>,
        alloc_table: &mut dyn FnMut() -> Option<u64>,
        free_frame: fn(u64),
        release: fn(u64),
    ) -> Result<KernelStack, StackError> {
        let len = (guard_pages + pages).checked_mul(4096).ok_or(StackError::BadRange)?;
        let end = base.checked_add(len).ok_or(StackError::BadRange)?;
        if guard_pages == 0 || pages == 0 || base & 0xFFF != 0 || base < crate::VMAP_BASE || end > bootinfo::layout::VMAP_END {
            return Err(StackError::BadRange);
        }
        let root = stack_root();
        if (0..guard_pages + pages).any(|i| paging::translate(root, base + i * 4096).is_some()) {
            return Err(StackError::InUse);
        }
        let mut frames = alloc::vec::Vec::new();
        frames.try_reserve_exact(pages as usize).map_err(|_| StackError::NoMemory)?;
        let bottom = base + guard_pages * 4096;
        let mut fail = None;
        while (frames.len() as u64) < pages {
            let Some(f) = alloc_frame() else {
                fail = Some(StackError::NoMemory);
                break;
            };
            if let Err(e) = paging::map_4k(root, bottom + frames.len() as u64 * 4096, f, PageFlags::kernel_data(), alloc_table) {
                free_frame(f);
                fail = Some(StackError::Map(e));
                break;
            }
            frames.push(f);
        }
        let mapped = !frames.is_empty();
        let inner = StackInner { bounds: StackBounds { guard: base, bottom, top: end }, free_frame, release, frames, sync_gen: 0, next: AtomicPtr::new(core::ptr::null_mut()) };
        match fail {
            None => Ok(KernelStack { inner: Some(Box::new(inner)) }),
            // Nothing was ever present: no CPU can hold a translation, the range stays the caller's.
            Some(e) if !mapped => Err(e),
            Some(e) => {
                // Present pages may sit in other CPUs' TLBs (speculative walks, SDM 4.10.2.3): the same path as a
                // dropped stack, unmapped by the reaper and freed after a sync generation, range included.
                retire(Box::new(inner));
                Err(StackError::Retired(match e {
                    StackError::Map(m) => Some(m),
                    _ => None,
                }))
            }
        }
    }

    pub fn bounds(&self) -> StackBounds {
        self.inner.as_ref().map(|i| i.bounds).unwrap_or_default()
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        if let Some(i) = self.inner.take() {
            retire(i);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Retired stacks and quiescent points (SC-3; module doc)

/// Stacks waiting to be unmapped (any CPU pushes; `reap_stacks` takes the whole list).
static RETIRED: AtomicPtr<StackInner> = AtomicPtr::new(core::ptr::null_mut());
/// Unmapped stacks waiting for their sync generation; only the reaper (holder of `REAPING`) touches it.
static UNMAPPED: AtomicPtr<StackInner> = AtomicPtr::new(core::ptr::null_mut());
static REAPING: AtomicBool = AtomicBool::new(false);
/// Stacks retired and not yet freed (statistics, tests).
static RETIRED_COUNT: AtomicU64 = AtomicU64::new(0);
/// Requested sync generation: a CPU that saw it flushed its TLB after the request.
static SYNC_GEN: AtomicU64 = AtomicU64::new(0);

/// Lock-free push (Treiber stack; push-only producers and a consumer that swaps out the whole list, so no ABA).
fn push_list(head: &AtomicPtr<StackInner>, node: Box<StackInner>) {
    let p = Box::into_raw(node);
    let mut h = head.load(Ordering::Acquire);
    loop {
        // SAFETY: `p` came from Box::into_raw just above and is not published yet: ours alone.
        unsafe { (*p).next.store(h, Ordering::Relaxed) };
        match head.compare_exchange_weak(h, p, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return,
            Err(now) => h = now,
        }
    }
}

/// Takes a whole list (in LIFO order) back as boxes (they go back onto a list as they are, so boxed).
#[allow(clippy::vec_box)]
fn take_list(head: &AtomicPtr<StackInner>) -> alloc::vec::Vec<Box<StackInner>> {
    let mut p = head.swap(core::ptr::null_mut(), Ordering::AcqRel);
    let mut v = alloc::vec::Vec::new();
    while !p.is_null() {
        // SAFETY: every node on the lists came from Box::into_raw in `push_list`; the swap above made this
        // whole chain ours (pushers only ever link a new head in front of what they saw).
        let b = unsafe { Box::from_raw(p) };
        p = b.next.swap(core::ptr::null_mut(), Ordering::AcqRel);
        v.push(b);
    }
    v
}

fn retire(i: Box<StackInner>) {
    RETIRED_COUNT.fetch_add(1, Ordering::Relaxed);
    push_list(&RETIRED, i);
}

/// Whether stacks wait to be unmapped or freed.
pub fn retired_pending() -> bool {
    !RETIRED.load(Ordering::Acquire).is_null() || !UNMAPPED.load(Ordering::Acquire).is_null()
}

/// Stacks retired and not freed yet.
pub fn retired_count() -> u64 {
    RETIRED_COUNT.load(Ordering::Relaxed)
}

/// What one `reap_stacks` pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StackReap {
    /// Stacks unmapped in this pass (they wait for `sync_gen`).
    pub unmapped: usize,
    /// Stacks freed in this pass (frames and range given back).
    pub freed: usize,
    /// Unmapped stacks still waiting for their generation.
    pub waiting: usize,
    /// The generation the newly unmapped stacks wait for (0: none unmapped).
    pub sync_gen: u64,
}

/// One pass of the stack reaper (thread context, interrupts on or off; allocates a little): unmaps every
/// retired stack (local `invlpg`), requests a sync generation for them, and frees every unmapped stack whose
/// generation all CPUs have passed. The caller makes the other CPUs reach a quiescent point (sched: its
/// reschedule IPI) and calls again. `None`: another pass runs right now.
pub fn reap_stacks() -> Option<StackReap> {
    if REAPING.swap(true, Ordering::Acquire) {
        return None;
    }
    let mut r = StackReap::default();
    let fresh = take_list(&RETIRED);
    if !fresh.is_empty() {
        let root = stack_root();
        let mut wait = alloc::vec::Vec::new();
        for s in fresh {
            r.unmapped += 1;
            // MM-6: one confirmed TLB shootdown per stack (no allocation for a stack); frames come back only
            // after every CPU answered, so slot and frames are free at once.
            let len = s.frames.len() as u64 * 4096;
            let mut back = 0u64;
            let n = paging::unmap_kernel_pages(root, s.bounds.bottom, len, &mut |f| {
                (s.free_frame)(f);
                back += 1;
            });
            if n > 0 || len == 0 {
                if back == n {
                    (s.release)(s.bounds.guard);
                    r.freed += 1;
                }
                // Otherwise a CPU did not answer: MM keeps those frames out of use; the range stays reserved.
                RETIRED_COUNT.fetch_sub(1, Ordering::Relaxed);
                continue;
            }
            // Outside MM's vmap check (or nothing mapped there): the SC-3 way, local unmaps and a sync generation.
            for i in 0..s.frames.len() as u64 {
                paging::unmap_4k(root, s.bounds.bottom + i * 4096);
            }
            wait.push(s);
        }
        if !wait.is_empty() {
            // After every unmap: a CPU that sees this generation flushes after the PTEs were cleared.
            r.sync_gen = request_sync();
            for mut s in wait {
                s.sync_gen = r.sync_gen;
                push_list(&UNMAPPED, s);
            }
        }
    }
    for s in take_list(&UNMAPPED) {
        if sync_passed(s.sync_gen) {
            for &f in &s.frames {
                (s.free_frame)(f);
            }
            (s.release)(s.bounds.guard);
            RETIRED_COUNT.fetch_sub(1, Ordering::Relaxed);
            r.freed += 1;
        } else {
            r.waiting += 1;
            push_list(&UNMAPPED, s);
        }
    }
    REAPING.store(false, Ordering::Release);
    Some(r)
}

/// Asks every CPU to pass a quiescent point; returns the generation to wait for with `sync_passed`.
pub fn request_sync() -> u64 {
    SYNC_GEN.fetch_add(1, Ordering::SeqCst) + 1
}

/// The generation requested last.
pub fn sync_generation() -> u64 {
    SYNC_GEN.load(Ordering::SeqCst)
}

/// True once every CPU that runs threads (`init_this_cpu` done) passed a quiescent point at or after `sync_gen`.
pub fn sync_passed(sync_gen: u64) -> bool {
    if !CPUS_READY.load(Ordering::Acquire) {
        return true;
    }
    // `ready` SeqCst: pairs with `init_this_cpu` (see there) and with `request_sync`'s SeqCst increment.
    (0..crate::percpu::cpu_count()).all(|c| CPUS.get_for(c).is_none_or(|r| !r.ready.load(Ordering::SeqCst) || r.sync_seen.load(Ordering::Acquire) >= sync_gen))
}

/// CPUs that have not passed `sync_gen` yet (for the scheduler's IPIs and its stall report).
pub fn sync_lagging(sync_gen: u64) -> impl Iterator<Item = usize> {
    let ready = CPUS_READY.load(Ordering::Acquire);
    (0..crate::percpu::cpu_count())
        .filter(move |&c| ready && CPUS.get_for(c).is_some_and(|r| r.ready.load(Ordering::SeqCst) && r.sync_seen.load(Ordering::Acquire) < sync_gen))
}

/// A quiescent point of this CPU: if a newer sync generation was requested, flush the whole TLB (global entries
/// too) and record it. Call from thread context or a scheduler IPI, never from inside another interrupt handler.
pub fn quiescent_point() {
    let _irq = crate::cpu::IrqGuard::new();
    if !CPUS_READY.load(Ordering::Acquire) {
        return;
    }
    let Some(c) = CPUS.get_for(crate::percpu::this_cpu()) else { return };
    if !c.ready.load(Ordering::Acquire) {
        return;
    }
    let g = SYNC_GEN.load(Ordering::SeqCst);
    if c.sync_seen.load(Ordering::Relaxed) < g {
        paging::flush_all();
        c.sync_seen.store(g, Ordering::Release);
    }
}

/// `KernelContext::new_thread`'s `cr3` for "the kernel root": resolved at every switch from `kernel_root()`, so
/// a thread never holds a copy of a root that MM may replace later.
pub const KERNEL_CR3: u64 = u64::MAX;

/// The kernel's own root (bitsboot's tables with the populated kernel half), set by the scheduler at start-up.
static KERNEL_ROOT: AtomicU64 = AtomicU64::new(0);

/// Sets the root that `KERNEL_CR3` threads run with (sched::init with the boot CR3; MM when it replaces the
/// kernel root, e.g. for PCID). The root must stay valid for as long as any thread can be switched to.
pub fn set_kernel_root(root: u64) {
    KERNEL_ROOT.store(root, Ordering::Release);
}

/// The root that `KERNEL_CR3` threads run with (0 before `set_kernel_root`).
pub fn kernel_root() -> u64 {
    KERNEL_ROOT.load(Ordering::Acquire)
}

/// The root kernel-half mappings are made, checked and removed in (kernel stacks; `mmio`): the kernel root, whose
/// kernel half every user root shares, never whatever CR3 holds. CR3 may be a program's root held lazily (MM-6),
/// and a thread that read it and was preempted keeps that value after the program ended and its tables were
/// freed: the reaper walked such a root, found none of a live stack's pages and freed the stack (frames and
/// slot) a second time (INT-A2a2, `stack.slot_in_use`). Before `set_kernel_root` only the boot tables exist,
/// so the loaded root is the kernel's.
pub(crate) fn stack_root() -> u64 {
    match kernel_root() {
        0 => paging::current_root(),
        k => k,
    }
}

/// A thread's saved kernel context: stack pointer, CR3, stack, XSAVE area.
pub struct KernelContext {
    rsp: AtomicU64,
    /// The CR3 this thread runs with (0: keep whatever is loaded; `KERNEL_CR3`: the kernel root of the moment).
    cr3: AtomicU64,
    id: u64,
    bounds: StackBounds,
    stack: AtomicPtr<StackInner>,
    xstate: Option<FpuState>,
    dead: AtomicBool,
    /// Set while a CPU runs this context (from the switch to it until `finish` after the switch away), so it is
    /// never resumed twice or before its registers are saved.
    on_cpu: AtomicBool,
    /// Per-thread data of a lower crate (`with_ext`, `with_current_ext`).
    ext: Option<Box<dyn Any + Send + Sync>>,
    /// The thread's user GS base while it is not running (IA32_KERNEL_GS_BASE holds it while it runs in the
    /// kernel; `swapgs` makes it GS on the way to user mode). Saved and loaded by `switch_to`.
    user_gs: AtomicU64,
}

impl KernelContext {
    /// The code running now on this CPU (the boot thread): its stack is not hal's, so its bounds are unknown.
    pub fn adopt_running(id: u64, xstate: Option<FpuState>) -> Self {
        Self::adopt_running_on(id, xstate, StackBounds::default())
    }

    /// The code running now on this CPU, on a stack hal knows but does not own (an AP's boot stack from
    /// `smp::boot_stack`): its bounds let the guard hooks name an overflow there. The stack is never freed.
    pub fn adopt_running_on(id: u64, xstate: Option<FpuState>, bounds: StackBounds) -> Self {
        KernelContext {
            rsp: AtomicU64::new(0),
            cr3: AtomicU64::new(crate::cpu::read_cr3()),
            id,
            bounds,
            stack: AtomicPtr::new(core::ptr::null_mut()),
            xstate,
            dead: AtomicBool::new(false),
            on_cpu: AtomicBool::new(false),
            ext: None,
            user_gs: AtomicU64::new(crate::msr::read(crate::msr::IA32_KERNEL_GS_BASE)),
        }
    }

    /// A new thread on `stack`: the first switch to it calls `entry(arg)` (after `finish`), which must never
    /// return. It runs with `cr3` (0: the CR3 of whoever switches to it; `KERNEL_CR3`: the kernel root) and
    /// with interrupts off until it turns them on itself.
    pub fn new_thread(id: u64, mut stack: KernelStack, entry: extern "C" fn(usize) -> !, arg: usize, xstate: Option<FpuState>, cr3: u64) -> Self {
        let bounds = stack.bounds();
        let inner = stack.inner.take();
        // A stack without frames (never happens from `map`) gives a context that can never be started.
        let rsp = if inner.is_some() { imp::initial_frame(bounds.top, entry, arg) } else { 0 };
        KernelContext {
            rsp: AtomicU64::new(rsp),
            cr3: AtomicU64::new(cr3),
            id,
            bounds,
            stack: AtomicPtr::new(inner.map_or(core::ptr::null_mut(), Box::into_raw)),
            xstate,
            dead: AtomicBool::new(false),
            on_cpu: AtomicBool::new(false),
            ext: None,
            user_gs: AtomicU64::new(0),
        }
    }

    /// Attaches per-thread data that `with_current_ext` hands out while this context runs (before the context is
    /// shared; one per context).
    pub fn with_ext(mut self, ext: Box<dyn Any + Send + Sync>) -> Self {
        self.ext = Some(ext);
        self
    }

    pub fn ext(&self) -> Option<&(dyn Any + Send + Sync)> {
        self.ext.as_deref()
    }

    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn saved_rsp(&self) -> u64 {
        self.rsp.load(Ordering::Relaxed)
    }
    /// The saved CR3 (`KERNEL_CR3` = the kernel root).
    pub fn saved_cr3(&self) -> u64 {
        self.cr3.load(Ordering::Relaxed)
    }
    pub fn stack_bounds(&self) -> StackBounds {
        self.bounds
    }
    /// Whether the stack is still mapped (false after the thread's last switch).
    pub fn has_stack(&self) -> bool {
        !self.stack.load(Ordering::Acquire).is_null()
    }
    pub fn xstate(&self) -> Option<&FpuState> {
        self.xstate.as_ref()
    }
    /// True once the thread left for the last time (`switch_to(.., true)`); it can never run again.
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }
    /// True while a CPU runs this context (until `finish` after the switch away has saved its registers).
    pub fn is_on_cpu(&self) -> bool {
        self.on_cpu.load(Ordering::Acquire)
    }

    /// The user GS base this thread gets when it next runs (the saved one; for the running thread it lives in
    /// IA32_KERNEL_GS_BASE and `switch_to` saves it from there).
    pub fn user_gs_base(&self) -> u64 {
        self.user_gs.load(Ordering::Relaxed)
    }

    /// Sets the user GS base of a thread that is not running (a new thread of a process: `clone` copies the
    /// parent's). The running thread changes its own by writing IA32_KERNEL_GS_BASE (arch_prctl). False for a
    /// non-canonical value (wrmsr would fault) or a context that runs now.
    pub fn set_user_gs_base(&self, base: u64) -> bool {
        let canonical = ((base as i64) << 16 >> 16) as u64 == base;
        if !canonical || self.is_on_cpu() {
            return false;
        }
        self.user_gs.store(base, Ordering::Relaxed);
        true
    }

    fn take_stack(&self) -> Option<Box<StackInner>> {
        let p = self.stack.swap(core::ptr::null_mut(), Ordering::AcqRel);
        // SAFETY: non-null values come only from Box::into_raw in `new_thread`; the swap hands each out once.
        (!p.is_null()).then(|| unsafe { Box::from_raw(p) })
    }
}

impl Drop for KernelContext {
    fn drop(&mut self) {
        // Nobody can switch to a context without an Arc to it, and hal holds one for the running and the
        // just-left context of every CPU, so a dropped context's stack is not in use; other CPUs may still
        // hold its translations, hence retired, not freed.
        if let Some(s) = self.take_stack() {
            retire(s);
        }
    }
}

/// A context as hal holds it: the owner (keeps it alive) and the context it gave at `init_this_cpu` or
/// `switch_to`, captured once. hal never asks `context()` again, so an impl that returns different contexts on
/// different calls cannot make hal resume or release one it did not check.
struct Held {
    /// Keeps the context alive; hal itself uses only `ctx` (`current_owner` hands out clones).
    owner: Arc<dyn Switchable>,
    ctx: *const KernelContext,
}

// SAFETY: `ctx` points into the value `owner` keeps alive, and that value is Send + Sync (`Switchable`'s
// bounds); moving a Held is moving the Arc plus a shared reference into it.
unsafe impl Send for Held {}

impl Held {
    fn new(owner: Arc<dyn Switchable>) -> Held {
        let ctx: *const KernelContext = owner.context();
        Held { owner, ctx }
    }
    fn ctx(&self) -> &KernelContext {
        // SAFETY: `ctx` came from `owner.context()`, a shared borrow of the value behind `owner`; `owner` is held
        // as long as this `Held`, an Arc's value never moves, and only shared access exists to it, so the
        // referent stays alive and in place for the lifetime of `&self`.
        unsafe { &*self.ctx }
    }
}

/// Per-CPU record. `busy` is this CPU's switch section: whoever sets it (switch_to, init_this_cpu,
/// overflow_selftest) is the only code touching the cells until it is cleared, also against an NMI or fault
/// hook that calls back into hal on the same CPU (refused with `Reentered`). switch_to clears it only in
/// `finish`, on the thread it switched to. The other fields are atomics the fault hooks and other CPUs may read.
struct CpuSwitch {
    current: UnsafeCell<Option<Held>>,
    prev: UnsafeCell<Option<(Held, bool)>>,
    busy: AtomicBool,
    id: AtomicU64,
    guard: AtomicU64,
    bottom: AtomicU64,
    top: AtomicU64,
    /// rsp of an armed `overflow_selftest` (0 = none): where an overflow of this CPU's running stack resumes.
    landing_rsp: AtomicU64,
    switches: AtomicU64,
    /// The context `publish` made current (for `with_current_ext`); null before `init_this_cpu`.
    cur_ctx: AtomicPtr<KernelContext>,
    /// The CPU runs threads (`init_this_cpu` done): it takes part in sync generations.
    ready: AtomicBool,
    /// The newest sync generation this CPU flushed its TLB for.
    sync_seen: AtomicU64,
}

// SAFETY: the UnsafeCells are accessed only by the CPU the record belongs to and only while that CPU holds its
// `busy` flag (taken with swap, so a nested caller on the same CPU is refused); everything else is atomic.
unsafe impl Sync for CpuSwitch {}

static CPUS: PerCpu<CpuSwitch> = PerCpu::new(|_| CpuSwitch {
    current: UnsafeCell::new(None),
    prev: UnsafeCell::new(None),
    busy: AtomicBool::new(false),
    id: AtomicU64::new(0),
    guard: AtomicU64::new(0),
    bottom: AtomicU64::new(0),
    top: AtomicU64::new(0),
    landing_rsp: AtomicU64::new(0),
    switches: AtomicU64::new(0),
    cur_ctx: AtomicPtr::new(core::ptr::null_mut()),
    ready: AtomicBool::new(false),
    sync_seen: AtomicU64::new(0),
});

/// Set once `CPUS` exists (first `init_this_cpu`): `with_current_ext` must never be the first user of `CPUS`,
/// whose creation allocates (it is called from inside lock bookkeeping).
static CPUS_READY: AtomicBool = AtomicBool::new(false);

impl CpuSwitch {
    /// Takes this CPU's switch section; false if it is held (a hook re-entered hal on this CPU).
    fn enter(&self) -> bool {
        !self.busy.swap(true, Ordering::Acquire)
    }
    fn leave(&self) {
        self.busy.store(false, Ordering::Release);
    }
}

fn publish(cpu: &CpuSwitch, c: &KernelContext) {
    cpu.id.store(c.id, Ordering::Relaxed);
    cpu.guard.store(c.bounds.guard, Ordering::Relaxed);
    cpu.bottom.store(c.bounds.bottom, Ordering::Relaxed);
    cpu.top.store(c.bounds.top, Ordering::Relaxed);
    cpu.cur_ctx.store(c as *const KernelContext as *mut KernelContext, Ordering::Release);
    crate::percpu::set_current(c.id);
}

/// Calls `f` with the extension of the context running on this CPU (None before `init_this_cpu`, on a CPU
/// without a record, or for a context without one). Lock-free and allocation-free; safe from any context on
/// this CPU, NMI included. Interrupts are off while `f` runs, so the caller cannot be switched out or moved to
/// another CPU between finding this CPU's record and finishing with the context it points to.
pub fn with_current_ext<R>(f: impl FnOnce(Option<&(dyn Any + Send + Sync)>) -> R) -> R {
    let _irq = crate::cpu::IrqGuard::new();
    if !CPUS_READY.load(Ordering::Acquire) {
        return f(None);
    }
    let Some(cpu) = CPUS.get_for(crate::percpu::this_cpu()) else { return f(None) };
    let p = cpu.cur_ctx.load(Ordering::Acquire);
    if p.is_null() {
        return f(None);
    }
    // SAFETY: interrupts are off from before `this_cpu()` until `f` returns, so this code stays on this CPU
    // and is not switched out: `cpu` is this CPU's record and `p` is the context running here (the caller's
    // own, or the one an NMI interrupted). `cur_ctx` is only ever set by `publish` on this CPU, to a context
    // held in this CPU's `current` cell (or in the `Held` that moves there right after); the context it
    // replaced stays in `prev` until `finish`, which runs after the next `publish`. Neither can happen on this
    // CPU while `f` runs (a switch needs this code to give up the CPU), and other CPUs never write this record.
    let c = unsafe { &*p };
    f(c.ext())
}

/// Makes `running` the current context of this CPU (once per CPU, before its first `switch_to`). Returns false
/// if this CPU already has one or is inside a switch.
pub fn init_this_cpu(running: Arc<dyn Switchable>) -> bool {
    let _irq = crate::cpu::IrqGuard::new();
    let cpu = CPUS.get();
    CPUS_READY.store(true, Ordering::Release);
    if !cpu.enter() {
        return false;
    }
    // SAFETY: own CPU, inside its switch section (see CpuSwitch).
    let cur = unsafe { &mut *cpu.current.get() };
    let held = Held::new(running);
    let ok = cur.is_none() && !held.ctx().on_cpu.swap(true, Ordering::AcqRel);
    if ok {
        publish(cpu, held.ctx());
        *cur = Some(held);
        // From now on this CPU takes part in sync generations. `ready` goes first (SeqCst, while `sync_seen` is
        // still behind): a reaper whose SeqCst load of `ready` saw false requested its generation before this
        // CPU's SeqCst load of SYNC_GEN below, so the flush that follows comes after that reaper's unmap; one
        // that saw true waits for `sync_seen`. (Storing `ready` last would let a reaper skip this CPU between
        // its flush and the store, while a stale translation from before the unmap could be cached.)
        cpu.ready.store(true, Ordering::SeqCst);
        let g = SYNC_GEN.load(Ordering::SeqCst);
        paging::flush_all();
        cpu.sync_seen.store(g, Ordering::Release);
    }
    cpu.leave();
    ok
}

/// The owner of the context running on this CPU (a clone of what `init_this_cpu`/`switch_to` got). `None`
/// before `init_this_cpu`, or when called from a hook that interrupted a switch on this CPU.
pub fn current_owner() -> Option<Arc<dyn Switchable>> {
    let _irq = crate::cpu::IrqGuard::new();
    if !CPUS_READY.load(Ordering::Acquire) {
        return None;
    }
    let cpu = CPUS.get_for(crate::percpu::this_cpu())?;
    if !cpu.enter() {
        return None;
    }
    // SAFETY: own CPU (interrupts off since before `this_cpu()`), inside its switch section (see CpuSwitch).
    let r = unsafe { (*cpu.current.get()).as_ref().map(|h| h.owner.clone()) };
    cpu.leave();
    r
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchError {
    /// `init_this_cpu` has not run on this CPU.
    NotInitialised,
    /// The target left for good already.
    TargetDead,
    /// The target runs (or is still leaving another CPU), or was never started (no saved stack pointer).
    TargetNotReady,
    /// Called from a hook (NMI, fault) that interrupted a switch on this CPU; nothing was changed.
    Reentered,
    /// Called on an IST stack (an NMI, #DF or #MC hook) or outside the running context's own stack: a thread
    /// parked there would be overwritten by the next such exception. Nothing was changed.
    WrongStack,
}

/// True if `sp` may be parked by a switch: not on one of this CPU's IST stacks, and inside the running context's
/// stack when its bounds are known (the boot thread's are not).
fn on_thread_stack(sp: u64, own: StackBounds) -> bool {
    let on_ist = crate::gdt::ist_stacks().iter().any(|s| s.bottom != 0 && s.contains(sp));
    !on_ist && (own.bottom == 0 || own.contains(sp))
}

/// Switches from the running context to `to`. `from_done`: the running thread never runs again (its stack is
/// retired by `finish` and unmapped and freed by `reap_stacks` once every CPU passed a quiescent point). Returns when something switches back (never if
/// `from_done`). The caller has interrupts off or not; they are off inside and the thread's own IF comes back
/// with it. On `Err` nothing changed and the caller keeps running.
pub fn switch_to(to: Arc<dyn Switchable>, from_done: bool) -> Result<(), SwitchError> {
    let _irq = crate::cpu::IrqGuard::new();
    let cpu = CPUS.get();
    if !cpu.enter() {
        return Err(SwitchError::Reentered);
    }
    // SAFETY: own CPU, inside its switch section (see CpuSwitch).
    let cur = unsafe { &mut *cpu.current.get() };
    let refuse = |e| {
        cpu.leave();
        Err(e)
    };
    let Some(from) = cur.take() else { return refuse(SwitchError::NotInitialised) };
    // Any local lives on the stack this runs on.
    let probe = 0u8;
    let sp = core::hint::black_box(&probe) as *const u8 as u64;
    if !on_thread_stack(sp, from.ctx().bounds) {
        *cur = Some(from);
        return refuse(SwitchError::WrongStack);
    }
    let to = Held::new(to);
    if core::ptr::addr_eq(from.ctx, to.ctx) {
        *cur = Some(from);
        cpu.leave();
        return Ok(());
    }
    // Every check below and the switch itself use the one context captured in `to`.
    let t = to.ctx();
    if t.is_dead() {
        *cur = Some(from);
        return refuse(SwitchError::TargetDead);
    }
    if t.saved_rsp() == 0 || t.on_cpu.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        *cur = Some(from);
        return refuse(SwitchError::TargetNotReady);
    }
    let fc: *const KernelContext = from.ctx;
    let tc: *const KernelContext = to.ctx;
    // SAFETY: both point into contexts kept alive by the owners in `from` and `to` (Held::ctx), which move into
    // the per-CPU cells below and stay there until `finish` after the switch.
    let (f, t) = unsafe { (&*fc, &*tc) };
    if from_done {
        f.dead.store(true, Ordering::Release);
    } else if let Some(x) = f.xstate() {
        x.save();
    }
    // MM-6 lazy TLB: a CPU that holds a user root only lazily reports the kernel root here, so a kernel thread
    // never saves a user root as its own.
    let cr3_now = paging::thread_root();
    let kroot = kernel_root();
    let saved = if kroot != 0 && (cr3_now ^ kroot) & !0xFFF == 0 { KERNEL_CR3 } else { cr3_now };
    f.cr3.store(saved, Ordering::Relaxed);
    if let Some(x) = t.xstate() {
        x.restore();
    }
    // The user GS base travels with the thread, not the CPU (a thread blocked in a syscall may resume elsewhere).
    let gs_now = crate::msr::read(crate::msr::IA32_KERNEL_GS_BASE);
    if !from_done {
        f.user_gs.store(gs_now, Ordering::Relaxed);
    }
    let gs_want = t.user_gs.load(Ordering::Relaxed);
    if gs_want != gs_now {
        crate::msr::write(crate::msr::IA32_KERNEL_GS_BASE, gs_want);
    }
    match t.cr3.load(Ordering::Relaxed) {
        // A kernel thread keeps whatever user root this CPU has loaded (lazy TLB), or gets the kernel root.
        KERNEL_CR3 => paging::activate_lazy_kernel(),
        // A saved user root whose context is gone was the CPU's root when a kernel thread was switched out (an
        // interrupt handler loaded it, e.g. MM's AP probes), and its space has ended since: never load a freed
        // root; the kernel root serves a kernel thread (a process thread's own root lives while it runs).
        want if want != 0 && (want ^ cr3_now) & !0xFFF != 0 => {
            if kroot == 0 || (want ^ kroot) & !0xFFF == 0 {
                paging::activate(want)
            } else {
                paging::activate_saved(want)
            }
        }
        _ => {}
    }
    publish(cpu, t);
    // SAFETY: own CPU, inside its switch section; `finish` emptied `prev` after the previous switch here.
    unsafe {
        let prev = &mut *cpu.prev.get();
        // A non-empty slot would be a missed `finish`; dropping it here is still correct (that switch is over).
        drop(prev.take());
        *prev = Some((from, from_done));
    }
    *cur = Some(to);
    imp::switch(&f.rsp, t.rsp.load(Ordering::Relaxed));
    finish();
    Ok(())
}

/// The second half of every switch, run by the thread that was switched to (`switch_to` after the assembly
/// returns, or the start trampoline of a new thread): the left thread's context is released; a finished
/// thread's stack is retired now that nothing runs on it (`reap_stacks` frees it later); the switch section ends.
fn finish() {
    let cpu = CPUS.get();
    // SAFETY: own CPU, still inside the switch section `switch_to` took on the thread that switched here
    // (interrupts are off: both paths restore an RFLAGS saved with IF=0).
    let prev = unsafe { (*cpu.prev.get()).take() };
    if let Some((p, done)) = prev {
        // Its registers are saved now: another CPU may resume it from here on.
        p.ctx().on_cpu.store(false, Ordering::Release);
        if done && let Some(s) = p.ctx().take_stack() {
            retire(s);
        }
        drop(p);
    }
    cpu.switches.fetch_add(1, Ordering::Relaxed);
    cpu.leave();
    quiescent_point();
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn hal_switch_finish() {
    finish();
}

/// Id of the context running on this CPU (0 before `init_this_cpu`).
pub fn current_id() -> u64 {
    CPUS.get().id.load(Ordering::Relaxed)
}

/// Stack bounds of the context running on this CPU (zeros for the boot thread). Lock-free, for fault hooks.
pub fn current_stack() -> StackBounds {
    let c = CPUS.get();
    StackBounds { guard: c.guard.load(Ordering::Relaxed), bottom: c.bottom.load(Ordering::Relaxed), top: c.top.load(Ordering::Relaxed) }
}

/// Switches done on this CPU.
pub fn switch_count() -> u64 {
    CPUS.get().switches.load(Ordering::Relaxed)
}

/// Switches done on `cpu`.
pub fn switch_count_of(cpu: usize) -> u64 {
    CPUS.get_for(cpu).map_or(0, |c| c.switches.load(Ordering::Relaxed))
}

/// Switches done on all CPUs together.
pub fn total_switches() -> u64 {
    (0..crate::percpu::cpu_count()).map(switch_count_of).sum()
}

/// The test of a real stack overflow (QemuOnly, ktest `sc.stack.overflow`): with interrupts off, hal's own
/// assembly recurses (about 0.5 KiB per frame) from the calling thread's current depth until it runs into the
/// guard page; the scheduler's #DF/#PF hook (`take_overflow_landing`) then resumes right here with the stack
/// pointer and callee-saved registers of this call, and the number of frames is returned. Only hal's own
/// recursion frames are abandoned (they hold nothing); every caller frame is untouched, so this is safe to call
/// from any thread. `None`: not under a hypervisor (resuming after a #DF is architecturally undefined, SDM 6.15,
/// as CP's #DF self-test says), not on a hal stack, or this CPU is inside a switch.
pub fn overflow_selftest() -> Option<u64> {
    if !crate::cpu::live().has("hypervisor") || current_stack().guard == 0 {
        return None;
    }
    let _irq = crate::cpu::IrqGuard::new();
    let cpu = CPUS.get();
    // The switch section keeps this CPU on this thread (and its guard in `current_stack`) until the landing.
    if !cpu.enter() {
        return None;
    }
    let mut depth = 0u64;
    imp::overflow_selftest(&mut depth, &cpu.landing_rsp);
    cpu.landing_rsp.store(0, Ordering::Relaxed);
    cpu.leave();
    Some(depth)
}

/// For the scheduler's #DF/#PF hooks: if an `overflow_selftest` is armed on this CPU and `fault_addr` (CR2) or
/// the trapped rsp lies in the running thread's guard, rewrites `frame` to resume that self-test (interrupts
/// off) and returns true.
pub fn take_overflow_landing(frame: &mut TrapFrame, fault_addr: u64) -> bool {
    let b = current_stack();
    if !(b.in_guard(fault_addr) || b.in_guard(frame.rsp)) {
        return false;
    }
    let rsp = CPUS.get().landing_rsp.swap(0, Ordering::Relaxed);
    if rsp == 0 {
        return false;
    }
    frame.rip = imp::overflow_resume_site();
    frame.cs = crate::gdt::KERNEL_CS as u64;
    frame.ss = crate::gdt::KERNEL_SS as u64;
    frame.rflags = 0x2;
    frame.rsp = rsp;
    true
}

/// Where `probe_current_guard` loads (fault rip) and where a hook resumes it (resume rip).
pub fn guard_probe_site() -> (u64, u64) {
    imp::probe_site()
}

/// Loads 8 bytes from the lowest guard page of the running thread, which must fault: a #PF hook that
/// recognises `guard_probe_site().0` resumes at `.1` with its own value in rax, which is returned. `None` if
/// this thread has no hal stack or the guard is mapped (then nothing is read). Without such a hook the
/// kernel panics on the #PF.
pub fn probe_current_guard() -> Option<u64> {
    let b = current_stack();
    if b.guard == 0 || paging::translate(stack_root(), b.guard).is_some() {
        return None;
    }
    Some(imp::probe_load(b.guard))
}

#[cfg(target_os = "none")]
mod imp {
    use core::sync::atomic::AtomicU64;

    unsafe extern "C" {
        fn hal_context_switch(from_rsp: *mut u64, to_rsp: u64);
        fn hal_thread_trampoline();
        fn hal_guard_probe(addr: u64) -> u64;
        static hal_guard_probe_fault: u8;
        static hal_guard_probe_resume: u8;
        fn hal_overflow_selftest(depth: *mut u64, landing_rsp: *mut u64);
        static hal_overflow_resume: u8;
    }

    core::arch::global_asm!(
        r#"
        .text
        .global hal_context_switch
    hal_context_switch:
        pushfq
        pushq %rbp
        pushq %rbx
        pushq %r12
        pushq %r13
        pushq %r14
        pushq %r15
        movq %rsp, (%rdi)
        movq %rsi, %rsp
        popq %r15
        popq %r14
        popq %r13
        popq %r12
        popq %rbx
        popq %rbp
        popfq
        ret

        .global hal_thread_trampoline
    hal_thread_trampoline:
        andq $-16, %rsp
        call hal_switch_finish
        movq %r12, %rdi
        call *%r13
        ud2

        .global hal_guard_probe
    hal_guard_probe:
        .global hal_guard_probe_fault
    hal_guard_probe_fault:
        movq (%rdi), %rax
        .global hal_guard_probe_resume
    hal_guard_probe_resume:
        ret

        // overflow_selftest: rdi = frame counter, rsi = the CPU's landing slot. Saves RFLAGS and the
        // callee-saved registers, publishes rsp as the landing, then recurses until a push or store hits the
        // guard; the fault hook resumes at hal_overflow_resume with rsp = the landing.
        .global hal_overflow_selftest
    hal_overflow_selftest:
        pushfq
        cli
        pushq %rbp
        pushq %rbx
        pushq %r12
        pushq %r13
        pushq %r14
        pushq %r15
        movq %rsp, (%rsi)
        call hal_overflow_recurse
        ud2
    hal_overflow_recurse:
        subq $504, %rsp
        incq (%rdi)
        movq %rdi, (%rsp)
        call hal_overflow_recurse
        ud2
        .global hal_overflow_resume
    hal_overflow_resume:
        popq %r15
        popq %r14
        popq %r13
        popq %r12
        popq %rbx
        popq %rbp
        popfq
        ret
    "#,
        options(att_syntax)
    );

    pub fn initial_frame(stack_top: u64, entry: extern "C" fn(usize) -> !, arg: usize) -> u64 {
        let top = stack_top & !15;
        // Popped by hal_context_switch: r15 r14 r13 r12 rbx rbp rflags (IF=0), then the return address; rbp = 0
        // ends backtraces.
        let frame: [u64; 9] = [0, 0, entry as *const () as u64, arg as u64, 0, 0, 0x2, hal_thread_trampoline as *const () as u64, 0];
        let base = top - (frame.len() as u64) * 8;
        // SAFETY: `stack_top` is the top of a KernelStack that `new_thread` now owns: mapped, writable,
        // 64 KiB or more, used by nobody yet; the 72 bytes below its top are written.
        unsafe { core::ptr::copy_nonoverlapping(frame.as_ptr(), base as *mut u64, frame.len()) };
        base
    }

    pub fn switch(from_rsp: &AtomicU64, to_rsp: u64) {
        // SAFETY: `to_rsp` was produced by initial_frame or saved by this routine when its thread switched away,
        // on a stack that is still mapped (switch_to keeps the context alive and refuses dead ones); the
        // assembly stores the old rsp through the atomic and preserves every callee-saved register and RFLAGS,
        // so this returns like a normal call when someone switches back.
        unsafe { hal_context_switch(from_rsp.as_ptr(), to_rsp) };
    }

    pub fn probe_site() -> (u64, u64) {
        // Only the addresses of the two labels are taken (no read), which needs no unsafe.
        (&raw const hal_guard_probe_fault as u64, &raw const hal_guard_probe_resume as u64)
    }

    pub fn probe_load(addr: u64) -> u64 {
        // SAFETY: `addr` is the running thread's guard page, verified unmapped by the caller, so the load faults
        // without touching memory; a #PF hook resumes after it (registers saved and restored by the trap path)
        // or the kernel panics.
        unsafe { hal_guard_probe(addr) }
    }

    pub fn overflow_selftest(depth: &mut u64, landing_rsp: &AtomicU64) {
        // SAFETY: the caller runs on a hal stack with an unmapped guard below (checked) and holds its CPU's
        // switch section with interrupts off, so nothing else runs here until the landing. The assembly writes
        // only `*depth`, the landing slot and the stack below its own entry frame (down to the guard, where it
        // faults). The fault hook resumes at hal_overflow_resume with rsp = the published landing, which pops the
        // saved callee-saved registers and RFLAGS and returns like a normal call; the abandoned frames are the
        // assembly's own and hold no Rust values. Without such a hook the kernel panics on the fault.
        unsafe { hal_overflow_selftest(depth, landing_rsp.as_ptr()) }
    }

    pub fn overflow_resume_site() -> u64 {
        // Only the label's address is taken (no read).
        &raw const hal_overflow_resume as u64
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    use core::sync::atomic::AtomicU64;
    pub fn initial_frame(_stack_top: u64, _entry: extern "C" fn(usize) -> !, _arg: usize) -> u64 {
        0
    }
    pub fn switch(_from_rsp: &AtomicU64, _to_rsp: u64) {
        panic!("kernel threads exist only in the kernel build");
    }
    pub fn probe_site() -> (u64, u64) {
        (0, 0)
    }
    pub fn probe_load(_addr: u64) -> u64 {
        0
    }
    pub fn overflow_selftest(_depth: &mut u64, _landing_rsp: &AtomicU64) {}
    pub fn overflow_resume_site() -> u64 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds() {
        let b = StackBounds { guard: 0x1000, bottom: 0x2000, top: 0x12000 };
        assert!(b.in_guard(0x1000) && b.in_guard(0x1FF8) && !b.in_guard(0x2000) && !b.in_guard(0xFFF));
        assert!(b.contains(0x2000) && b.contains(0x11FF8) && !b.contains(0x12000));
        assert!(!StackBounds::default().in_guard(0));
    }

    #[test]
    fn only_thread_stacks_switch() {
        let own = StackBounds { guard: 0x1000, bottom: 0x2000, top: 0x12000 };
        assert!(on_thread_stack(0x8000, own));
        assert!(!on_thread_stack(0x1800, own), "guard page");
        assert!(!on_thread_stack(0x20000, own), "another stack");
        assert!(on_thread_stack(0x20000, StackBounds::default()), "unknown bounds (boot thread)");
    }

    #[test]
    fn user_gs_base_must_be_canonical() {
        let c = KernelContext::adopt_running(1, None);
        assert!(c.set_user_gs_base(0x7fff_ffff_f000));
        assert_eq!(c.user_gs_base(), 0x7fff_ffff_f000);
        assert!(c.set_user_gs_base(0xffff_8000_0000_0000));
        assert!(!c.set_user_gs_base(0x0000_8000_0000_0000));
        assert_eq!(c.user_gs_base(), 0xffff_8000_0000_0000);
    }

    #[test]
    fn map_refuses_bad_ranges() {
        let mut none = || None;
        let mut none2 = || None;
        let r = KernelStack::map(0x1000, 1, 16, &mut none, &mut none2, |_| {}, |_| {});
        assert_eq!(r.err(), Some(StackError::BadRange));
        let r = KernelStack::map(crate::VMAP_BASE, 0, 16, &mut none, &mut none2, |_| {}, |_| {});
        assert_eq!(r.err(), Some(StackError::BadRange));
        let r = KernelStack::map(bootinfo::layout::VMAP_END - 4096, 1, 1, &mut none, &mut none2, |_| {}, |_| {});
        assert_eq!(r.err(), Some(StackError::BadRange));
    }
}
