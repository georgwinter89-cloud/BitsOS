//! Per-CPU data (owner: CP; contract `C: PerCpu`, V2-PLAN §9.1).
//!
//! Two things live here: the raw `CpuLocal` block that GS points to in the kernel (its field offsets are a
//! contract with the entry stubs in `entry.rs`), and `PerCpu<T>`, the safe per-CPU container every crate uses.
//! The BSP's block is static (GS is set before memory exists); every AP gets a leaked block from
//! `prepare_ap` (CP-4, no maximum). `this_cpu`, `current` and `set_current` are one GS-relative instruction
//! each, so a migration cannot split them.
//!
//! CPU numbers: `possible_cpus()` is what the MADT lists (set by `acpi::init` before the first `PerCpu` is
//! used, so every container is sized once), `cpu_count()` the CPUs that run kernel code now (0 = BSP, APs
//! numbered densely as they come up). A `PerCpu` used before `set_possible`, or a CPU beyond the MADT, grows
//! the container (allocates once; values never move, so references stay valid).

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicUsize, Ordering};

/// GS-relative block. Offsets are used by assembly: keep them in sync with `entry.rs` (CL_* constants).
#[repr(C)]
pub struct CpuLocal {
    pub self_ptr: u64,
    /// Kernel stack pointer saved by `UserContext::execute` while user code runs.
    pub kernel_rsp: u64,
    /// The user rsp at SYSCALL entry.
    pub user_rsp_scratch: u64,
    /// `*mut UserContext` of the user code running on this CPU.
    pub user_ctx: u64,
    pub cpu_index: u64,
    /// Opaque pointer the scheduler keeps (current thread).
    pub current: u64,
    /// This CPU's descriptor tables (`gdt::CpuGdt`), for `set_rsp0` and the IST stacks.
    pub gdt: u64,
    /// Hard-IRQ nesting depth (`vectors::dispatch`).
    pub irq_depth: u64,
    /// ksync's preemption blockers on this CPU (spin locks and `PreemptGuard`s held; Linux `__preempt_count`).
    pub preempt: u64,
    /// An interrupt exit found a reschedule pending but the preemption blocked (`defer_resched`): the point where
    /// the count drops to 0 with interrupts on preempts instead (Linux `preempt_enable` -> `preempt_schedule`).
    pub resched: u64,
    /// The trap frame of the interrupt `vectors::dispatch` hands to a handler on this CPU now (0: none), for a
    /// sampling profiler's look at the interrupted code (`sampling`).
    pub irq_frame: u64,
}

pub const CL_SELF: usize = 0;
pub const CL_KERNEL_RSP: usize = 8;
pub const CL_USER_RSP: usize = 16;
pub const CL_USER_CTX: usize = 24;
pub const CL_CPU_INDEX: usize = 32;
pub const CL_CURRENT: usize = 40;
pub const CL_GDT: usize = 48;
pub const CL_IRQ_DEPTH: usize = 56;
pub const CL_PREEMPT: usize = 64;
pub const CL_RESCHED: usize = 72;
pub const CL_IRQ_FRAME: usize = 80;

/// One block per cache line, so two CPUs never share a line of their GS blocks.
#[repr(C, align(64))]
struct LocalCell(UnsafeCell<CpuLocal>);
// SAFETY: the mutable fields (kernel_rsp, user_*, current, irq_depth) are written only by the CPU that owns the
// block, through GS; the fields other CPUs read (self_ptr, cpu_index, gdt) are written before the block is
// published (`prepare_ap` registers it with Release, `init_bsp` runs before any other CPU exists).
unsafe impl Sync for LocalCell {}

const fn empty_local() -> CpuLocal {
    CpuLocal { self_ptr: 0, kernel_rsp: 0, user_rsp_scratch: 0, user_ctx: 0, cpu_index: 0, current: 0, gdt: 0, irq_depth: 0, preempt: 0, resched: 0, irq_frame: 0 }
}

static BSP_LOCAL: LocalCell = LocalCell(UnsafeCell::new(empty_local()));

static CPU_COUNT: AtomicUsize = AtomicUsize::new(1);
static POSSIBLE: AtomicUsize = AtomicUsize::new(1);
static GS_READY: AtomicBool = AtomicBool::new(false);
static BSP_INIT_TAKEN: AtomicBool = AtomicBool::new(false);

/// Sets GS to the BSP's block. Called once by the kernel before anything uses `this_cpu`, after `gdt::init`.
/// (IA32_TSC_AUX = CPU index, for rdtscp/rdpid and later the vDSO's getcpu, is set by `cpu::init_this_cpu`.)
pub fn init_bsp() {
    // Once, on the BSP: a later call (on an AP it would point that AP's GS at the BSP's block) does nothing.
    if BSP_INIT_TAKEN.swap(true, Ordering::AcqRel) {
        return;
    }
    let p = BSP_LOCAL.0.get();
    // SAFETY: BSP start-up, single writer, nothing reads the block before GS_READY.
    unsafe {
        (*p).self_ptr = p as u64;
        (*p).cpu_index = 0;
        (*p).gdt = crate::gdt::bsp_tables() as *const crate::gdt::CpuGdt as u64;
    }
    crate::msr::write(crate::msr::IA32_GS_BASE, p as u64);
    crate::msr::write(crate::msr::IA32_KERNEL_GS_BASE, 0);
    GS_READY.store(true, Ordering::Release);
}

/// The blocks of the APs: index `cpu - 1` (grown copy-on-write; old slices stay leaked: they change only at
/// CPU bring-up, a few pointers per CPU).
static AP_LOCALS: AtomicPtr<Box<[&'static LocalCell]>> = AtomicPtr::new(core::ptr::null_mut());
static AP_LOCALS_LOCK: AtomicBool = AtomicBool::new(false);

fn ap_locals() -> &'static [&'static LocalCell] {
    let p = AP_LOCALS.load(Ordering::Acquire);
    if p.is_null() {
        return &[];
    }
    // SAFETY: AP_LOCALS only ever holds pointers from `Box::into_raw` of a fully built boxed slice, published
    // with Release in `prepare_ap`; they are never freed.
    unsafe { &*p }
}

fn cell_of(cpu: usize) -> Option<&'static LocalCell> {
    if cpu == 0 { Some(&BSP_LOCAL) } else { ap_locals().get(cpu - 1).copied() }
}

/// Allocates and registers the GS block of AP `cpu` (BSP side, before the AP starts) with its descriptor
/// tables; returns the address its GS base gets. A second call for the same CPU (a retried start) replaces the
/// registration; the old block stays allocated (the AP that may still hold it was sent INIT). `cpu` 0 is the
/// BSP's static block and is not changed.
pub fn prepare_ap(cpu: usize, gdt: &'static crate::gdt::CpuGdt) -> u64 {
    if cpu == 0 {
        return BSP_LOCAL.0.get() as u64;
    }
    let cell: &'static LocalCell = Box::leak(Box::new(LocalCell(UnsafeCell::new(empty_local()))));
    let p = cell.0.get();
    // SAFETY: a fresh block nobody else knows yet.
    unsafe {
        (*p).self_ptr = p as u64;
        (*p).cpu_index = cpu as u64;
        (*p).gdt = gdt as *const crate::gdt::CpuGdt as u64;
    }
    // Interrupts off while waiting and holding: the holder copies a few pointers and allocates, so the wait is
    // bounded; never called from IRQ context (it allocates).
    let _irq = crate::cpu::IrqGuard::new();
    while AP_LOCALS_LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
    let mut v: alloc::vec::Vec<&'static LocalCell> = ap_locals().to_vec();
    if v.len() < cpu {
        // Gaps (CPUs not prepared yet) point at this block until their own `prepare_ap`.
        v.resize(cpu, cell);
    }
    v[cpu - 1] = cell;
    AP_LOCALS.store(Box::into_raw(Box::new(v.into_boxed_slice())), Ordering::Release);
    AP_LOCALS_LOCK.store(false, Ordering::Release);
    p as u64
}

/// Points this CPU's GS base at `local` (an AP's first act in the kernel, from `smp`).
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
pub(crate) fn install_this_cpu(local: u64) {
    crate::msr::write(crate::msr::IA32_GS_BASE, local);
    crate::msr::write(crate::msr::IA32_KERNEL_GS_BASE, 0);
}

#[cfg(target_os = "none")]
fn gs_read(off: usize) -> u64 {
    let v: u64;
    // SAFETY (every arm): after init_bsp (and on an AP after install_this_cpu, its first act in the kernel) the
    // kernel GS base points at this CPU's CpuLocal. Kernel Rust code runs with the kernel GS: the user GS is live
    // only inside entry.rs's swapgs windows, before the stubs' iretq to user mode and inside idt.rs's GS probes
    // (`probe_load_with_gs`, `probe_iret_with_foreign_gs`), where interrupts are off;
    // the exceptions that ignore IF (NMI, #DF, #MC, from either mode) and #NP/#SS/#GP/#PF raised in those windows
    // switch to the kernel GS first (`paranoid_gs_enter`, from `idt::hal_kernel_trap`); #DB is not covered, as
    // the kernel sets no hardware breakpoints. Every offset is a field of the block. One
    // instruction, so no migration can split it.
    unsafe {
        match off {
            CL_SELF => core::arch::asm!("mov {}, gs:[{o}]", out(reg) v, o = const CL_SELF, options(nostack, readonly, preserves_flags)),
            CL_CPU_INDEX => core::arch::asm!("mov {}, gs:[{o}]", out(reg) v, o = const CL_CPU_INDEX, options(nostack, readonly, preserves_flags)),
            CL_CURRENT => core::arch::asm!("mov {}, gs:[{o}]", out(reg) v, o = const CL_CURRENT, options(nostack, readonly, preserves_flags)),
            CL_GDT => core::arch::asm!("mov {}, gs:[{o}]", out(reg) v, o = const CL_GDT, options(nostack, readonly, preserves_flags)),
            CL_IRQ_DEPTH => core::arch::asm!("mov {}, gs:[{o}]", out(reg) v, o = const CL_IRQ_DEPTH, options(nostack, readonly, preserves_flags)),
            CL_PREEMPT => core::arch::asm!("mov {}, gs:[{o}]", out(reg) v, o = const CL_PREEMPT, options(nostack, readonly, preserves_flags)),
            CL_RESCHED => core::arch::asm!("mov {}, gs:[{o}]", out(reg) v, o = const CL_RESCHED, options(nostack, readonly, preserves_flags)),
            CL_IRQ_FRAME => core::arch::asm!("mov {}, gs:[{o}]", out(reg) v, o = const CL_IRQ_FRAME, options(nostack, readonly, preserves_flags)),
            _ => v = 0,
        }
    }
    v
}

#[cfg(not(target_os = "none"))]
fn gs_read(off: usize) -> u64 {
    // SAFETY: host builds have one CPU and the static BSP block; plain reads of its fields.
    let l = unsafe { &*BSP_LOCAL.0.get() };
    match off {
        CL_SELF => BSP_LOCAL.0.get() as u64,
        CL_CPU_INDEX => l.cpu_index,
        CL_CURRENT => l.current,
        CL_GDT => l.gdt,
        CL_IRQ_DEPTH => l.irq_depth,
        CL_PREEMPT => l.preempt,
        CL_RESCHED => l.resched,
        CL_IRQ_FRAME => l.irq_frame,
        _ => 0,
    }
}

/// The GS base an NMI, #DF or #MC from kernel mode found, when it was not this CPU's block (see
/// `paranoid_gs_enter`); dropping it puts that value back.
pub(crate) struct ParanoidGs(Option<u64>);

impl Drop for ParanoidGs {
    fn drop(&mut self) {
        if let Some(v) = self.0 {
            crate::msr::write(crate::msr::IA32_GS_BASE, v);
        }
    }
}

/// Linux's paranoid entry with FSGSBASE, for NMI, #DF and #MC from either mode (no `swapgs` for them, CP-8) and
/// for faults in the return-to-user windows (`idt::in_user_gs_window`): they can arrive in user mode or inside
/// entry.rs's swapgs windows (after `syscall` before its `swapgs`, or after the return path's `swapgs` before
/// `iretq`), where the GS base is the user's, which `wrgsbase` lets a program set to anything canonical. The CPU is found by its
/// APIC ID (hardware, not user state); when the GS base is not that CPU's block, the block is written and the old
/// value comes back when the guard drops. Uses no GS, no lock and no allocation. A CPU that is not online yet (an
/// AP in its set-up, where no user code ever ran) is left as it is.
pub(crate) fn paranoid_gs_enter() -> ParanoidGs {
    if !GS_READY.load(Ordering::Acquire) {
        return ParanoidGs(None);
    }
    let Some(cpu) = crate::smp::cpu_by_apic_id() else { return ParanoidGs(None) };
    let want = local_addr_of(cpu);
    if want == 0 {
        return ParanoidGs(None);
    }
    let cur = crate::msr::read(crate::msr::IA32_GS_BASE);
    if cur == want {
        return ParanoidGs(None);
    }
    crate::msr::write(crate::msr::IA32_GS_BASE, want);
    ParanoidGs(Some(cur))
}

/// `self_ptr` read through GS (`gs:[CL_SELF]`), i.e. what the entry assembly sees; 0 before `init_bsp`.
/// Kernel context only (outside the swapgs windows of `entry.rs`).
pub fn gs_self() -> u64 {
    if !GS_READY.load(Ordering::Acquire) {
        return 0;
    }
    gs_read(CL_SELF)
}

/// Raw pointer to this CPU's block (for hal-internal users like `entry`).
pub fn local_ptr() -> *mut CpuLocal {
    if !GS_READY.load(Ordering::Acquire) {
        return BSP_LOCAL.0.get();
    }
    gs_read(CL_SELF) as *mut CpuLocal
}

/// Index of the CPU this code runs on (0 = BSP). Stable while preemption is off.
pub fn this_cpu() -> usize {
    if !GS_READY.load(Ordering::Acquire) {
        return 0;
    }
    gs_read(CL_CPU_INDEX) as usize
}

/// This CPU's descriptor tables (0 before `init_bsp`).
pub(crate) fn this_gdt() -> u64 {
    if !GS_READY.load(Ordering::Acquire) {
        return 0;
    }
    gs_read(CL_GDT)
}

/// The descriptor tables of `cpu` (0 = unknown), for reports on other CPUs' IST stacks.
pub(crate) fn gdt_of(cpu: usize) -> u64 {
    // SAFETY: `gdt` is written before the block is published (see LocalCell) and never changed afterwards.
    cell_of(cpu).map_or(0, |c| unsafe { core::ptr::addr_of!((*c.0.get()).gdt).read_volatile() })
}

/// The address the GS base of `cpu` points at (0 = not prepared).
pub fn local_addr_of(cpu: usize) -> u64 {
    cell_of(cpu).map_or(0, |c| c.0.get() as u64)
}

/// Number of CPUs that may run kernel code (grows when CP-4 starts APs).
pub fn cpu_count() -> usize {
    CPU_COUNT.load(Ordering::Acquire)
}

/// CPUs 0..n are up (hal::smp only: APs are numbered densely).
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
pub(crate) fn set_online(n: usize) {
    POSSIBLE.fetch_max(n, Ordering::AcqRel);
    CPU_COUNT.fetch_max(n, Ordering::AcqRel);
}

/// The CPUs the firmware lists (at least the ones online); `PerCpu` containers are sized to it.
pub fn possible_cpus() -> usize {
    POSSIBLE.load(Ordering::Acquire).max(cpu_count())
}

/// Sets the possible-CPU count (`acpi::init` from the MADT, before the first `PerCpu` is used). Only grows.
pub fn set_possible(n: usize) {
    POSSIBLE.fetch_max(n.max(1), Ordering::AcqRel);
}

/// Opaque scheduler pointer of this CPU.
pub fn current() -> u64 {
    if !GS_READY.load(Ordering::Acquire) {
        // SAFETY: before GS exists only the BSP runs; a plain aligned load of its own block.
        return unsafe { (*BSP_LOCAL.0.get()).current };
    }
    gs_read(CL_CURRENT)
}

pub fn set_current(v: u64) {
    if !GS_READY.load(Ordering::Acquire) {
        // SAFETY: before GS exists only the BSP runs; its own block.
        unsafe { (*BSP_LOCAL.0.get()).current = v };
        return;
    }
    #[cfg(target_os = "none")]
    // SAFETY: this CPU's own block through GS (see gs_read); one instruction.
    unsafe {
        core::arch::asm!("mov gs:[{o}], {v}", v = in(reg) v, o = const CL_CURRENT, options(nostack, preserves_flags))
    };
    #[cfg(not(target_os = "none"))]
    // SAFETY: host: one CPU, the static block.
    unsafe {
        (*BSP_LOCAL.0.get()).current = v
    };
}

/// One more preemption blocker on this CPU, in one instruction (Linux `preempt_count_add`, `incl %gs:`): no interrupt
/// and no move to another CPU can split it, so no interrupts off and no locked instruction, on every spin lock.
#[inline]
pub fn preempt_inc() {
    if !GS_READY.load(Ordering::Acquire) {
        // SAFETY: before GS exists only the BSP runs, on its own block.
        unsafe { (*BSP_LOCAL.0.get()).preempt += 1 };
        return;
    }
    #[cfg(target_os = "none")]
    // SAFETY: this CPU's own block through GS (see gs_read); one instruction.
    unsafe {
        core::arch::asm!("inc qword ptr gs:[{o}]", o = const CL_PREEMPT, options(nostack))
    };
}

/// Undoes `preempt_inc`, never below 0. While the count is above 0 this thread cannot move, and an interrupt between
/// the read and the instruction leaves the count as it found it.
#[inline]
pub fn preempt_dec() {
    if !GS_READY.load(Ordering::Acquire) {
        // SAFETY: before GS exists only the BSP runs, on its own block.
        unsafe {
            let l = &mut *BSP_LOCAL.0.get();
            l.preempt = l.preempt.saturating_sub(1);
        }
        return;
    }
    #[cfg(target_os = "none")]
    if gs_read(CL_PREEMPT) != 0 {
        // SAFETY: this CPU's own block through GS (see gs_read); one instruction.
        unsafe { core::arch::asm!("dec qword ptr gs:[{o}]", o = const CL_PREEMPT, options(nostack)) };
        if gs_read(CL_PREEMPT) == 0 && gs_read(CL_RESCHED) != 0 {
            run_resched_hook();
        }
    }
}

/// The scheduler's deferred preemption (`set_resched_hook`); it checks the context itself.
static RESCHED_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Installs what runs when a deferred reschedule (`defer_resched`) meets a point where the count is 0 again.
pub fn set_resched_hook(f: fn()) {
    RESCHED_HOOK.store(f as usize, Ordering::Release);
}

#[cold]
pub(crate) fn run_resched_hook() {
    let f = RESCHED_HOOK.load(Ordering::Acquire);
    if f != 0 {
        // SAFETY: only `set_resched_hook` stores here, a `fn()`.
        let f: fn() = unsafe { core::mem::transmute::<usize, fn()>(f) };
        f();
    }
}

/// An interrupt exit could not preempt (a spin lock held, interrupts off where it hit) although a switch is
/// pending: the next drop of the count to 0, or interrupts turned on with it at 0, asks the scheduler again.
pub fn defer_resched() {
    if !GS_READY.load(Ordering::Acquire) {
        return;
    }
    #[cfg(target_os = "none")]
    // SAFETY: this CPU's own block through GS (see gs_read); one instruction.
    unsafe {
        core::arch::asm!("mov qword ptr gs:[{o}], 1", o = const CL_RESCHED, options(nostack, preserves_flags))
    };
}

/// Whether a reschedule was deferred on this CPU; clears it. A plain read and write: only this CPU and its own
/// interrupts write the flag, and an interrupt between the two that could not preempt sets it again for the
/// caller's next point (the caller looks at the reschedule itself anyway).
pub fn take_deferred_resched() -> bool {
    if !GS_READY.load(Ordering::Acquire) {
        return false;
    }
    #[cfg(target_os = "none")]
    {
        let v = gs_read(CL_RESCHED);
        if v != 0 {
            // SAFETY: this CPU's own block through GS (see gs_read); one instruction.
            unsafe { core::arch::asm!("mov qword ptr gs:[{o}], 0", o = const CL_RESCHED, options(nostack, preserves_flags)) };
        }
        v != 0
    }
    #[cfg(not(target_os = "none"))]
    false
}

/// A point where the thread could be preempted if the count is 0 (interrupts just came back on, a wake-up on this
/// CPU): runs the deferred reschedule. The hook itself checks interrupts and the interrupt context.
#[inline]
pub fn resched_point() {
    if preempt_count() == 0 && resched_deferred() {
        run_resched_hook();
    }
}

/// Whether a reschedule waits for the next preemption point on this CPU (`defer_resched`).
pub fn resched_deferred() -> bool {
    GS_READY.load(Ordering::Acquire) && gs_read(CL_RESCHED) != 0
}

/// Preemption blockers on this CPU.
#[inline]
pub fn preempt_count() -> u64 {
    if !GS_READY.load(Ordering::Acquire) {
        // SAFETY: before GS exists only the BSP runs; a plain load of its own block.
        return unsafe { (*BSP_LOCAL.0.get()).preempt };
    }
    gs_read(CL_PREEMPT)
}

/// Sets the count (the scheduler moves it with the thread across a switch, interrupts off).
pub fn preempt_set(v: u64) {
    if !GS_READY.load(Ordering::Acquire) {
        // SAFETY: before GS exists only the BSP runs, on its own block.
        unsafe { (*BSP_LOCAL.0.get()).preempt = v };
        return;
    }
    #[cfg(target_os = "none")]
    // SAFETY: this CPU's own block through GS (see gs_read); one instruction.
    unsafe {
        core::arch::asm!("mov gs:[{o}], {v}", v = in(reg) v, o = const CL_PREEMPT, options(nostack, preserves_flags))
    };
}

/// Hard-IRQ bookkeeping for `vectors::dispatch` (interrupts are off there).
pub(crate) fn irq_enter() {
    #[cfg(target_os = "none")]
    if GS_READY.load(Ordering::Acquire) {
        // SAFETY: this CPU's own block through GS; one read-modify-write instruction on its own field.
        unsafe { core::arch::asm!("inc qword ptr gs:[{o}]", o = const CL_IRQ_DEPTH, options(nostack)) };
    }
}

pub(crate) fn irq_exit() {
    #[cfg(target_os = "none")]
    if GS_READY.load(Ordering::Acquire) {
        // SAFETY: as irq_enter; always paired with it in `dispatch`.
        unsafe { core::arch::asm!("dec qword ptr gs:[{o}]", o = const CL_IRQ_DEPTH, options(nostack)) };
    }
}

/// Publishes `frame` (a `TrapFrame` address, 0 for none) as the frame of the interrupt this CPU handles now and
/// returns the one before. Only `vectors::dispatch`, around the handler call, with interrupts off.
pub(crate) fn set_irq_frame(frame: u64) -> u64 {
    #[cfg(target_os = "none")]
    if GS_READY.load(Ordering::Acquire) {
        let old = gs_read(CL_IRQ_FRAME);
        // SAFETY: this CPU's own block through GS; one store to its own field.
        unsafe { core::arch::asm!("mov qword ptr gs:[{o}], {v}", o = const CL_IRQ_FRAME, v = in(reg) frame, options(nostack, preserves_flags)) };
        return old;
    }
    let _ = frame;
    0
}

/// The frame `set_irq_frame` published on this CPU (0: not inside a handler of `dispatch`).
pub(crate) fn irq_frame() -> u64 {
    if GS_READY.load(Ordering::Acquire) { gs_read(CL_IRQ_FRAME) } else { 0 }
}

/// True while this CPU runs a hard-IRQ handler (`vectors::dispatch`); for lock checkers and wake-up rules.
pub fn in_hard_irq() -> bool {
    GS_READY.load(Ordering::Acquire) && gs_read(CL_IRQ_DEPTH) != 0
}

// ---------------------------------------------------------------------------------------------------------
// PerCpu<T>

/// CPUs [first, first + items.len()).
struct Chunk<T> {
    first: usize,
    items: Box<[T]>,
    next: AtomicPtr<Chunk<T>>,
}

/// One `T` per CPU, created on first use for every possible CPU. Values never move: a CPU beyond what the
/// container covers gets a new chunk, the old ones stay.
pub struct PerCpu<T> {
    init: fn(usize) -> T,
    head: AtomicPtr<Chunk<T>>,
    covered: AtomicUsize,
    grow: AtomicU8,
}

// SAFETY: chunks are built completely before they are published (Release) and never changed or freed while the
// container lives; afterwards only shared references to the items exist, and `T: Sync` makes sharing those
// across CPUs fine.
unsafe impl<T: Sync + Send> Sync for PerCpu<T> {}

impl<T> PerCpu<T> {
    pub const fn new(init: fn(usize) -> T) -> Self {
        PerCpu { init, head: AtomicPtr::new(core::ptr::null_mut()), covered: AtomicUsize::new(0), grow: AtomicU8::new(0) }
    }

    fn find(&self, cpu: usize) -> Option<&T> {
        let mut p = self.head.load(Ordering::Acquire);
        while !p.is_null() {
            // SAFETY: published chunks are never freed while `self` lives (see the Sync impl).
            let c = unsafe { &*p };
            if cpu >= c.first && cpu - c.first < c.items.len() {
                return Some(&c.items[cpu - c.first]);
            }
            p = c.next.load(Ordering::Acquire);
        }
        None
    }

    /// Makes sure CPUs 0..n have values. The values and their chunk are made first, with interrupts as the
    /// caller has them (a driver's interrupt setup: 32 KiB of MSI slots for 16 CPUs; with interrupts off the heap
    /// may not reclaim, ATOMIC-3); interrupts are off only while the holder of the grow flag links the chunk, so
    /// waiters spin a bounded time. A chunk made for a stale `covered` is dropped and made again.
    fn cover(&self, n: usize) {
        loop {
            let have = self.covered.load(Ordering::Acquire);
            if have >= n {
                return;
            }
            let items: Box<[T]> = (have..n).map(self.init).collect();
            let chunk = Box::into_raw(Box::new(Chunk { first: have, items, next: AtomicPtr::new(core::ptr::null_mut()) }));
            let linked = {
                let _irq = crate::cpu::IrqGuard::new();
                while self.grow.compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
                    core::hint::spin_loop();
                }
                let fits = self.covered.load(Ordering::Acquire) == have;
                if fits {
                    let mut tail = &self.head;
                    loop {
                        let p = tail.load(Ordering::Acquire);
                        if p.is_null() {
                            break;
                        }
                        // SAFETY: as in `find`.
                        tail = unsafe { &(*p).next };
                    }
                    tail.store(chunk, Ordering::Release);
                    self.covered.store(n, Ordering::Release);
                }
                self.grow.store(0, Ordering::Release);
                fits
            };
            if linked {
                return;
            }
            // SAFETY: never published (another grower moved `covered` first), so nobody else can reach it.
            drop(unsafe { Box::from_raw(chunk) });
        }
    }

    fn get_or_grow(&self, cpu: usize) -> &T {
        if let Some(v) = self.find(cpu) {
            return v;
        }
        self.cover(possible_cpus().max(cpu + 1));
        match self.find(cpu) {
            Some(v) => v,
            // `cover` built every index below its target; reaching this is a broken invariant, not input.
            None => panic!("PerCpu: no value for CPU {cpu} after growing"),
        }
    }

    /// This CPU's value.
    pub fn get(&self) -> &T {
        self.get_or_grow(this_cpu())
    }

    /// The value of `cpu`; `None` for a CPU number that is neither possible nor online.
    pub fn get_for(&self, cpu: usize) -> Option<&T> {
        if cpu >= possible_cpus() {
            return None;
        }
        Some(self.get_or_grow(cpu))
    }

    /// The value of `cpu` only if it exists already; never grows (never allocates). For callers inside an
    /// allocator, where growing would re-enter it (an AP's first allocation spun on its own grow flag).
    pub fn find_for(&self, cpu: usize) -> Option<&T> {
        self.find(cpu)
    }

    /// The existing values of the CPUs online now; never grows (see `find_for`).
    pub fn iter_existing(&self) -> impl Iterator<Item = &T> {
        (0..cpu_count()).filter_map(move |c| self.find(c))
    }

    /// Creates the values of every possible CPU now (allocates). Allocators call it from the BSP before the
    /// APs start, so their own `find_for` paths find a slot on every CPU.
    pub fn cover_possible(&self) {
        self.cover(possible_cpus().max(cpu_count()));
    }

    /// The values of the CPUs that are online now (0..cpu_count()).
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        let n = cpu_count();
        (0..n).map(move |c| self.get_or_grow(c))
    }
}

impl<T> Drop for PerCpu<T> {
    fn drop(&mut self) {
        let mut p = *self.head.get_mut();
        while !p.is_null() {
            // SAFETY: `&mut self`: nobody else holds a reference into the chunks; each came from Box::into_raw.
            let c = unsafe { Box::from_raw(p) };
            p = c.next.load(Ordering::Acquire);
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use core::sync::atomic::AtomicU32;

    #[test]
    fn offsets_match_the_assembly() {
        assert_eq!(core::mem::offset_of!(CpuLocal, self_ptr), CL_SELF);
        assert_eq!(core::mem::offset_of!(CpuLocal, cpu_index), CL_CPU_INDEX);
        assert_eq!(core::mem::offset_of!(CpuLocal, current), CL_CURRENT);
        assert_eq!(core::mem::offset_of!(CpuLocal, gdt), CL_GDT);
        assert_eq!(core::mem::offset_of!(CpuLocal, irq_depth), CL_IRQ_DEPTH);
        assert_eq!(core::mem::offset_of!(CpuLocal, preempt), CL_PREEMPT);
        assert_eq!(core::mem::offset_of!(CpuLocal, resched), CL_RESCHED);
        assert_eq!(core::mem::offset_of!(CpuLocal, irq_frame), CL_IRQ_FRAME);
        assert_eq!(core::mem::align_of::<LocalCell>(), 64);
    }

    #[test]
    fn per_cpu_grows_without_moving() {
        let p: PerCpu<AtomicU32> = PerCpu::new(|i| AtomicU32::new(i as u32 + 1));
        assert_eq!(p.get().load(Ordering::Relaxed), 1);
        let first: *const AtomicU32 = p.get();
        assert!(p.get_for(possible_cpus() + 5).is_none(), "beyond the possible CPUs");
        let v = p.get_or_grow(9);
        assert_eq!(v.load(Ordering::Relaxed), 10);
        assert_eq!(first, p.get() as *const AtomicU32, "CPU 0's value did not move");
        for c in 0..10 {
            assert_eq!(p.get_or_grow(c).load(Ordering::Relaxed), c as u32 + 1);
        }
        assert_eq!(p.iter().count(), cpu_count());
    }

    static MADE: AtomicU32 = AtomicU32::new(0);
    static DROPPED: AtomicU32 = AtomicU32::new(0);

    struct Counted(u32);
    impl Drop for Counted {
        fn drop(&mut self) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// ATOMIC-3: values are made before the grow flag (and interrupts off) are taken, so growers race; the one
    /// that finds `covered` unchanged links, the others drop what they made and look again. Every CPU gets one
    /// value that never moves, nothing made is lost or freed twice.
    #[test]
    fn racing_growers_link_one_chunk_each_and_drop_the_rest() {
        let p: PerCpu<Counted> = PerCpu::new(|i| {
            MADE.fetch_add(1, Ordering::Relaxed);
            Counted(i as u32)
        });
        let top = 64usize;
        std::thread::scope(|sc| {
            for t in 0..8 {
                let p = &p;
                sc.spawn(move || {
                    for c in (t % 3..top).step_by(3) {
                        let v = p.get_or_grow(c);
                        assert_eq!(v.0, c as u32);
                    }
                });
            }
        });
        let at: std::vec::Vec<*const Counted> = (0..top).map(|c| p.get_or_grow(c) as *const Counted).collect();
        for (c, &a) in at.iter().enumerate() {
            assert_eq!(p.get_or_grow(c) as *const Counted, a, "CPU {c}'s value did not move");
        }
        let (made, dropped) = (MADE.load(Ordering::Relaxed), DROPPED.load(Ordering::Relaxed));
        let covered = p.covered.load(Ordering::Acquire) as u32;
        assert!(covered >= top as u32);
        assert_eq!(made - dropped, covered, "exactly the linked values live");
        drop(p);
        assert_eq!(MADE.load(Ordering::Relaxed), DROPPED.load(Ordering::Relaxed), "every value made was dropped once");
    }
}
