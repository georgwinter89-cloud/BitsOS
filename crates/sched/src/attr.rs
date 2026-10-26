//! SC-6: what a program may set about its threads' scheduling, with Linux's meaning (the syscalls are in
//! `syscall::sched_sys`): the **nice value** (-20..19) and the **policy** (SCHED_OTHER, SCHED_BATCH, SCHED_IDLE)
//! feed the fair class's weights; the **CPU affinity mask** limits where a thread runs.
//!
//! Weights: Linux's `sched_prio_to_weight` (linux-6.6/kernel/sched/core.c:11542), SCHED_IDLE = 3
//! (`WEIGHT_IDLEPRIO`, kernel/sched/sched.h:2179). The B4 class sets a thread's base nice (Interactive -5,
//! Normal 0, Background 10: the class weights SC-3 used); the program's nice is added to it (clamped to
//! -20..19), so a program can never lift itself above what policy granted its class, only move inside it.
//! SCHED_BATCH keeps the weight but never preempts on wake-up, SCHED_IDLE has weight 3 and is preempted by any
//! other fair thread that wakes (Linux `check_preempt_wakeup`, kernel/sched/fair.c:8093-8103).
//!
//! A change of weight acts from the next accounting on; a queued thread keeps its place (Linux 6.6 rescales
//! the lag with `reweight_entity`; V2's queue position is only a start value, the next slice corrects it).
//!
//! SC-7a (B4): SCHED_FIFO/SCHED_RR are accepted only for a thread whose group holds an Interactive grant
//! (`interactive`; the grant takes the place of Linux's privilege, root alone is not enough: "granted by policy,
//! never self-raised") or, without one, as Linux allows with RLIMIT_RTPRIO 0: the same RT policy at the same or
//! a lower priority (linux-6.6/kernel/sched/core.c:7572-7582). They run in the Interactive class (fair weight of
//! nice -5, the program's nice does not count, as Linux ignores it for RT); the priority is stored and reported,
//! not used for ordering (a strict RT order could starve, B4). SCHED_IDLE runs in the Background class (weight
//! 3 as Linux). A thread's class follows from its spawn class, its policy and its group's grant
//! (`effective_class`), recomputed under `WRITERS` whenever one of them changes.
//!
//! Affinity: one bit per possible CPU (Linux `nr_cpu_ids` = `hal::percpu::possible_cpus()`), kept in atomic
//! words allocated once with the thread, so placement reads it without a lock, also in interrupt handlers.
//! A kernel pin (`pin_current`, `SpawnOpts::pin`) goes before the mask. A new mask moves the thread at once
//! when it runs or waits on a CPU it may no longer use (Linux `affine_move_task`): the caller itself switches
//! away, a running thread is preempted (its requeue picks an allowed CPU), a queued one is taken out and
//! queued again.

use crate::cpus;
use crate::run::{self, Why};
use crate::{Class, NO_CPU, State, Thread, table};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::SpinLock;

pub const MIN_NICE: i32 = -20;
pub const MAX_NICE: i32 = 19;
/// Linux's `sched_prio_to_weight`, nice -20 first (linux-6.6/kernel/sched/core.c:11542).
const NICE_WEIGHTS: [u64; 40] = [
    88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916, 9548, 7620, 6100, 4904, 3906, 3121, 2501, 1991, 1586, 1277, 1024, 820, 655, 526,
    423, 335, 272, 215, 172, 137, 110, 87, 70, 56, 45, 36, 29, 23, 18, 15,
];
/// SCHED_IDLE's weight (`WEIGHT_IDLEPRIO`).
pub const WEIGHT_IDLEPRIO: u64 = 3;

/// The fair policies a program may choose (Linux numbers). SCHED_FIFO/RR (1, 2) and SCHED_DEADLINE (6) are
/// not policies of a V2 thread: the syscalls refuse them (SC-7 maps FIFO/RR onto the Interactive class).
/// SCHED_EXT (7, Linux 6.12+ and CachyOS, the ABI diff's oracle) is accepted and runs as SCHED_OTHER, as Linux
/// runs it while no BPF scheduler is loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Policy {
    Other = 0,
    /// SC-7a: SCHED_FIFO/SCHED_RR, only by grant (see the module comment).
    Fifo = 1,
    Rr = 2,
    Batch = 3,
    Idle = 5,
    Ext = 7,
}

impl Policy {
    pub fn from_linux(p: i32) -> Option<Policy> {
        match p {
            0 => Some(Policy::Other),
            1 => Some(Policy::Fifo),
            2 => Some(Policy::Rr),
            3 => Some(Policy::Batch),
            5 => Some(Policy::Idle),
            7 => Some(Policy::Ext),
            _ => None,
        }
    }
    pub(crate) fn from_u8(v: u8) -> Policy {
        Policy::from_linux(v as i32).unwrap_or(Policy::Other)
    }
    pub fn linux(self) -> i32 {
        self as i32
    }
    /// SCHED_FIFO or SCHED_RR.
    pub fn is_rt(self) -> bool {
        matches!(self, Policy::Fifo | Policy::Rr)
    }
    pub fn name(self) -> &'static str {
        match self {
            Policy::Other => "other",
            Policy::Fifo => "fifo",
            Policy::Rr => "rr",
            Policy::Batch => "batch",
            Policy::Idle => "idle",
            Policy::Ext => "ext",
        }
    }
}

/// Linux's weight of nice `nice` (clamped to -20..19).
pub fn nice_weight(nice: i32) -> u64 {
    NICE_WEIGHTS[(nice.clamp(MIN_NICE, MAX_NICE) - MIN_NICE) as usize]
}

/// The nice value a class stands for (its SC-3 weight: 3121, 1024, 110).
pub fn base_nice(class: Class) -> i32 {
    match class {
        Class::Interactive => -5,
        Class::KernelRt | Class::Normal => 0,
        Class::Background => 10,
    }
}

/// A fair thread's weight from its class, policy and nice value (pure).
pub fn weight_of(class: Class, policy: Policy, nice: i32) -> u64 {
    if policy == Policy::Idle {
        return WEIGHT_IDLEPRIO;
    }
    if policy.is_rt() {
        return nice_weight(base_nice(class));
    }
    nice_weight(base_nice(class).saturating_add(nice))
}

/// The class a thread runs in (pure): KernelRt stays; SCHED_IDLE runs as Background; a Background spawn stays;
/// SCHED_BATCH is never Interactive; a Normal user thread whose group holds a grant runs as Interactive
/// (FIFO/RR included: without the grant they run as Normal, the policy is still reported).
pub fn effective_class(base: Class, policy: Policy, granted: bool) -> Class {
    match (base, policy) {
        (Class::KernelRt, _) => Class::KernelRt,
        (_, Policy::Idle) | (Class::Background, _) => Class::Background,
        (_, Policy::Batch) => Class::Normal,
        (Class::Interactive, _) => Class::Interactive,
        (Class::Normal, _) if granted => Class::Interactive,
        _ => Class::Normal,
    }
}

/// SC-7b: the class a thread of class `own` runs in while waiters lend it `boost` (`Class` as u8,
/// `crate::pi::NO_BOOST` for none): the more urgent of the two among the fair classes; KernelRt stays and nothing
/// is lent above Interactive (pure).
pub fn boosted_class(own: Class, boost: u8) -> Class {
    if own == Class::KernelRt || boost == crate::pi::NO_BOOST {
        return own;
    }
    own.min(Class::from_u8(boost).max(Class::Interactive))
}

/// Whether a program may move a thread from `cur` (RT priority `cur_prio`) to the RT policy `new` at `prio`:
/// with a grant always; without one only as Linux allows with RLIMIT_RTPRIO 0 (same policy, priority not
/// raised; linux-6.6/kernel/sched/core.c:7575-7582).
pub fn rt_change_allowed(cur: Policy, cur_prio: u8, new: Policy, prio: u8, granted: bool) -> bool {
    granted || (new == cur && prio <= cur_prio)
}

/// Why `set_scheduler` refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedError {
    /// No such thread (it ended).
    NoThread,
    /// SCHED_FIFO/RR without an Interactive grant (Linux's EPERM).
    NotGranted,
    /// RT priority outside 1..=99 for FIFO/RR, or not 0 for a fair policy (Linux's EINVAL).
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttrError {
    /// No such thread (it ended).
    NoThread,
    /// The mask holds no online CPU.
    Invalid,
    /// The thread has no mask words (its allocation failed at spawn).
    NoMemory,
}

/// Words of a mask for `cpus` CPUs (Linux `cpumask_size() / 8` with `nr_cpu_ids` = `cpus`).
pub fn mask_words(cpus: usize) -> usize {
    cpus.max(1).div_ceil(64)
}

/// Bit `cpu` of a mask (missing words are 0).
pub fn mask_has(mask: &[u64], cpu: usize) -> bool {
    mask.get(cpu / 64).is_some_and(|w| (w >> (cpu % 64)) & 1 == 1)
}

/// `mask` limited to the first `cpus` CPUs, in `mask_words(cpus)` words (Linux ANDs a new mask with the
/// possible CPUs before it checks it, so bits beyond them are dropped, never an error).
pub fn mask_trim(mask: &[u64], cpus: usize) -> Vec<u64> {
    let n = mask_words(cpus);
    let mut v: Vec<u64> = (0..n).map(|i| mask.get(i).copied().unwrap_or(0)).collect();
    let rest = cpus % 64;
    if rest != 0 && cpus > 0 {
        v[n - 1] &= (1u64 << rest) - 1;
    }
    v
}

klog::counter!(NICE_SETS, "sched.nice_sets");
klog::counter!(POLICY_SETS, "sched.policy_sets");
klog::counter!(AFFINITY_SETS, "sched.affinity_sets");
klog::counter!(pub RT_REFUSED, "sched.rt_refused");
klog::counter!(pub CLASS_CHANGES, "sched.class_changes");

/// Serialises writers of thread attributes (two `set_affinity` on one thread must not mix their words);
/// readers never take it.
pub(crate) static WRITERS: SpinLock<()> = SpinLock::named("sched.attr", ());

fn thread(id: u64) -> Result<Arc<Thread>, AttrError> {
    table::get(id).filter(|t| t.state() != State::Dead).ok_or(AttrError::NoThread)
}

impl Thread {
    /// The thread's nice value (-20..19, without its class's base).
    pub fn nice(&self) -> i32 {
        self.nice.load(Ordering::Relaxed)
    }
    pub fn policy(&self) -> Policy {
        Policy::from_u8(self.policy.load(Ordering::Relaxed))
    }
    pub fn reset_on_fork(&self) -> bool {
        self.reset_on_fork.load(Ordering::Relaxed)
    }
    /// Its weight in the fair class now.
    pub fn weight(&self) -> u64 {
        self.weight.load(Ordering::Relaxed)
    }
    /// SC-7a: its RT priority as a program set it (0 under a fair policy).
    pub fn user_rt_priority(&self) -> u8 {
        self.user_rt.load(Ordering::Relaxed)
    }
    /// Recomputes its class (from spawn class, policy and its group's grant) and its weight. Callers hold
    /// `WRITERS` (lock order: `sched.attr` before `sched.grants`). Returns the class before and after.
    /// SC-7b: a class lent by priority passing (`boosted_class`) runs with that class's weight under a normal
    /// policy (a SCHED_IDLE or niced-down holder must not keep an Interactive waiter waiting); its own nice only
    /// counts when it raises the weight further.
    fn reweight(&self) -> (Class, Class) {
        let own = effective_class(self.base, self.policy(), crate::interactive::granted(self.group()));
        self.own.store(own as u8, Ordering::Relaxed);
        let class = boosted_class(own, self.boost.load(Ordering::Acquire));
        let old = Class::from_u8(self.class.swap(class as u8, Ordering::Relaxed));
        let w = if class < own { weight_of(class, Policy::Other, self.nice().min(0)) } else { weight_of(class, self.policy(), self.nice()) };
        self.weight.store(w, Ordering::Relaxed);
        (old, class)
    }
    /// SCHED_BATCH and SCHED_IDLE threads never preempt on wake-up (Linux `check_preempt_wakeup`).
    pub(crate) fn wakeup_preempts(policy: u8) -> bool {
        policy != Policy::Batch as u8 && policy != Policy::Idle as u8
    }
    /// Whether it may run on `cpu` (its kernel pin first, then its affinity mask). Allocation- and lock-free.
    pub fn may_run_on(&self, cpu: usize) -> bool {
        let pin = self.pin.load(Ordering::Relaxed);
        if pin != NO_CPU {
            return pin == cpu;
        }
        self.affine_all.load(Ordering::Acquire) || self.affinity.get(cpu / 64).is_some_and(|w| (w.load(Ordering::Relaxed) >> (cpu % 64)) & 1 == 1)
    }
    /// Its affinity mask as words (all possible CPUs when never narrowed).
    pub fn affinity_mask(&self) -> Vec<u64> {
        let n = mask_words(hal::percpu::possible_cpus());
        if self.affine_all.load(Ordering::Acquire) {
            return mask_trim(&alloc::vec![u64::MAX; n], hal::percpu::possible_cpus());
        }
        (0..n).map(|i| self.affinity.get(i).map_or(0, |w| w.load(Ordering::Relaxed))).collect()
    }
}

/// The words a new thread's mask needs (all ones; None: no memory, the thread then keeps "every CPU").
pub(crate) fn new_mask() -> alloc::boxed::Box<[AtomicU64]> {
    let n = mask_words(hal::percpu::possible_cpus());
    let mut v: Vec<AtomicU64> = Vec::new();
    if v.try_reserve_exact(n).is_err() {
        return alloc::boxed::Box::new([]);
    }
    v.extend((0..n).map(|_| AtomicU64::new(u64::MAX)));
    v.into_boxed_slice()
}

pub fn nice(id: u64) -> Option<i32> {
    thread(id).ok().map(|t| t.nice())
}

/// Sets thread `id`'s nice value (clamped to -20..19; permission checks are the caller's, as `set_user_nice`).
/// A thread that runs and lost priority is asked to reschedule (Linux `prio_changed_fair`).
pub fn set_nice(id: u64, nice: i32) -> Result<(), AttrError> {
    let t = thread(id)?;
    let nice = nice.clamp(MIN_NICE, MAX_NICE);
    let old = {
        let _w = WRITERS.lock();
        let old = t.nice.swap(nice, Ordering::Relaxed);
        let _ = t.reweight();
        old
    };
    NICE_SETS.inc();
    klog::event!(Debug, "sched", "nice.set", id = id, name = t.name.as_str(), old = old, new = nice, weight = t.weight());
    if nice > old {
        let _ = resched_if_running(&t);
    }
    Ok(())
}

/// Thread `id`'s policy and its reset-on-fork flag.
pub fn policy(id: u64) -> Option<(Policy, bool)> {
    thread(id).ok().map(|t| (t.policy(), t.reset_on_fork()))
}

/// Sets thread `id`'s fair policy (its nice value stays, as `sched_setscheduler` keeps `static_prio`).
/// SCHED_FIFO/RR go through `set_scheduler` (Invalid here).
pub fn set_policy(id: u64, policy: Policy, reset_on_fork: bool) -> Result<(), AttrError> {
    match set_scheduler(id, policy, 0, reset_on_fork) {
        Ok(()) => Ok(()),
        Err(SchedError::NoThread) => Err(AttrError::NoThread),
        Err(_) => Err(AttrError::Invalid),
    }
}

/// Thread `id`'s RT priority as the program set it (0 under a fair policy; `sched_getparam`).
pub fn rt_priority(id: u64) -> Option<u8> {
    thread(id).ok().map(|t| t.user_rt_priority())
}

/// Whether thread `id`'s group holds an Interactive grant (it may ask for SCHED_FIFO/RR).
pub fn rt_granted(id: u64) -> bool {
    thread(id).ok().is_some_and(|t| crate::interactive::granted(t.group()))
}

/// SC-7a: `sched_setscheduler`'s scheduler part for thread `id`: a fair policy (`rt_prio` must be 0) or
/// SCHED_FIFO/RR at `rt_prio` 1..=99 when `rt_change_allowed` (the grant is checked under the writers' lock,
/// so a grant revoked at the same moment cannot be missed). The caller checks ownership and B3 first.
pub fn set_scheduler(id: u64, policy: Policy, rt_prio: u8, reset_on_fork: bool) -> Result<(), SchedError> {
    let t = thread(id).map_err(|_| SchedError::NoThread)?;
    if policy.is_rt() != (rt_prio != 0) || rt_prio > crate::RT_PRIO_MAX {
        return Err(SchedError::Invalid);
    }
    let (old, classes) = {
        let _w = WRITERS.lock();
        let cur = t.policy();
        if policy.is_rt() && !rt_change_allowed(cur, t.user_rt_priority(), policy, rt_prio, crate::interactive::granted(t.group())) {
            drop(_w);
            RT_REFUSED.inc();
            klog::event!(Info, "sched", "rt.refused", id = id, name = t.name.as_str(), group = t.group(), policy = policy.name(), priority = rt_prio;
                "SCHED_FIFO/RR only with an Interactive grant (B4)");
            return Err(SchedError::NotGranted);
        }
        t.policy.store(policy as u8, Ordering::Relaxed);
        t.user_rt.store(rt_prio, Ordering::Relaxed);
        t.reset_on_fork.store(reset_on_fork, Ordering::Relaxed);
        (cur, t.reweight())
    };
    POLICY_SETS.inc();
    klog::event!(Debug, "sched", "policy.set", id = id, name = t.name.as_str(), old = old.name(), new = policy.name(), rt_prio = rt_prio, reset_on_fork = reset_on_fork, class = classes.1.name(), weight = t.weight());
    class_changed(&t, classes);
    if policy != old && classes.0 == classes.1 {
        let _ = resched_if_running(&t);
    }
    Ok(())
}

/// Recomputes `t`'s class and weight (a grant of its group changed, or it was just spawned into a group).
pub(crate) fn refresh(t: &Thread) {
    let classes = {
        let _w = WRITERS.lock();
        t.reweight()
    };
    class_changed(t, classes);
}

/// After a class change: the CPU running `t` reschedules (its per-CPU class mirror is refreshed there by
/// `run::account`, and a lowered thread gives way); a queued thread that was raised is checked against the
/// thread running on its CPU at once (`run::check_preempt`), so the program brought to the front does not wait
/// for that CPU's next tick; the change is an event (B6).
fn class_changed(t: &Thread, (old, new): (Class, Class)) {
    if old == new {
        return;
    }
    CLASS_CHANGES.inc();
    klog::event!(Debug, "sched", "class.set", id = t.id, name = t.name.as_str(), group = t.group(), old = old.name(), new = new.name(), weight = t.weight());
    // SC-7b: a waiter's donation follows its class (a no-op unless it waits for a lock that passes on priority).
    crate::pi::class_changed(t);
    if resched_if_running(t) {
        return;
    }
    // Lower = more urgent. A queued thread's `cpu` is the CPU whose queue holds it (`run::enqueue`); a race with
    // its switch-in only costs a spurious kick.
    if new < old
        && t.state() == State::Runnable
        && let Some(cs) = cpus::of(t.last_cpu())
    {
        run::check_preempt(cs, t);
    }
}

/// Thread `id`'s affinity mask ANDed with the online CPUs (what `sched_getaffinity` reports: Linux ANDs with
/// `cpu_active_mask`), in `mask_words(possible_cpus())` words.
pub fn affinity(id: u64) -> Option<Vec<u64>> {
    let t = thread(id).ok()?;
    let mut m = t.affinity_mask();
    for (i, w) in m.iter_mut().enumerate() {
        for b in 0..64 {
            let cpu = i * 64 + b;
            if (*w >> b) & 1 == 1 && !cpus::of(cpu).is_some_and(|c| c.online.load(Ordering::Acquire)) {
                *w &= !(1u64 << b);
            }
        }
    }
    Some(m)
}

/// Sets thread `id`'s affinity: `mask` limited to the possible CPUs must hold an online CPU (Invalid
/// otherwise, Linux's EINVAL). Moves the thread off a CPU it may no longer use.
pub fn set_affinity(id: u64, mask: &[u64]) -> Result<(), AttrError> {
    let t = thread(id)?;
    let possible = hal::percpu::possible_cpus();
    let m = mask_trim(mask, possible);
    if !(0..possible).any(|c| mask_has(&m, c) && cpus::of(c).is_some_and(|s| s.online.load(Ordering::Acquire))) {
        return Err(AttrError::Invalid);
    }
    if t.affinity.len() < m.len() {
        return Err(AttrError::NoMemory);
    }
    let all = (0..possible).all(|c| mask_has(&m, c));
    {
        let _w = WRITERS.lock();
        if all {
            t.affine_all.store(true, Ordering::Release);
        } else {
            // Narrowing: the words first, then the flag readers test first, so nobody sees "not all" with
            // old words. Widening to a smaller set than all: readers may briefly see a mix of old and new
            // words (one placement decision), never a CPU outside both masks.
            for (w, v) in t.affinity.iter().zip(m.iter()) {
                w.store(*v, Ordering::Relaxed);
            }
            t.affine_all.store(false, Ordering::Release);
        }
    }
    AFFINITY_SETS.inc();
    // Pairs with the fence in `run::enqueue` (mask stored, then `t.cpu` read here; `t.cpu` stored, then the
    // mask read there): at least one side sees the other's store, so a racing enqueue cannot leave `t` on a
    // CPU the new mask forbids.
    core::sync::atomic::fence(Ordering::SeqCst);
    let moved = enforce(&t);
    klog::event!(Debug, "sched", "affinity.set", id = id, name = t.name.as_str(), mask = klog::Hex(m.first().copied().unwrap_or(0)), words = m.len(), all = all, moved = moved);
    Ok(())
}

/// Copies nice, policy and affinity from thread `from` to `to` (fork and clone: Linux `sched_fork` and the
/// mask `dup_task_struct` copies). With reset-on-fork the child gets nice 0 if the parent's was negative, keeps
/// a fair policy, and the flag is cleared (linux-6.6/kernel/sched/core.c `sched_fork`).
pub fn copy_attrs(from: u64, to: u64) -> Result<(), AttrError> {
    let p = thread(from)?;
    let c = thread(to)?;
    let (mut nice, mut policy, mut rt, reset) = (p.nice(), p.policy(), p.user_rt_priority(), p.reset_on_fork());
    if reset && policy.is_rt() {
        (policy, rt, nice) = (Policy::Other, 0, 0);
    }
    if reset && nice < 0 {
        nice = 0;
    }
    // SC-7a: the class is not copied; the child's own group decides (a fork's child is a new process without
    // the parent's grant; a FIFO policy it inherits runs as Normal until its group is granted).
    let classes = {
        let _w = WRITERS.lock();
        c.nice.store(nice, Ordering::Relaxed);
        c.policy.store(policy as u8, Ordering::Relaxed);
        c.user_rt.store(rt, Ordering::Relaxed);
        c.reset_on_fork.store(false, Ordering::Relaxed);
        c.reweight()
    };
    class_changed(&c, classes);
    let mask = p.affinity_mask();
    if p.affine_all.load(Ordering::Acquire) {
        c.affine_all.store(true, Ordering::Release);
        Ok(())
    } else {
        set_affinity(to, &mask)
    }
}

/// Asks the CPU running `t` (if any) to reschedule soon; whether one runs it.
fn resched_if_running(t: &Thread) -> bool {
    if let Some(cs) = cpus::online().find(|c| c.curr_id.load(Ordering::Acquire) == t.id) {
        cpus::kick(cs, false);
        return true;
    }
    false
}

klog::counter!(pub AFFINITY_MOVES, "sched.affinity_moves");

/// Moves `t` off a CPU its mask no longer allows. Returns whether it acted. The caller itself switches away
/// (bounded tries, as `pin_current`); a thread running elsewhere is preempted (urgent kick: its requeue in
/// `after_switch` goes through `enqueue`, which picks an allowed CPU); a queued one is taken out of its queue and
/// queued again; a sleeping one is placed by its next wake-up.
fn enforce(t: &Arc<Thread>) -> bool {
    if t.id == crate::current_id() {
        if t.may_run_on(crate::current_cpu()) {
            return false;
        }
        let mut tries = 0;
        while !t.may_run_on(crate::current_cpu()) && tries < 1000 {
            {
                let _irq = hal::cpu::IrqGuard::new();
                let _ = run::schedule(crate::Leave::Move);
            }
            tries += 1;
        }
        AFFINITY_MOVES.inc();
        return true;
    }
    if let Some(cs) = cpus::online().find(|c| c.curr_id.load(Ordering::Acquire) == t.id) {
        if t.may_run_on(cs.cpu) {
            return false;
        }
        cpus::kick(cs, true);
        AFFINITY_MOVES.inc();
        return true;
    }
    let cpu = t.cpu.load(Ordering::Relaxed);
    let Some(cs) = cpus::of(cpu) else { return false };
    if t.may_run_on(cpu) || t.state() != State::Runnable {
        return false;
    }
    let taken = {
        let mut rq = cs.rq.lock();
        let r = rq.remove_id(t.id);
        if let Some((_, crate::rq::Taken::Fair(v))) = &r {
            t.lag.store(run::lag_of(*v, rq.min_vruntime), Ordering::Relaxed);
        }
        cs.nr_queued.store(rq.len(), Ordering::Release);
        r
    };
    match taken {
        Some((t, _)) => {
            let to = cpus::select_for(&t);
            run::enqueue(t, to, Why::Migrate);
            AFFINITY_MOVES.inc();
            true
        }
        // Picked by its CPU in the meantime (it runs there now: the urgent kick makes its next preemption point,
        // which turns into `Leave::Move`), or not queued yet (its `enqueue` checks the mask again under this
        // queue's lock and places it elsewhere).
        None => {
            cpus::kick(cs, true);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn boosts_lend_only_among_the_fair_classes() {
        use crate::pi::NO_BOOST;
        assert_eq!(boosted_class(Class::Background, NO_BOOST), Class::Background);
        assert_eq!(boosted_class(Class::Background, Class::Normal as u8), Class::Normal);
        assert_eq!(boosted_class(Class::Normal, Class::Interactive as u8), Class::Interactive);
        assert_eq!(boosted_class(Class::Normal, Class::KernelRt as u8), Class::Interactive, "never above Interactive");
        assert_eq!(boosted_class(Class::Interactive, Class::Background as u8), Class::Interactive, "never lowers");
        assert_eq!(boosted_class(Class::KernelRt, Class::Interactive as u8), Class::KernelRt);
    }

    #[test]
    fn weights_are_linux_and_classes_keep_their_sc3_weight() {
        assert_eq!(nice_weight(0), 1024);
        assert_eq!(nice_weight(-20), 88761);
        assert_eq!(nice_weight(19), 15);
        assert_eq!(nice_weight(-100), 88761, "clamped");
        assert_eq!(nice_weight(100), 15, "clamped");
        assert_eq!(nice_weight(5), 335);
        // Spot checks against linux-6.6/kernel/sched/core.c:11542 beyond the ends, and the table's sum.
        assert_eq!((nice_weight(-5), nice_weight(-1), nice_weight(1), nice_weight(10)), (3121, 1277, 820, 110));
        assert_eq!(NICE_WEIGHTS.iter().sum::<u64>(), 445_163, "the table differs from Linux's");
        // The class weights SC-3 used are the base nice values of the classes.
        for c in [Class::Interactive, Class::Normal, Class::Background] {
            assert_eq!(weight_of(c, Policy::Other, 0), c.weight(), "{c:?}");
        }
        assert_eq!(weight_of(Class::Normal, Policy::Batch, 5), 335, "batch keeps the nice weight");
        assert_eq!(weight_of(Class::Normal, Policy::Idle, -20), WEIGHT_IDLEPRIO);
        assert_eq!(weight_of(Class::Background, Policy::Other, 15), 15, "10 + 15 clamps at 19");
        assert_eq!(weight_of(Class::Interactive, Policy::Other, -20), 88761, "-5 - 20 clamps at -20");
        // Neighbouring nice levels differ by about 1.25 (Linux's 10 % CPU per level).
        for n in MIN_NICE..MAX_NICE {
            let r = nice_weight(n) as f64 / nice_weight(n + 1) as f64;
            assert!((1.2..1.32).contains(&r), "nice {n}: ratio {r}");
        }
    }

    #[test]
    fn policies_by_linux_number() {
        assert_eq!(Policy::from_linux(0), Some(Policy::Other));
        assert_eq!(Policy::from_linux(3), Some(Policy::Batch));
        assert_eq!(Policy::from_linux(5), Some(Policy::Idle));
        assert_eq!(Policy::from_linux(7), Some(Policy::Ext));
        assert_eq!(weight_of(Class::Normal, Policy::Ext, 0), 1024, "SCHED_EXT runs as SCHED_OTHER");
        assert_eq!((Policy::from_linux(1), Policy::from_linux(2)), (Some(Policy::Fifo), Some(Policy::Rr)), "SC-7a");
        assert!(Policy::Fifo.is_rt() && Policy::Rr.is_rt() && !Policy::Other.is_rt());
        assert_eq!(weight_of(Class::Interactive, Policy::Fifo, 19), 3121, "RT ignores nice");
        assert_eq!(weight_of(Class::Normal, Policy::Rr, -20), 1024);
        assert_eq!(weight_of(Class::Background, Policy::Idle, 0), WEIGHT_IDLEPRIO, "SCHED_IDLE keeps Linux's weight as Background");
        for p in [4, 6, 8, -1, 0x4000_0000] {
            assert_eq!(Policy::from_linux(p), None, "{p}");
        }
        assert_eq!(Policy::Idle.linux(), 5);
    }

    #[test]
    fn masks_are_trimmed_to_the_possible_cpus() {
        assert_eq!(mask_words(1), 1);
        assert_eq!(mask_words(16), 1);
        assert_eq!(mask_words(64), 1);
        assert_eq!(mask_words(65), 2);
        assert_eq!(mask_words(0), 1);
        assert_eq!(mask_trim(&[u64::MAX], 16), vec![0xFFFF]);
        assert_eq!(mask_trim(&[1 << 63], 16), vec![0], "a bit beyond the CPUs is dropped");
        assert_eq!(mask_trim(&[], 16), vec![0]);
        assert_eq!(mask_trim(&[5, 7, 9], 70), vec![5, 7 & 0x3F]);
        assert_eq!(mask_trim(&[u64::MAX], 64), vec![u64::MAX]);
        assert!(mask_has(&[0b100], 2) && !mask_has(&[0b100], 1) && !mask_has(&[0b100], 64));
        assert!(mask_has(&[0, 1], 64));
    }
}
