//! Per-CPU scheduler state (SC-3): the run queue with its own lock, what runs now (for wake-up preemption
//! decisions made by other CPUs), the interrupt-time wake list, the hand-over slot between the two halves of a
//! switch, and statistics. Fields other CPUs read without the queue lock are atomics.

use crate::place::{CpuView, Pref};
use crate::rq::RunQueue;
use crate::{Class, Leave, Thread};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use hal::percpu::PerCpu;
use hal::smp::Topology;
use ksync::{IrqSpinLock, Once};

pub(crate) struct CpuSched {
    pub cpu: usize,
    /// Threads waiting to run here.
    pub rq: IrqSpinLock<RunQueue<Arc<Thread>>>,
    /// The thread that just left this CPU and how (set before a switch, taken by whoever runs next here).
    pub after: IrqSpinLock<Option<(Arc<Thread>, Leave)>>,
    pub idle: Once<Arc<Thread>>,
    pub topo: Once<Topology>,
    /// Takes part in scheduling (its idle thread exists).
    pub online: AtomicBool,
    /// In `hlt` (an IPI is needed to get its attention).
    pub halted: AtomicBool,
    /// Runs its idle thread.
    pub in_idle: AtomicBool,
    /// The running thread should give the CPU up at its next preemption point.
    pub need_resched: AtomicBool,
    /// `rq.len()`, readable without the lock.
    pub nr_queued: AtomicUsize,
    /// The running thread: id, class, KernelRt priority, virtual runtime, start of its current run.
    pub curr_id: AtomicU64,
    pub curr_class: AtomicU8,
    pub curr_prio: AtomicU8,
    pub curr_vruntime: AtomicU64,
    pub curr_start: AtomicU64,
    /// SC-6: the running thread's policy (`attr::Policy` as u8; wake-up preemption rules).
    pub curr_policy: AtomicU8,
    /// Interrupt-time wake-ups placed here: a lock-free list of thread ids linked through `Thread::wake_next`.
    pub wake_head: AtomicU64,
    pub busy_ns: AtomicU64,
    pub idle_ns: AtomicU64,
    /// SC-6: user-mode time of the threads that ran here, and of that the time of threads with nice > 0.
    pub user_ns: AtomicU64,
    pub nice_ns: AtomicU64,
    /// Up to when `busy_ns`/`idle_ns` are accounted (the running thread's time since then is not in them yet).
    pub acct_at: AtomicU64,
    pub switches: AtomicU64,
    pub migrations_in: AtomicU64,
    pub steals: AtomicU64,
    pub pushes: AtomicU64,
    pub resched_ipis: AtomicU64,
    pub irq_wakes: AtomicU64,
    pub last_hk: AtomicU64,
    pub last_balance: AtomicU64,
    /// `curr_start` of the KernelRt run the watchdog last reported (one report per run).
    pub rt_reported: AtomicU64,
    /// SC-4: the tick (armed only with more than one runnable thread) and the deadline it was last armed for.
    pub tick: Once<time::HrTimer>,
    pub tick_at: AtomicU64,
    /// The sleep timer of the sleepers that went to sleep here (INT-A2a2: one per CPU).
    pub sleep: Once<time::HrTimer>,
    /// SC-4 (RB-3): bumped on every switch, idle return, preemption point, reschedule IPI and timer tick.
    pub heartbeat: AtomicU64,
    /// SC-4 (RB-3): the last idle entry, `timers::pack_idle(time, IF)` (0: never idle).
    pub idle_entry: AtomicU64,
}

impl CpuSched {
    fn new(cpu: usize) -> CpuSched {
        CpuSched {
            cpu,
            rq: IrqSpinLock::named("sched", RunQueue::new()),
            after: IrqSpinLock::named("sched", None),
            idle: Once::new(),
            topo: Once::new(),
            online: AtomicBool::new(false),
            halted: AtomicBool::new(false),
            in_idle: AtomicBool::new(false),
            need_resched: AtomicBool::new(false),
            nr_queued: AtomicUsize::new(0),
            curr_id: AtomicU64::new(0),
            curr_class: AtomicU8::new(Class::Normal as u8),
            curr_prio: AtomicU8::new(0),
            curr_vruntime: AtomicU64::new(0),
            curr_start: AtomicU64::new(0),
            curr_policy: AtomicU8::new(0),
            wake_head: AtomicU64::new(0),
            busy_ns: AtomicU64::new(0),
            idle_ns: AtomicU64::new(0),
            user_ns: AtomicU64::new(0),
            nice_ns: AtomicU64::new(0),
            acct_at: AtomicU64::new(0),
            switches: AtomicU64::new(0),
            migrations_in: AtomicU64::new(0),
            steals: AtomicU64::new(0),
            pushes: AtomicU64::new(0),
            resched_ipis: AtomicU64::new(0),
            irq_wakes: AtomicU64::new(0),
            last_hk: AtomicU64::new(0),
            last_balance: AtomicU64::new(0),
            rt_reported: AtomicU64::new(0),
            tick: Once::new(),
            tick_at: AtomicU64::new(0),
            sleep: Once::new(),
            heartbeat: AtomicU64::new(0),
            idle_entry: AtomicU64::new(0),
        }
    }

    /// One heartbeat (any context, also hard IRQ).
    pub fn beat(&self) {
        self.heartbeat.fetch_add(1, Ordering::Release);
    }

    /// Records an idle entry at `now` with the given IF state (this CPU's idle thread).
    pub fn record_idle(&self, now: u64, interrupts_on: bool) {
        self.idle_entry.store(crate::timers::pack_idle(now, interrupts_on), Ordering::Release);
    }

    /// Records `t` as the thread that runs here from now on.
    pub fn set_curr(&self, t: &Thread, now: u64) {
        self.curr_id.store(t.id, Ordering::Relaxed);
        self.curr_class.store(t.class() as u8, Ordering::Relaxed);
        self.curr_prio.store(t.rt_prio(), Ordering::Relaxed);
        self.curr_vruntime.store(t.vruntime.load(Ordering::Relaxed), Ordering::Relaxed);
        self.curr_policy.store(t.policy.load(Ordering::Relaxed), Ordering::Relaxed);
        self.curr_start.store(now, Ordering::Relaxed);
        self.acct_at.store(now, Ordering::Relaxed);
        self.in_idle.store(t.is_idle(), Ordering::Release);
    }

    /// Busy and idle time up to `now` (what `busy_ns`/`idle_ns` hold plus the running thread's time since).
    pub fn busy_idle_at(&self, now: u64) -> (u64, u64) {
        let (b, i, at) = (self.busy_ns.load(Ordering::Relaxed), self.idle_ns.load(Ordering::Relaxed), self.acct_at.load(Ordering::Relaxed));
        let open = if at == 0 { 0 } else { now.saturating_sub(at) };
        if self.in_idle.load(Ordering::Relaxed) { (b, i + open) } else { (b + open, i) }
    }

    /// Something to do here: queued threads, interrupt-time wake-ups, or a reschedule request.
    pub fn has_work(&self) -> bool {
        self.nr_queued.load(Ordering::Acquire) > 0 || self.wake_head.load(Ordering::Acquire) != 0 || self.need_resched.load(Ordering::Acquire)
    }

    /// Idle for placement: runs the idle thread with nothing waiting.
    pub fn is_idle(&self) -> bool {
        self.in_idle.load(Ordering::Acquire) && self.nr_queued.load(Ordering::Acquire) == 0 && self.wake_head.load(Ordering::Acquire) == 0
    }

    /// Threads running or waiting here.
    pub fn load(&self) -> usize {
        self.nr_queued.load(Ordering::Acquire) + usize::from(!self.in_idle.load(Ordering::Acquire))
    }

    pub fn view(&self) -> CpuView {
        let t = self.topo.get().copied();
        CpuView {
            idle: self.is_idle(),
            load: self.load(),
            core: t.map(|t| t.core_key()).unwrap_or((u32::MAX, self.cpu as u32)),
            cluster: t.map(|t| (t.package_id, t.cluster_id)).unwrap_or((0, 0)),
            core_type: t.map(|t| t.core_type).unwrap_or(hal::smp::CORE_TYPE_UNIFORM),
            // Read on every view: it divides by the running maximum, which settles only at the smp shape event.
            capacity: t.map(|t| t.capacity(hal::smp::max_perf())).unwrap_or(hal::smp::CAPACITY_SCALE),
        }
    }

    /// Whether the fair thread running here has used up its slice while others wait.
    pub fn slice_expired(&self, now: u64) -> bool {
        Class::from_u8(self.curr_class.load(Ordering::Relaxed)) != Class::KernelRt
            && !self.in_idle.load(Ordering::Relaxed)
            && self.nr_queued.load(Ordering::Relaxed) > 0
            && now.saturating_sub(self.curr_start.load(Ordering::Relaxed)) >= crate::SLICE_NS
    }
}

static CPUS: PerCpu<CpuSched> = PerCpu::new(CpuSched::new);

/// This CPU's state (call with interrupts off, or accept that the answer may be stale).
pub(crate) fn this() -> &'static CpuSched {
    CPUS.get()
}

/// The state of `cpu` (None beyond the possible CPUs).
pub(crate) fn of(cpu: usize) -> Option<&'static CpuSched> {
    CPUS.get_for(cpu)
}

/// Online scheduler CPUs.
pub(crate) fn online() -> impl Iterator<Item = &'static CpuSched> {
    (0..hal::percpu::cpu_count()).filter_map(of).filter(|c| c.online.load(Ordering::Acquire))
}

pub(crate) fn count() -> usize {
    hal::percpu::cpu_count()
}

pub(crate) fn view_of(c: usize) -> Option<CpuView> {
    of(c).filter(|s| s.online.load(Ordering::Acquire)).map(|s| s.view())
}

pub(crate) fn pref_of(class: Class) -> Pref {
    match class {
        Class::KernelRt | Class::Interactive => Pref::Performance,
        Class::Normal => Pref::Any,
        Class::Background => Pref::Efficiency,
    }
}

/// Where `t` should run (allocation-free; also from interrupt handlers). Its pinned CPU if it has one, else a
/// CPU its affinity mask allows (SC-6; any online CPU if the mask holds none online, which `set_affinity`
/// refuses).
pub(crate) fn select_for(t: &Thread) -> usize {
    let pin = t.pin.load(Ordering::Relaxed);
    if pin != crate::NO_CPU && of(pin).is_some_and(|c| c.online.load(Ordering::Acquire)) {
        return pin;
    }
    let prev = t.cpu.load(Ordering::Relaxed);
    let prev = (prev != crate::NO_CPU).then_some(prev);
    let allowed = |c: usize| if t.may_run_on(c) { view_of(c) } else { None };
    crate::place::select(count(), &allowed, prev, pref_of(t.class()))
        .or_else(|| crate::place::select(count(), &view_of, prev, pref_of(t.class())))
        .unwrap_or_else(hal::percpu::this_cpu)
}

// ---------------------------------------------------------------------------------------------------------
// The reschedule IPI (a system vector, `hal::vectors::register_system`)

static RESCHED_VECTOR: AtomicU8 = AtomicU8::new(0);
klog::counter!(pub RESCHED_SENT, "sched.resched_ipis_sent");

fn resched_irq(_v: u8) {
    // Hard-IRQ context: count, and pass a quiescent point (the reaper's sync generations). The flag that asks
    // for a switch was set by the sender; the thread here acts on it at its next preemption point.
    if let Some(c) = of(hal::percpu::this_cpu()) {
        c.resched_ipis.fetch_add(1, Ordering::Relaxed);
        // SC-4: a CPU that takes interrupts is alive (RB-3's `probe_cpu`).
        c.beat();
    }
    hal::switch::quiescent_point();
}

/// Registers the reschedule vector (first free system vector from 0xF2). Returns it, or None.
pub(crate) fn register_resched() -> Option<u8> {
    for v in 0xF2..=hal::vectors::LAST_SYSTEM {
        if hal::vectors::register_system(v, resched_irq).is_ok() {
            RESCHED_VECTOR.store(v, Ordering::Release);
            return Some(v);
        }
    }
    None
}

pub(crate) fn resched_vector() -> u8 {
    RESCHED_VECTOR.load(Ordering::Acquire)
}

/// Sends the reschedule IPI to `cpu` (not to this CPU; IRQ-safe).
pub(crate) fn send_resched(cpu: usize) {
    let v = resched_vector();
    if v == 0 || cpu == hal::percpu::this_cpu() {
        return;
    }
    if let Some(apic) = hal::smp::apic_id(cpu)
        && hal::apic::send_ipi(hal::apic::IpiDest::Physical(apic), hal::apic::IpiKind::Fixed(v)).is_ok()
    {
        RESCHED_SENT.inc();
    }
}

/// Asks `cs` to reschedule: sets its flag and, if it sleeps in `hlt` (or `urgent`, so an interrupt-exit
/// preemption can act at once), sends the IPI. IRQ-safe.
pub(crate) fn kick(cs: &CpuSched, urgent: bool) {
    cs.need_resched.store(true, Ordering::SeqCst);
    // Pairs with the fence in `idle::halt`: either it sees the flag, or this sees `halted`.
    if cs.cpu != hal::percpu::this_cpu() && (urgent || cs.halted.load(Ordering::SeqCst)) {
        send_resched(cs.cpu);
    }
}

/// A KernelRt thread that kept its CPU this long without blocking or giving way is reported (B4: KernelRt code
/// has no unbounded loops; nothing throttles it, so fair threads on that CPU starve meanwhile).
pub(crate) const RT_HOG_NS: u64 = 1_000_000_000;
static LAST_RT_CHECK: AtomicU64 = AtomicU64::new(0);

/// The KernelRt watchdog (from `poll_pass`, at most every 100 ms): one Warn per run longer than `RT_HOG_NS`.
pub(crate) fn rt_watchdog(now: u64) {
    let last = LAST_RT_CHECK.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 100_000_000 || LAST_RT_CHECK.compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        return;
    }
    for cs in online() {
        if Class::from_u8(cs.curr_class.load(Ordering::Relaxed)) != Class::KernelRt || cs.in_idle.load(Ordering::Relaxed) {
            continue;
        }
        let start = cs.curr_start.load(Ordering::Relaxed);
        if start == 0 || now.saturating_sub(start) < RT_HOG_NS || cs.rt_reported.swap(start, Ordering::Relaxed) == start {
            continue;
        }
        RT_HOGS.inc();
        klog::event!(Warn, "sched", "rt.hog", cpu = cs.cpu, id = cs.curr_id.load(Ordering::Relaxed), ran_ms = now.saturating_sub(start) / 1_000_000,
            waiting = cs.nr_queued.load(Ordering::Relaxed); "a KernelRt thread holds its CPU without blocking (B4); fair threads there starve");
    }
}
klog::counter!(pub RT_HOGS, "sched.rt_hogs");
