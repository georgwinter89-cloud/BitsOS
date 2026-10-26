//! Threads and scheduling (owner: SC; contract `C: Thread/spawn`, scheduling classes B4, V2-PLAN §2.6,
//! spec/core.md §9.2).
//!
//! SC-1: real kernel threads. Each has a kernel stack in vmap space with an unmapped guard page below
//! (`stack`, overflow reported by `guard`), its own XSAVE area switched eagerly (`hal::xsave`) and its own CR3,
//! so user processes alternate safely. The current thread of each CPU is in `hal::percpu` (CpuLocal.current,
//! set by `hal::switch`).
//!
//! SC-3: every CPU schedules (R4).
//! - **Per-CPU run queues**, each with its own lock (`cpus`, `rq`). Classes: `KernelRt` by strict priority
//!   (1..=99, FIFO inside a priority, a preempted thread keeps its place at the front); `Interactive`, `Normal`
//!   and `Background` share the CPU fairly by **virtual runtime** with weights (Linux nice -5 / 0 / 10), and an
//!   Interactive thread gets a latency bonus: more sleeper credit on wake-up and it preempts a running Normal
//!   or Background thread at once.
//! - **Wake placement** on CP-4's topology data (`place`): the last CPU if idle, else an idle CPU in the same
//!   cache cluster, a whole idle core before an idle SMT sibling; hybrid CPUs: KernelRt/Interactive prefer P
//!   cores, Background E cores. **Idle stealing** and **periodic pushing** move waiting threads (`balance`).
//! - The **reschedule IPI** (a system vector) wakes halted CPUs; a CPU gives up its thread at **preemption
//!   points**: `yield_now`, `cond_resched` (long loops call it; it switches when `need_resched` is set or the
//!   fair slice of `SLICE_NS` is used up), blocking. Full timer preemption needs CP-5's LAPIC timer: its handler
//!   calls `timer_tick`, its interrupt exit `irq_exit_preempt` (never preempts a spin-lock holder:
//!   `ksync::preempt_count`).
//! - **Wake-ups from interrupt handlers are legal**: `ksync`'s wake paths and the wake hook are IRQ-safe (an
//!   interrupt-time wake-up goes onto the chosen CPU's lock-free wake list; nothing allocates).
//! - Stacks and thread records of exited threads are freed by the **reaper** after every CPU passed a quiescent
//!   point (`reap`): until MM-6's TLB shootdown that keeps thread migration safe.
//! - Idle CPUs halt; one keeps **poll duty** (`idle`).
//! - `run_exclusive` holds every other CPU in a stopper thread while a function runs (consistency checks).
//!
//! SC-4 (`timers`): sleeps and timed waits end through an `HrTimer` (the sleep timer, armed at the earliest
//! deadline); the **tick** is a per-CPU `HrTimer` armed only while a CPU has more than one runnable thread
//! (tickless otherwise); every CPU keeps a **heartbeat** and an **idle-entry record** for RB-3's watchdog
//! (`heartbeat`, `last_idle_entry`, `probe_cpu`). Until CP-5's LAPIC one-shot timer, `time` polls its hrtimers
//! (the poll-duty CPU, the poll thread), so the duty CPU still spins.
//!
//! SC-6 (`attr`, `cputime`): programs' nice values and policies (SCHED_OTHER/BATCH/IDLE) set each fair
//! thread's weight; affinity masks limit where a thread runs (placement, balancing and requeues honour them);
//! CPU time is split into user and system time at kernel entry and exit, readable with the running slice
//! (`Thread::cpu_times`), with voluntary/involuntary switch counts and per-CPU user/nice time.
//!
//! Interrupts: new threads start with IF=1 once `acpi::irq::enabled()` (else IF=0 like the boot thread); every
//! thread keeps its own IF across switches. A thread that first ran before interrupts were on keeps IF=0 until
//! it calls `catch_up_interrupts()` at a point where it holds no `IrqGuard` (idle threads do so every round).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod attr;
mod attr_ktests;
mod balance;
mod cpus;
pub mod cputime;
pub mod futex;
mod guard;
mod idle;
pub mod interactive;
mod interactive_ktests;
mod ktests;
mod lock_ktests;
pub mod pi;
mod pi_ktests;
pub mod place;
mod reap;
pub mod rq;
mod run;
mod sleepers;
mod smp_ktests;
mod state_ktests;
pub mod stack;
mod stop;
mod table;
pub mod timers;
mod timer_ktests;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use hal::switch::{KernelContext, StackBounds, Switchable};
use hal::xsave::FpuState;
use ksync::{Completion, SchedHooks, SpinLock, ThreadRef};

pub use attr::{AttrError, Policy};
pub use cputime::CpuTimes;
pub use idle::duty_cpu;
pub use reap::reap;
pub use stop::{ExclusiveError, run_exclusive};
pub use timers::{IdleEntry, heartbeat, last_idle_entry, probe_cpu, tick_armed};

/// Scheduling class (B4). KernelRt always runs before the others (strict priority); Interactive, Normal and
/// Background share the CPU by weight (none starves). A thread never raises its own class beyond what policy
/// granted (SC-7). Rule: KernelRt/Interactive threads never wait for user space or a disk without a deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Class {
    /// MCP, input, network receive: the lifeline and the pointer.
    KernelRt = 0,
    /// Compositor, pointer path, the program in front.
    Interactive = 1,
    Normal = 2,
    Background = 3,
}

impl Class {
    fn from_u8(v: u8) -> Class {
        match v {
            0 => Class::KernelRt,
            1 => Class::Interactive,
            2 => Class::Normal,
            _ => Class::Background,
        }
    }
    /// Weight in the fair class (Linux's nice table: -5 = 3121, 0 = 1024, 10 = 110); KernelRt has none.
    pub fn weight(self) -> u64 {
        match self {
            Class::KernelRt | Class::Normal => NICE0_WEIGHT,
            Class::Interactive => 3121,
            Class::Background => 110,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Class::KernelRt => "kernel_rt",
            Class::Interactive => "interactive",
            Class::Normal => "normal",
            Class::Background => "background",
        }
    }
}

const NICE0_WEIGHT: u64 = 1024;
/// A fair thread runs at most this long while others wait on its CPU (spec §9.2: ~10 ms like V1).
pub const SLICE_NS: u64 = 10_000_000;
/// A woken fair thread preempts the running one when its virtual runtime is this much behind.
pub const WAKEUP_GRAN_NS: u64 = 1_000_000;
/// How often a busy CPU looks for an idle one to give work to.
pub const BALANCE_NS: u64 = 4_000_000;
/// Minimum distance between two housekeeping rounds of one busy CPU.
pub const HOUSEKEEP_NS: u64 = 1_000_000;
/// The poll thread's period while no CPU is idle.
pub const POLL_NS: u64 = 1_000_000;
/// KernelRt priorities: 1 (lowest) ..= 99; spawn's default.
pub const RT_PRIO_DEFAULT: u8 = 50;
pub const RT_PRIO_MAX: u8 = 99;
/// "No CPU" for pins and last-CPU fields.
pub const NO_CPU: usize = usize::MAX;

/// Virtual runtime for `ns` of CPU time at fair weight `weight` (SC-6: from class, policy and nice).
fn vdelta(ns: u64, weight: u64) -> u64 {
    ns.saturating_mul(NICE0_WEIGHT) / weight.max(1)
}

/// How far before the CPU's virtual time a waking thread may be placed (Interactive: the latency bonus).
fn sleeper_credit(class: Class) -> u64 {
    match class {
        Class::Interactive => SLICE_NS,
        _ => SLICE_NS / 2,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum State {
    Runnable = 0,
    Running = 1,
    Blocking = 2,
    Blocked = 3,
    Dead = 4,
}

pub struct Thread {
    pub id: u64,
    pub name: String,
    /// The class it was spawned with (KernelRt and Background stay; a Normal user thread is raised to
    /// Interactive while its group holds a grant, SC-7a, `interactive`).
    base: Class,
    /// The class it runs in now (`Class` as u8; changes only among the fair classes, under `attr::WRITERS`).
    class: AtomicU8,
    /// SC-7b: its class without priority passing (`effective_class`, written with `class`).
    own: AtomicU8,
    /// SC-7b: the class lent to it by waiters of locks it holds (`pi::NO_BOOST`: none; written under `pi`'s lock).
    boost: AtomicU8,
    /// SC-7b fixer: its boost changed but no memory was left to note it for a refresh (`pi::Live::apply`).
    pi_stale: AtomicBool,
    /// SC-7a: its thread group (the process's tgid, `SpawnOpts::group`; 0 = a kernel thread, never granted).
    group: AtomicU64,
    /// SC-7a: the SCHED_FIFO/SCHED_RR priority a program set (1..=99; 0 under a fair policy).
    user_rt: AtomicU8,
    /// SC-7a: when its last wake-up made it runnable (ns; 0 = not waiting since a wake-up).
    woken_at: AtomicU64,
    /// SC-7a: its longest wake-to-run wait while Interactive (ns).
    wait_max: AtomicU64,
    /// SC-7a: it has passed `interactive::thread_exits` (no longer counts as a live thread of its group).
    left: AtomicBool,
    state: AtomicU8,
    /// Started with IF=0 because interrupts were not on yet (`catch_up_interrupts`).
    irq_late: AtomicBool,
    /// The CPU this is the idle thread of (`NO_CPU`: a normal thread).
    idle_of: usize,
    rt_prio: AtomicU8,
    /// The only CPU it may run on (`NO_CPU`: any).
    pin: AtomicUsize,
    /// The CPU it runs or last ran (or is queued) on.
    cpu: AtomicUsize,
    vruntime: AtomicU64,
    /// Virtual runtime relative to its CPU's floor, kept while it is in no queue (asleep, moving).
    lag: AtomicI64,
    /// Start of the running slice (0 while it does not run). SC-6: written inside `acct_seq` (`cputime`).
    exec_start: AtomicU64,
    runtime_ns: AtomicU64,
    /// SC-6: the part of `runtime_ns` spent in user mode, whether it is in user mode now (by the process loop's
    /// marks), and the sequence counter readers on other CPUs use (odd while the owner writes).
    utime_ns: AtomicU64,
    in_user: AtomicBool,
    acct_seq: AtomicU64,
    /// The largest user/system times handed out so far: reads never go backwards (INT-W3; Linux's
    /// cputime_adjust keeps `prev_cputime` for the same reason).
    seen_user: AtomicU64,
    seen_sys: AtomicU64,
    /// SC-6: switches because it blocked / because it was preempted or yielded (Linux nvcsw/nivcsw).
    nvcsw: AtomicU64,
    nivcsw: AtomicU64,
    /// SC-6: nice value (-20..19), policy (`attr::Policy` as u8), reset-on-fork, and the fair weight they give.
    nice: core::sync::atomic::AtomicI32,
    policy: AtomicU8,
    reset_on_fork: AtomicBool,
    weight: AtomicU64,
    /// SC-6: affinity mask, one bit per possible CPU (`affine_all`: every CPU, the words are not looked at).
    affinity: Box<[AtomicU64]>,
    affine_all: AtomicBool,
    migrations: AtomicU64,
    wakeups: AtomicU64,
    /// Link of a CPU's interrupt-time wake list.
    wake_next: AtomicU64,
    /// The deadline of the current sleep (0: none).
    sleep_deadline: AtomicU64,
    ctx: KernelContext,
    entry: SpinLock<Option<Box<dyn FnOnce() + Send>>>,
    done: Completion,
    /// Futex wait state (SC-5).
    futex: futex::ThreadState,
}

impl Thread {
    fn new(id: u64, name: String, class: Class, ctx: KernelContext, entry: Option<Box<dyn FnOnce() + Send>>, idle_of: usize) -> Thread {
        Thread {
            id,
            name,
            base: class,
            class: AtomicU8::new(class as u8),
            own: AtomicU8::new(class as u8),
            boost: AtomicU8::new(pi::NO_BOOST),
            pi_stale: AtomicBool::new(false),
            group: AtomicU64::new(0),
            user_rt: AtomicU8::new(0),
            woken_at: AtomicU64::new(0),
            wait_max: AtomicU64::new(0),
            left: AtomicBool::new(false),
            state: AtomicU8::new(State::Runnable as u8),
            irq_late: AtomicBool::new(false),
            idle_of,
            rt_prio: AtomicU8::new(RT_PRIO_DEFAULT),
            pin: AtomicUsize::new(NO_CPU),
            cpu: AtomicUsize::new(NO_CPU),
            vruntime: AtomicU64::new(0),
            lag: AtomicI64::new(0),
            exec_start: AtomicU64::new(0),
            runtime_ns: AtomicU64::new(0),
            utime_ns: AtomicU64::new(0),
            in_user: AtomicBool::new(false),
            acct_seq: AtomicU64::new(0),
            seen_user: AtomicU64::new(0),
            seen_sys: AtomicU64::new(0),
            nvcsw: AtomicU64::new(0),
            nivcsw: AtomicU64::new(0),
            nice: core::sync::atomic::AtomicI32::new(0),
            policy: AtomicU8::new(attr::Policy::Other as u8),
            reset_on_fork: AtomicBool::new(false),
            weight: AtomicU64::new(attr::weight_of(class, attr::Policy::Other, 0)),
            affinity: attr::new_mask(),
            affine_all: AtomicBool::new(true),
            migrations: AtomicU64::new(0),
            wakeups: AtomicU64::new(0),
            wake_next: AtomicU64::new(0),
            sleep_deadline: AtomicU64::new(0),
            ctx,
            entry: SpinLock::new(entry),
            done: Completion::new(),
            futex: futex::ThreadState::new(),
        }
    }
    fn state(&self) -> State {
        match self.state.load(Ordering::Acquire) {
            0 => State::Runnable,
            1 => State::Running,
            2 => State::Blocking,
            3 => State::Blocked,
            _ => State::Dead,
        }
    }
    fn set_state(&self, s: State) {
        self.state.store(s as u8, Ordering::Release);
    }
    fn cas_state(&self, from: State, to: State) -> bool {
        self.state.compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }
    fn is_idle(&self) -> bool {
        self.idle_of != NO_CPU
    }
    fn rt_prio(&self) -> u8 {
        self.rt_prio.load(Ordering::Relaxed)
    }
    /// The class it runs in now (SC-7a: may change among Interactive, Normal and Background).
    pub fn class(&self) -> Class {
        Class::from_u8(self.class.load(Ordering::Relaxed))
    }
    /// The class it was spawned with.
    pub fn base_class(&self) -> Class {
        self.base
    }
    /// Its thread group (the tgid for a user thread, 0 for a kernel thread).
    pub fn group(&self) -> u64 {
        self.group.load(Ordering::Relaxed)
    }
    /// Its longest wake-to-run wait while Interactive (ns, SC-7a).
    pub fn interactive_wait_max(&self) -> u64 {
        self.wait_max.load(Ordering::Relaxed)
    }
    /// Whether it still counts as a live thread of its group (it has not started to exit; SC-7a).
    pub fn in_group(&self) -> bool {
        !self.left.load(Ordering::SeqCst) && self.state() != State::Dead
    }
    /// Where this thread's kernel stack is (zeros for the boot thread).
    pub fn stack_bounds(&self) -> StackBounds {
        self.ctx.stack_bounds()
    }
    /// The saved user FPU/SIMD state (valid while the thread does not run).
    pub fn fpu(&self) -> Option<&FpuState> {
        self.ctx.xstate()
    }
    /// CPU time used so far, the running slice included (SC-6; AB-3 change request 6).
    pub fn runtime_ns(&self) -> u64 {
        self.cpu_times().total_ns()
    }
    /// The CPU it runs or last ran on (`NO_CPU` before its first run).
    pub fn last_cpu(&self) -> usize {
        self.cpu.load(Ordering::Relaxed)
    }
    pub fn migrations(&self) -> u64 {
        self.migrations.load(Ordering::Relaxed)
    }
    pub fn pinned(&self) -> Option<usize> {
        let p = self.pin.load(Ordering::Relaxed);
        (p != NO_CPU).then_some(p)
    }
    pub fn rt_priority(&self) -> u8 {
        self.rt_prio()
    }
}

impl Switchable for Thread {
    fn context(&self) -> &KernelContext {
        &self.ctx
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// Why the running thread calls the scheduler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Leave {
    /// Still runnable: back into its run queue.
    Yield,
    /// As Yield, but asked to by `need_resched` (a KernelRt thread keeps its place in line).
    Preempt,
    /// Sleeps until woken or its deadline; a wake since `prepare_to_block` cancels it.
    Block,
    /// The idle thread found work.
    Idle,
    /// Never runs again.
    Exit,
    /// As Yield, but switches to the idle thread if nothing else waits (moves a thread to its new pin).
    Move,
}

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU64 = AtomicU64::new(2);
/// The thread id `sc.sched.rollback` refuses on purpose (0: none): its switch failure is logged at Debug.
pub(crate) static EXPECTED_SWITCH_FAIL: AtomicU64 = AtomicU64::new(0);
/// Threads that called `exit` and have not switched away for the last time.
static EXITING: AtomicUsize = AtomicUsize::new(0);
static ONLINE: AtomicUsize = AtomicUsize::new(0);

klog::counter!(SPAWNED, "sched.threads_spawned");
klog::counter!(EXITED, "sched.threads_exited");
klog::counter!(SPAWN_FAILED, "sched.spawn_failed");
klog::counter!(LOST_WAKES, "sched.lost_wakes");

/// Default kernel stack: 64 KiB (2^4 pages) above one unmapped guard page.
pub const STACK_ORDER: u32 = 4;

/// Turns the running boot code into thread 1 ("boot"), creates CPU 0's idle thread, the reaper and the poll
/// thread, registers the reschedule IPI and installs the hooks. The APs join through `sched.smp` (initcall).
pub fn init() {
    if INITIALIZED.swap(true, Ordering::AcqRel) {
        return;
    }
    let x = hal::xsave::init_this_cpu();
    // bitsboot's tables with the populated kernel half: what kernel threads run with (hal resolves
    // `KERNEL_CR3` at every switch, so MM can replace the root later through `hal::switch::set_kernel_root`).
    hal::switch::set_kernel_root(hal::cpu::read_cr3());
    // First use of the per-CPU state, in thread context: covers every possible CPU (no allocation later).
    let cs = cpus::this();
    let now = time::now_ns();
    let boot = Arc::new(Thread::new(1, String::from("boot"), Class::Normal, with_lock_state(KernelContext::adopt_running(1, FpuState::initial())), None, NO_CPU));
    boot.set_state(State::Running);
    boot.cpu.store(cs.cpu, Ordering::Relaxed);
    // The boot thread stays on the BSP: the start code and the ktest runner (which it also is) assume CPU 0.
    boot.pin.store(cs.cpu, Ordering::Relaxed);
    boot.set_exec_start(now);
    // As `spawn` (it exits after start); without the room `bury` and `sleepers::add` fall back safely.
    let _ = (reap::reserve_grave(), sleepers::reserve());
    if table::insert(boot.clone()).is_err() {
        klog::event!(Error, "sched", "init.failed", missing = "thread table"; "no scheduler");
        return;
    }
    cs.set_curr(&boot, now);
    hal::switch::init_this_cpu(boot);
    ksync::lockdep::prepare_irq_context();
    let vector = cpus::register_resched();
    timers::init(cs);
    match spawn_inner("idle/0", Class::Background, SpawnOpts::default(), 0, Box::new(|| {
        idle::idle_loop(0);
    }), Some(cs.cpu)) {
        Ok(h) => {
            let _ = cs.idle.get_or_init(|| h.thread.clone());
        }
        Err(e) => klog::event!(Error, "sched", "idle.failed", cpu = 0, error = alloc::format!("{e:?}"); "no idle thread: a thread that blocks with nothing else runnable spins"),
    }
    cs.online.store(true, Ordering::Release);
    ONLINE.fetch_add(1, Ordering::AcqRel);
    guard::install();
    ksync::set_tsc_hz(time::tsc_hz());
    ksync::set_sched_hooks(&HOOKS);
    STARTED.store(true, Ordering::Release);
    for (name, class, f) in [("reaper", Class::Normal, reap::reaper_loop as fn()), ("poll", Class::KernelRt, idle::poll_loop as fn())] {
        if let Err(e) = spawn(name, class, f) {
            klog::event!(Error, "sched", "service.failed", name = name, error = alloc::format!("{e:?}"));
        }
    }
    klog::event!(
        Info,
        "sched",
        "ready",
        mode = "sc3-smp",
        lock_checker = ksync::lockdep::enabled(),
        resched_vector = klog::Hex(vector.unwrap_or(0) as u64),
        slice_ms = SLICE_NS / 1_000_000,
        xsave = x.mode.name(),
        xsave_bytes = x.size,
        xsave_cpuid_bytes = x.cpuid_size,
        xsave_computed_bytes = x.computed_size,
        xcr0 = klog::Hex(x.mask),
        stack_kib = stack::STACK_PAGES * 4,
        guard_pages = stack::GUARD_PAGES,
        irq_on_start = acpi::irq::enabled()
    );
    if vector.is_none() {
        klog::event!(Error, "sched", "resched.no_vector"; "no free system vector: halted CPUs wake only by other interrupts");
    }
    if x.cpuid_size != x.computed_size {
        klog::event!(Warn, "sched", "xsave.size_mismatch", cpuid = x.cpuid_size, computed = x.computed_size; "the larger size is used");
    }
}

// The APs: after CP-4's `cpu.smp` (same level, later name), every AP gets an idle thread and schedules.
kreg::initcall!(kreg::Level::Core, "sched.smp", smp_start);

fn smp_start() -> Result<(), &'static str> {
    if !STARTED.load(Ordering::Acquire) {
        return Err("scheduler not started");
    }
    if let Some(t) = hal::smp::topology(0) {
        let _ = cpus::this().topo.get_or_init(|| t);
    }
    // RB-5b: a Reload parks the APs from an interrupt; a CPU that holds a ksync spin lock (or a PreemptGuard)
    // says "not now" and gets the park interrupt again later.
    hal::kexec_stub::set_park_guard(park_allowed);
    let woken = hal::smp::hand_over_aps(ap_entry);
    klog::event!(Info, "sched", "smp.handover", aps = woken.unwrap_or(0), cpus = hal::percpu::cpu_count());
    Ok(())
}

/// The Reload park guard (inside the park interrupt; no allocation, no lock).
fn park_allowed() -> bool {
    ksync::preempt_count() == 0
}

/// An AP's entry from CP-4 (its boot stack, interrupts off): the running code becomes its idle thread.
fn ap_entry(cpu: usize) -> ! {
    // CP-4 did this already; repeating it is harmless and keeps IA32_XSS right should that ever change.
    hal::xsave::init_this_cpu();
    // MM-6: kernel shootdowns ask only online CPUs; catch up with any sent while this CPU was starting.
    hal::paging::tlb::poll();
    let (guard, bottom, top) = hal::smp::boot_stack(cpu).unwrap_or((0, 0, 0));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let ctx = with_lock_state(KernelContext::adopt_running_on(id, None, StackBounds { guard, bottom, top }));
    let t = Arc::new(Thread::new(id, alloc::format!("idle/{cpu}"), Class::Background, ctx, None, cpu));
    let now = time::now_ns();
    t.set_state(State::Running);
    t.cpu.store(cpu, Ordering::Relaxed);
    t.set_exec_start(now);
    let Some(cs) = cpus::of(cpu) else { park(cpu, "no per-CPU state") };
    timers::init_cpu(cs);
    if let Some(topo) = hal::smp::topology(cpu) {
        let _ = cs.topo.get_or_init(|| topo);
    }
    if table::insert(t.clone()).is_err() {
        park(cpu, "no memory for the thread table");
    }
    let _ = cs.idle.get_or_init(|| t.clone());
    cs.set_curr(&t, now);
    if !hal::switch::init_this_cpu(t) {
        park(cpu, "hal refused the idle context");
    }
    ksync::lockdep::prepare_irq_context();
    cs.online.store(true, Ordering::Release);
    let n = ONLINE.fetch_add(1, Ordering::AcqRel) + 1;
    let topo = cs.topo.get().copied().unwrap_or_default();
    klog::event!(Info, "sched", "cpu.join", cpu = cpu, apic_id = topo.apic_id, core = topo.core_id, thread = topo.thread_id,
        cluster = topo.cluster_id, core_type = topo.core_type, online = n);
    hal::cpu::enable_interrupts();
    idle::idle_loop(cpu)
}

/// An AP that cannot join stays halted with interrupts on (it answers IPIs, runs nothing).
fn park(cpu: usize, why: &str) -> ! {
    klog::event!(Error, "sched", "cpu.join_failed", cpu = cpu, reason = why; "this CPU stays halted");
    loop {
        hal::cpu::enable_and_halt();
    }
}

/// CPUs that schedule threads now.
pub fn online_cpus() -> usize {
    ONLINE.load(Ordering::Acquire)
}

/// Waits (sleeping, bounded) until every CPU that is up has joined the scheduler. Returns the online count.
pub fn wait_cpus_online(timeout_ns: u64) -> usize {
    let t0 = time::now_ns();
    while online_cpus() < hal::percpu::cpu_count() && time::now_ns() - t0 < timeout_ns {
        sleep_ms(1);
    }
    online_cpus()
}

/// Gives a context ksync's per-thread state (the held-lock list of the order checker; debug builds only).
fn with_lock_state(ctx: KernelContext) -> KernelContext {
    match ksync::lockdep::new_thread_state() {
        Some(st) => ctx.with_ext(st),
        None => ctx,
    }
}

/// Emits a `ksync lock.violation` event for every lock-order violation not reported yet (the CPU on poll duty
/// does this; ktests call it directly). Needs no lock held by the caller.
pub fn report_lock_violations() {
    ksync::lockdep::report(|v| {
        // Only test-injected ones: Info (they are expected); any other: Error (a bug to fix).
        if v.injected >= v.count {
            klog::event!(Info, "ksync", "lock.violation", kind = v.kind.name(), held = v.held, taking = v.taking,
                count = v.count, new = v.new, injected = v.injected, thread = v.first_thread; "injected by a test");
        } else {
            klog::event!(Error, "ksync", "lock.violation", kind = v.kind.name(), held = v.held,
                held_level = v.held_level, taking = v.taking, taking_level = v.taking_level, count = v.count,
                new = v.new, injected = v.injected, thread = v.first_thread; "lock order broken (CONTRACTS.md R5, ksync order.rs)");
        }
    });
    // The checker's fallbacks: acquisitions it could not track, violations without their own record.
    static SEEN: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
    let c = ksync::lockdep::counts();
    let untracked = SEEN[0].swap(c.untracked, Ordering::Relaxed);
    if c.untracked > untracked {
        klog::event!(Warn, "ksync", "lock.untracked", total = c.untracked, new = c.untracked - untracked; "held list could not grow under an allocator lock; those acquisitions were not checked");
    }
    let unrecorded = SEEN[1].swap(c.unrecorded, Ordering::Relaxed);
    if c.unrecorded > unrecorded {
        klog::event!(Warn, "ksync", "lock.unrecorded", total = c.unrecorded, new = c.unrecorded - unrecorded; "violations counted without a record (no memory at that moment)");
    }
}

pub struct JoinHandle {
    thread: Arc<Thread>,
}

impl JoinHandle {
    pub fn id(&self) -> u64 {
        self.thread.id
    }
    /// Waits until the thread has returned from its function.
    pub fn join(&self) {
        self.thread.done.wait();
    }
    pub fn join_timeout(&self, ns: u64) -> bool {
        self.thread.done.wait_timeout(ns)
    }
    pub fn is_finished(&self) -> bool {
        self.thread.done.is_done()
    }
    pub fn thread(&self) -> &Arc<Thread> {
        &self.thread
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnError {
    NoMemory,
    NotStarted,
}

/// Extra choices for `spawn_with`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnOpts {
    /// Where it runs first (None: wake placement). Without `pin` balancing may move it later.
    pub cpu: Option<usize>,
    /// Keep it on `cpu` for good (tests, per-CPU workers; SC-6 brings affinity for programs).
    pub pin: bool,
    /// KernelRt priority 1..=99 (ignored for the other classes).
    pub rt_prio: u8,
}

impl Default for SpawnOpts {
    fn default() -> Self {
        SpawnOpts { cpu: None, pin: false, rt_prio: RT_PRIO_DEFAULT }
    }
}

/// Starts a kernel thread.
pub fn spawn(name: &str, class: Class, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle, SpawnError> {
    spawn_inner(name, class, SpawnOpts::default(), 0, Box::new(f), None)
}

/// SC-7a: starts a thread of thread group `group` (a user thread: its process's tgid), so a grant to the
/// process (`interactive`) reaches it from its first instruction.
pub fn spawn_in_group(name: &str, class: Class, group: u64, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle, SpawnError> {
    spawn_inner(name, class, SpawnOpts::default(), group, Box::new(f), None)
}

/// Starts a kernel thread with a first CPU, a pin or a KernelRt priority.
pub fn spawn_with(name: &str, class: Class, opts: SpawnOpts, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle, SpawnError> {
    spawn_inner(name, class, opts, 0, Box::new(f), None)
}

/// `idle_of`: Some(cpu) makes the idle thread of that CPU (no XSAVE area, never queued).
fn spawn_inner(name: &str, class: Class, opts: SpawnOpts, group: u64, f: Box<dyn FnOnce() + Send>, idle_of: Option<usize>) -> Result<JoinHandle, SpawnError> {
    if !INITIALIZED.load(Ordering::Acquire) {
        return Err(SpawnError::NotStarted);
    }
    let fail = |what: &str| {
        SPAWN_FAILED.inc();
        klog::event!(Warn, "sched", "spawn.failed", name = name, missing = what);
        SpawnError::NoMemory
    };
    let stack = stack::alloc().map_err(|e| {
        klog::event!(Warn, "sched", "stack.alloc_failed", name = name, error = alloc::format!("{e:?}"));
        fail("stack")
    })?;
    // On failure below, `stack` drops here and is retired.
    // Idle threads never run user code, so they need no XSAVE area (switching to or from them saves nothing).
    let xstate = if idle_of.is_some() { None } else { Some(FpuState::initial().ok_or_else(|| fail("xsave area"))?) };
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let ctx = with_lock_state(KernelContext::new_thread(id, stack, thread_start, id as usize, xstate, hal::switch::KERNEL_CR3));
    let t = Arc::new(Thread::new(id, String::from(name), class, ctx, Some(f), idle_of.unwrap_or(NO_CPU)));
    t.rt_prio.store(opts.rt_prio.clamp(1, RT_PRIO_MAX), Ordering::Relaxed);
    if opts.pin
        && let Some(c) = opts.cpu
    {
        t.pin.store(c, Ordering::Relaxed);
    }
    if idle_of.is_some() {
        t.cpu.store(idle_of.unwrap_or(0), Ordering::Relaxed);
    }
    t.group.store(group, Ordering::Relaxed);
    // Room for its record in the graveyard and its deadline among the sleepers, so exiting and sleeping never
    // allocate on the scheduler's paths.
    if reap::reserve_grave().is_err() || sleepers::reserve().is_err() {
        return Err(fail("scheduler room"));
    }
    if table::insert(t.clone()).is_err() {
        return Err(fail("thread table"));
    }
    // SC-7a: after the insert, so a grant made meanwhile either sees it in the table or is seen here.
    if group != 0 {
        attr::refresh(&t);
    }
    SPAWNED.inc();
    klog::event!(Debug, "sched", "thread.spawn", id = id, name = name, class = t.class().name(), group = group);
    if idle_of.is_none() {
        let cpu = match opts.cpu {
            Some(c) if cpus::of(c).is_some_and(|s| s.online.load(Ordering::Acquire)) => c,
            _ => cpus::select_for(&t),
        };
        run::enqueue(t.clone(), cpu, run::Why::Migrate);
    }
    Ok(JoinHandle { thread: t })
}

extern "C" fn thread_start(_id: usize) -> ! {
    // hal's start trampoline has finished the switch; interrupts are still off.
    run::after_switch();
    let on = acpi::irq::enabled();
    let t = current_thread();
    if on {
        hal::cpu::enable_interrupts();
    } else if let Some(t) = &t {
        t.irq_late.store(true, Ordering::Release);
    }
    let f = t.and_then(|t| t.entry.lock().take());
    if let Some(f) = f {
        f();
    }
    exit()
}

/// Ends the calling thread.
pub fn exit() -> ! {
    // Nothing that owns memory may stay on this stack: it is abandoned without unwinding.
    {
        if let Some(t) = current_thread() {
            EXITED.inc();
            klog::event!(Debug, "sched", "thread.exit", id = t.id, name = t.name.as_str());
            pi::thread_exits(&t);
            interactive::thread_exits(&t);
            EXITING.fetch_add(1, Ordering::AcqRel);
            t.done.complete();
        }
    }
    if !STARTED.load(Ordering::Acquire) {
        // The boot code before `init` (or a kernel without a scheduler): nothing to switch to.
        hal::panic::halt_forever()
    }
    let _irq = hal::cpu::IrqGuard::new();
    let mut tries = 0u64;
    loop {
        // A refused switch was rolled back (the thread is still current and listed): try again.
        match run::schedule(Leave::Exit) {
            Ok(true) => break,
            Ok(false) | Err(_) => {
                tries += 1;
                if tries == 1 {
                    klog::event!(Warn, "sched", "exit.retry", id = current_id(); "no switch possible yet, retrying");
                }
                hal::cpu::spin_hint();
            }
        }
    }
    // A dead thread is never switched back to.
    hal::panic::halt_forever()
}

/// Turns interrupts on for a thread that started before they were on (it began with IF=0). Call only where the
/// thread holds no `IrqGuard` of its own (e.g. the top of a service loop); no-op otherwise or once done.
pub fn catch_up_interrupts() {
    if !acpi::irq::enabled() {
        return;
    }
    if let Some(t) = current_thread()
        && t.irq_late.swap(false, Ordering::AcqRel)
    {
        hal::cpu::enable_interrupts();
        klog::event!(Debug, "sched", "irq.catch_up", id = t.id, name = t.name.as_str());
    }
}

/// The calling thread's id (0 before `init`). Lock-free (CpuLocal.current).
pub fn current_id() -> u64 {
    hal::percpu::current()
}

/// Makes the CPU that is running thread `id` interrupt it soon (Linux `kick_process`, kernel/signal.c
/// `signal_wake_up_state`): `need_resched` and a reschedule IPI to that CPU. A thread in user mode then returns
/// through the kernel at once (the user-IRQ hook sees a kill). MC-5a change request 1 (INT-A1): `proc`'s
/// `Process::kill` calls it. SC-5: a thread that sleeps (or is on its way to sleep) is woken as well, like
/// Linux's `signal_wake_up` waking a TASK_INTERRUPTIBLE task: its wait sees a spurious wake-up, re-checks and
/// finds the kill (futex waits ask `Space::interrupted`; every other wait loops on its condition). The full
/// fence pairs with the waiter's fence between "I sleep now" and its last look at the kill flag, so either the
/// waiter sees the flag the caller set before this call or this call sees the waiter asleep. IRQ-safe (atomics,
/// the thread table's IRQ lock, the interrupt-time wake list, an IPI; no allocation); true when the thread ran
/// or was woken.
pub fn kick_thread(id: u64) -> bool {
    if id == 0 {
        return false;
    }
    core::sync::atomic::fence(Ordering::SeqCst);
    let woken = table::get(id).is_some_and(|t| run::wake_thread(&t, hal::percpu::in_hard_irq()));
    match cpus::online().find(|c| c.curr_id.load(Ordering::Acquire) == id) {
        Some(cs) => {
            cpus::kick(cs, true);
            true
        }
        None => woken,
    }
}

/// The calling thread (lock-free: hal's record of what runs on this CPU).
pub fn current_thread() -> Option<Arc<Thread>> {
    hal::switch::current_owner()?.into_any().downcast::<Thread>().ok()
}

/// The CPU the caller runs on (it may move right after, unless pinned or holding a `PreemptGuard`).
pub fn current_cpu() -> usize {
    hal::percpu::this_cpu()
}

/// Lets other runnable threads run (a KernelRt thread only gives way to equal or higher priorities).
pub fn yield_now() {
    if !STARTED.load(Ordering::Acquire) {
        return;
    }
    if ksync::preempt_count() == 0 {
        idle::housekeeping(cpus::this(), time::now_ns());
    }
    let _irq = hal::cpu::IrqGuard::new();
    // A refused switch was rolled back: the caller just runs on (a yield is a hint).
    let _ = run::schedule(Leave::Yield);
}

/// Whether the running thread should give the CPU up soon (a reschedule was asked for, or its fair slice is used
/// up while others wait).
pub fn need_resched() -> bool {
    if !STARTED.load(Ordering::Acquire) {
        return false;
    }
    let cs = cpus::this();
    cs.need_resched.load(Ordering::Relaxed) || cs.slice_expired(time::now_ns())
}

/// A voluntary preemption point for long loops in kernel code: does the scheduler's housekeeping now and then
/// and switches if `need_resched()`. Returns whether it switched. Call with no spin lock held (it does nothing
/// then) and never from an interrupt handler.
pub fn cond_resched() -> bool {
    if !STARTED.load(Ordering::Acquire) || hal::percpu::in_hard_irq() || ksync::preempt_count() > 0 {
        return false;
    }
    let cs = cpus::this();
    let now = time::now_ns();
    idle::housekeeping(cs, now);
    if !(cs.need_resched.load(Ordering::Relaxed) || cs.slice_expired(now) || cs.wake_head.load(Ordering::Relaxed) != 0) {
        return false;
    }
    let _irq = hal::cpu::IrqGuard::new();
    matches!(run::schedule(Leave::Preempt), Ok(true))
}

/// For CP-5's LAPIC timer handler (hard-IRQ context, any CPU): charges the running thread and asks for a
/// reschedule when its slice is used up or a sleeper's deadline passed. Allocation- and lock-free.
pub fn timer_tick() {
    if !STARTED.load(Ordering::Acquire) {
        return;
    }
    let Some(cs) = cpus::of(hal::percpu::this_cpu()) else { return };
    cs.beat();
    let now = time::now_ns();
    if cs.slice_expired(now) || (sleepers::next() <= now && !cs.in_idle.load(Ordering::Relaxed)) {
        cs.need_resched.store(true, Ordering::Release);
    }
}

/// For CP-5's interrupt exit (hard-IRQ context, after the EOI, on the interrupted thread's stack, interrupts
/// off): preempts the interrupted thread when a reschedule is pending and it may be preempted (it had
/// interrupts on, holds no spin lock, is not in a nested handler). Returns whether it switched (this returns
/// when the thread runs again, possibly on another CPU).
pub fn irq_exit_preempt(interrupted_if: bool) -> bool {
    if !STARTED.load(Ordering::Acquire) || !interrupted_if || hal::percpu::in_hard_irq() || ksync::preempt_count() > 0 {
        return false;
    }
    let cs = cpus::this();
    // No spin lock held by the interrupted thread: arming may take `time.timers` (a thread queued here by
    // another CPU needs this CPU's tick).
    timers::tick_update(cs);
    if !(cs.need_resched.load(Ordering::Acquire) || cs.wake_head.load(Ordering::Acquire) != 0) || cs.in_idle.load(Ordering::Acquire) {
        return false;
    }
    PREEMPTIONS.inc();
    matches!(run::schedule(Leave::Preempt), Ok(true))
}
klog::counter!(PREEMPTIONS, "sched.irq_preemptions");

/// Keeps the caller on its CPU and unpreempted (by `irq_exit_preempt`) while the guard lives.
pub fn preempt_disable() -> ksync::PreemptGuard {
    ksync::PreemptGuard::new()
}

/// Sleeps at least `ns` nanoseconds (ended by the sleep timer, `timers`; costs nothing while asleep).
pub fn sleep_ns(ns: u64) {
    let until = time::now_ns().saturating_add(ns);
    if !STARTED.load(Ordering::Acquire) {
        time::delay_ns(ns);
        return;
    }
    ksync::lockdep::might_sleep();
    while time::now_ns() < until {
        HOOKS.prepare_to_block();
        HOOKS.block(Some(until));
    }
}

pub fn sleep_ms(ms: u64) {
    sleep_ns(ms.saturating_mul(1_000_000))
}

/// Sets the calling thread's KernelRt priority (1..=99; it matters only for KernelRt threads). It applies at
/// once: a thread that lowers itself below a waiting KernelRt thread of its CPU gives way (Linux
/// `sched_setscheduler`). Only the caller can change its own priority, and a running thread is in no queue, so
/// nothing has to be requeued; raising it needs no check (it runs already).
pub fn set_rt_priority(prio: u8) {
    let Some(t) = current_thread() else { return };
    let p = prio.clamp(1, RT_PRIO_MAX);
    let old = t.rt_prio.swap(p, Ordering::Relaxed);
    if t.class() != Class::KernelRt || !STARTED.load(Ordering::Acquire) {
        return;
    }
    let _irq = hal::cpu::IrqGuard::new();
    let cs = cpus::this();
    cs.curr_prio.store(p, Ordering::Relaxed);
    drop(t);
    if p < old && !hal::percpu::in_hard_irq() && ksync::preempt_count() == 0 && cs.rq.lock().best_rt_prio().is_some_and(|b| b > p) {
        // A refused switch was rolled back: the caller runs on and gives way at its next preemption point.
        cs.need_resched.store(true, Ordering::Relaxed);
        let _ = run::schedule(Leave::Preempt);
    }
}

/// Pins the calling thread to `cpu` (None: free again) and moves it there. False if `cpu` is not online.
pub fn pin_current(cpu: Option<usize>) -> bool {
    let Some(t) = current_thread() else { return false };
    match cpu {
        None => {
            t.pin.store(NO_CPU, Ordering::Relaxed);
            true
        }
        Some(c) => {
            if !cpus::of(c).is_some_and(|s| s.online.load(Ordering::Acquire)) {
                return false;
            }
            t.pin.store(c, Ordering::Relaxed);
            drop(t);
            // The requeue after the switch goes to the pinned CPU's queue.
            let mut tries = 0;
            while current_cpu() != c && tries < 1000 {
                {
                    let _irq = hal::cpu::IrqGuard::new();
                    let _ = run::schedule(Leave::Move);
                }
                tries += 1;
            }
            current_cpu() == c
        }
    }
}

/// Pins the calling thread to a CPU until dropped, then gives it its previous pin back (tests).
pub(crate) struct PinGuard(Option<usize>);

impl Drop for PinGuard {
    fn drop(&mut self) {
        pin_current(self.0);
    }
}

/// `pin_current(Some(cpu))` that undoes itself; None if the CPU is not online.
pub(crate) fn pin_for_now(cpu: usize) -> Option<PinGuard> {
    let old = current_thread()?.pinned();
    pin_current(Some(cpu)).then_some(PinGuard(old))
}

struct Hooks;
static HOOKS: Hooks = Hooks;

impl SchedHooks for Hooks {
    fn current(&self) -> ThreadRef {
        ThreadRef(current_id())
    }
    fn prepare_to_block(&self) {
        if let Some(t) = current_thread() {
            t.set_state(State::Blocking);
        }
    }
    fn block(&self, deadline_ns: Option<u64>) {
        let Some(me) = current_thread() else { return };
        if me.is_idle() {
            // The idle thread never sleeps; it polls.
            me.cas_state(State::Blocking, State::Running);
            idle::poll_pass(time::now_ns());
            return;
        }
        let d = deadline_ns.unwrap_or(0);
        if d != 0 {
            if time::now_ns() >= d {
                me.cas_state(State::Blocking, State::Running);
                return;
            }
            me.sleep_deadline.store(d, Ordering::Release);
            // One CPU for both steps: the deadline goes to the sleep timer of the CPU that records it.
            let added = {
                let _irq = hal::cpu::IrqGuard::new();
                let cpu = hal::percpu::this_cpu();
                let ok = sleepers::add(d, me.id, cpu);
                if ok {
                    timers::sleep_added(cpu, d);
                }
                ok
            };
            if !added {
                // No room for the deadline: nobody would end this sleep, so it becomes a yield and the caller
                // re-checks (its condition, and the time).
                klog::event!(Warn, "sched", "sleep.no_room", id = me.id; "no memory for a deadline; the timed wait yields instead");
                me.sleep_deadline.store(0, Ordering::Release);
                me.cas_state(State::Blocking, State::Running);
                drop(me);
                yield_now();
                return;
            }
        }
        let id = me.id;
        drop(me);
        {
            let _irq = hal::cpu::IrqGuard::new();
            // A refused switch was rolled back into a spurious wake-up; the caller re-checks and blocks again.
            let _ = run::schedule(Leave::Block);
        }
        if d != 0 {
            if let Some(me) = current_thread() {
                me.sleep_deadline.store(0, Ordering::Release);
            }
            sleepers::remove(d, id);
        }
    }
    fn wake(&self, r: ThreadRef) {
        // IRQ-safe: a table lookup (no allocation) and, in a handler, the lock-free wake list.
        if let Some(t) = table::get(r.0) {
            run::wake_thread(&t, hal::percpu::in_hard_irq());
        }
    }
    fn yield_now(&self) {
        yield_now()
    }
    fn now_ns(&self) -> u64 {
        time::now_ns()
    }
    fn pi_wait(&self, lock: usize, owner: &AtomicU64) -> bool {
        pi::mutex_wait(lock, owner)
    }
    fn pi_acquired(&self, lock: usize, waited: bool) {
        pi::mutex_acquired(lock, waited)
    }
    fn pi_gave_up(&self, lock: usize) {
        pi::mutex_gave_up(lock)
    }
    fn pi_released(&self, lock: usize) {
        pi::mutex_released(lock)
    }
    fn spin_on_owner(&self, owner: &AtomicU64, deadline_ns: u64, take: &dyn Fn() -> bool) -> bool {
        spin_on_owner(owner, deadline_ns, take)
    }
    fn rank(&self, r: ThreadRef) -> u8 {
        // Asked by the sleeper itself before it queues: its own thread, no table lookup.
        match current_thread() {
            Some(t) if t.id == r.0 => t.class() as u8,
            _ => table::get(r.0).map_or(u8::MAX, |t| t.class() as u8),
        }
    }
}

/// `SchedHooks::spin_on_owner` (INT-W7): Linux's `mutex_spin_on_owner` with a deadline. The owner's thread is
/// looked up only when the owner word changes (`u64::MAX`: taken but not yet named, or free: keep trying);
/// its `on_cpu` flag is then one atomic load per round. Never spins for the idle thread, before the scheduler
/// runs, or on a lock this thread itself holds.
fn spin_on_owner(owner: &AtomicU64, deadline_ns: u64, take: &dyn Fn() -> bool) -> bool {
    let me = match current_thread() {
        Some(t) if !t.is_idle() && STARTED.load(Ordering::Acquire) => t.id,
        _ => return false,
    };
    let (mut seen, mut th): (u64, Option<Arc<Thread>>) = (u64::MAX, None);
    let mut round = 0u32;
    loop {
        if take() {
            return true;
        }
        let o = owner.load(Ordering::Acquire);
        if o == me {
            return false;
        }
        if o != seen {
            seen = o;
            th = if o == u64::MAX { None } else { table::get(o) };
            if o != u64::MAX && th.is_none() {
                return false;
            }
        }
        if th.as_ref().is_some_and(|t| !t.ctx.is_on_cpu()) {
            return false;
        }
        round = round.wrapping_add(1);
        if round % 16 == 0 && (need_resched() || time::now_ns() >= deadline_ns) {
            return false;
        }
        core::hint::spin_loop();
    }
}

/// Snapshot for `ps`/MCP `cpus` (id, name, class, state).
pub fn threads() -> Vec<(u64, String, Class, &'static str)> {
    table::snapshot()
        .iter()
        .map(|t| {
            let st = match t.state() {
                State::Runnable => "runnable",
                State::Running => "running",
                // Blocking: on its way to sleep, or back from a wait whose condition came true first.
                State::Blocking => "running",
                State::Blocked => "sleeping",
                State::Dead => "dead",
            };
            (t.id, t.name.clone(), t.class(), st)
        })
        .collect()
}

/// One CPU as the scheduler sees it (MCP `sched`, tests, the Mini-PC check list).
#[derive(Clone, Copy, Debug, Default)]
pub struct CpuStat {
    pub cpu: usize,
    pub online: bool,
    pub idle: bool,
    pub halted: bool,
    pub queued: usize,
    pub busy_ns: u64,
    pub idle_ns: u64,
    /// SC-6: the part of `busy_ns` threads spent in user mode, and of that the part of threads with nice > 0
    /// (Linux /proc/stat's "user" + "nice"; system = busy - user).
    pub user_ns: u64,
    pub nice_ns: u64,
    pub switches: u64,
    pub migrations_in: u64,
    pub steals: u64,
    pub pushes: u64,
    pub resched_ipis: u64,
    pub irq_wakes: u64,
    pub topology: hal::smp::Topology,
    /// SC-4: heartbeat, tick armed, last idle entry (time, IF).
    pub heartbeat: u64,
    pub tick: bool,
    pub idle_entry: Option<(u64, bool)>,
}

pub fn cpu_stats() -> Vec<CpuStat> {
    let now = time::now_ns();
    (0..cpus::count())
        .filter_map(cpus::of)
        .map(|c| (c, c.busy_idle_at(now)))
        .map(|(c, (busy_ns, idle_ns))| CpuStat {
            cpu: c.cpu,
            online: c.online.load(Ordering::Acquire),
            idle: c.in_idle.load(Ordering::Relaxed),
            halted: c.halted.load(Ordering::Relaxed),
            queued: c.nr_queued.load(Ordering::Relaxed),
            busy_ns,
            idle_ns,
            user_ns: c.user_ns.load(Ordering::Relaxed),
            nice_ns: c.nice_ns.load(Ordering::Relaxed),
            switches: c.switches.load(Ordering::Relaxed),
            migrations_in: c.migrations_in.load(Ordering::Relaxed),
            steals: c.steals.load(Ordering::Relaxed),
            pushes: c.pushes.load(Ordering::Relaxed),
            resched_ipis: c.resched_ipis.load(Ordering::Relaxed),
            irq_wakes: c.irq_wakes.load(Ordering::Relaxed),
            topology: c.topo.get().copied().unwrap_or_default(),
            heartbeat: c.heartbeat.load(Ordering::Relaxed),
            tick: timers::tick_armed(c.cpu),
            idle_entry: timers::unpack_idle(c.idle_entry.load(Ordering::Relaxed)),
        })
        .collect()
}

/// One thread for listings.
#[derive(Clone, Debug)]
pub struct ThreadStat {
    pub id: u64,
    pub name: String,
    pub class: Class,
    /// SC-7a: thread group (tgid; 0 = kernel thread).
    pub group: u64,
    pub state: &'static str,
    pub cpu: usize,
    pub pinned: Option<usize>,
    pub rt_prio: u8,
    /// CPU time, the running slice included (= user_ns + system_ns).
    pub runtime_ns: u64,
    pub migrations: u64,
    pub wakeups: u64,
    /// SC-6: user/system split, voluntary/involuntary switches, nice, policy, affinity narrowed or not.
    pub user_ns: u64,
    pub system_ns: u64,
    pub nvcsw: u64,
    pub nivcsw: u64,
    pub nice: i32,
    pub policy: Policy,
    pub affinity_all: bool,
    /// SC-7a: longest wake-to-run wait while Interactive (ns).
    pub wait_max_ns: u64,
}

pub fn thread_stats() -> Vec<ThreadStat> {
    threads_detail()
}

fn threads_detail() -> Vec<ThreadStat> {
    table::snapshot()
        .iter()
        .map(|t| {
            let ct = t.cpu_times();
            ThreadStat {
                id: t.id,
                name: t.name.clone(),
                class: t.class(),
                group: t.group(),
                state: match t.state() {
                    State::Runnable => "runnable",
                    State::Running | State::Blocking => "running",
                    State::Blocked => "sleeping",
                    State::Dead => "dead",
                },
                cpu: t.last_cpu(),
                pinned: t.pinned(),
                rt_prio: if t.policy().is_rt() { t.user_rt.load(Ordering::Relaxed) } else { t.rt_prio() },
                runtime_ns: ct.total_ns(),
                migrations: t.migrations(),
                wakeups: t.wakeups.load(Ordering::Relaxed),
                user_ns: ct.user_ns,
                system_ns: ct.system_ns,
                nvcsw: ct.nvcsw,
                nivcsw: ct.nivcsw,
                nice: t.nice(),
                policy: t.policy(),
                affinity_all: t.affine_all.load(Ordering::Acquire),
                wait_max_ns: t.interactive_wait_max(),
            }
        })
        .collect()
}

/// The scheduler's counters (`sched.*`: sleep timer, ticks, fallbacks, halts, ...), sorted by name.
pub fn counters() -> Vec<(&'static str, u64)> {
    let mut c: Vec<(&'static str, u64)> = klog::metrics::snapshot().filter(|(n, _)| n.starts_with("sched.")).collect();
    c.sort();
    c
}

/// Kernel stacks mapped now, slots waiting for reuse, chunks of address space (hw_info, tests).
pub fn stack_stats() -> stack::StackStats {
    stack::stats()
}

/// Number of threads the scheduler knows (running, runnable, sleeping), the boot and idle threads included.
pub fn thread_count() -> usize {
    table::len()
}
