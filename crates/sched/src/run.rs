//! The heart of SC-3: accounting, queueing, wake-ups and the switch itself.
//!
//! A switch has two halves. `schedule` (interrupts off) accounts the running thread, picks the next one from
//! this CPU's queue and leaves the running thread in the CPU's `after` slot; `after_switch`, run by whoever
//! comes next on this CPU once hal has saved the old thread's registers, finishes the old thread's change of
//! state: back into a queue (yield, preemption), asleep (block) or into the graveyard (exit). So a run queue
//! never holds a thread whose registers are still in use, and a thread only becomes `Blocked` (wakeable into a
//! queue) once it is off its CPU.
//!
//! Thread states and who changes them:
//! - Running -> Blocking: the thread itself (`prepare_to_block`); Blocking -> Running: a waker (the thread
//!   is still on its CPU and will not sleep);
//! - Blocking -> Blocked: `after_switch` on the CPU it left; Blocked -> Runnable: the one waker that wins the
//!   compare-exchange, which then queues it;
//! - Runnable -> Running: `schedule` on the CPU that picked it; Running -> Runnable: `after_switch` (requeue);
//! - anything -> Dead: `exit`.

use crate::cpus::{self, CpuSched};
use crate::{Class, Leave, State, Thread, table};
use alloc::sync::Arc;
use core::sync::atomic::Ordering;

/// Why a thread is queued: decides its place in the fair class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    /// Back into the queue of the CPU it ran on (yield): keeps its virtual runtime.
    Requeue,
    /// Preempted: as `Requeue`, but a KernelRt thread goes before the others of its priority.
    Preempted,
    /// Woke up: placed at the CPU's virtual time plus its lag, with at most the sleeper credit as a bonus.
    Wake,
    /// Moved from another CPU (its lag was taken there) or new (lag 0).
    Migrate,
}

/// Charges the time since `t` started running to it (runtime, its user part, virtual runtime, the CPU's
/// busy/idle/user time). `t` runs on `cs` (this CPU), interrupts off.
pub(crate) fn account(cs: &CpuSched, t: &Thread, now: u64) {
    account_and_mark(cs, t, now, None);
}

/// `account`, then (SC-6) `user`: the thread enters (true) or leaves (false) user mode at `now`. One write
/// section for the readers of `Thread::cpu_times`.
pub(crate) fn account_and_mark(cs: &CpuSched, t: &Thread, now: u64, user: Option<bool>) {
    t.acct_begin();
    let start = t.exec_start.swap(now, Ordering::Relaxed);
    let d = if start == 0 { 0 } else { now.saturating_sub(start) };
    t.runtime_ns.fetch_add(d, Ordering::Relaxed);
    let was_user = t.in_user.load(Ordering::Relaxed);
    if was_user {
        t.utime_ns.fetch_add(d, Ordering::Relaxed);
    }
    if let Some(u) = user {
        t.in_user.store(u, Ordering::Relaxed);
    }
    t.acct_end();
    cs.acct_at.store(now, Ordering::Relaxed);
    if t.is_idle() {
        cs.idle_ns.fetch_add(d, Ordering::Relaxed);
        return;
    }
    cs.busy_ns.fetch_add(d, Ordering::Relaxed);
    if was_user {
        cs.user_ns.fetch_add(d, Ordering::Relaxed);
        if t.nice.load(Ordering::Relaxed) > 0 {
            cs.nice_ns.fetch_add(d, Ordering::Relaxed);
        }
    }
    if t.class() != Class::KernelRt {
        let v = t.vruntime.load(Ordering::Relaxed).saturating_add(crate::vdelta(d, t.weight.load(Ordering::Relaxed)));
        t.vruntime.store(v, Ordering::Relaxed);
        cs.curr_vruntime.store(v, Ordering::Relaxed);
        // SC-7a: its class or policy may have changed while it ran (`attr::class_changed` kicks this CPU); the
        // mirrors are only written on their own CPU, so a store here never lands on another thread's.
        cs.curr_class.store(t.class() as u8, Ordering::Relaxed);
        cs.curr_policy.store(t.policy.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

/// Queues `t` (state Runnable, not on any CPU) on `cpu` (its pinned CPU instead, if it has one; SC-6: a CPU
/// its affinity allows if `cpu` is not one) and asks that CPU to reschedule if `t` should run before what runs
/// there. Thread context (the queue may grow).
///
/// SC-6 affinity race: `attr::set_affinity` stores the new mask, then (SeqCst fence) reads `t.cpu` and looks
/// for `t` in that CPU's queue. Here `t.cpu` is stored first, then (SeqCst fence) the mask is checked again
/// **under the queue lock** before the push. Either `enforce` read the new `t.cpu` (then its queue-lock section
/// is ordered before or after ours: before, our check under the lock sees the new mask; after, it finds `t`
/// queued and moves it) or it read the old one (then the fences make our check see the new mask). So `t` never
/// stays queued on a CPU its new mask forbids. Bounded: after `ENQUEUE_TRIES` changes of mask during one
/// enqueue it is queued where it is (its next preemption point moves it, `Leave::Move`).
pub(crate) fn enqueue(t: Arc<Thread>, cpu: usize, mut why: Why) {
    let pin = t.pin.load(Ordering::Relaxed);
    let mut want = cpu;
    let mut tries = 0u32;
    let cs = loop {
        let mut cpu = if pin != crate::NO_CPU && cpus::of(pin).is_some_and(|c| c.online.load(Ordering::Acquire)) { pin } else { want };
        if pin == crate::NO_CPU && !t.may_run_on(cpu) {
            cpu = cpus::select_for(&t);
        }
        if cpus::of(cpu).is_none_or(|c| !c.online.load(Ordering::Acquire)) {
            cpu = hal::percpu::this_cpu();
        }
        // This CPU's state always exists: a queued thread is never dropped on the floor.
        let cs = cpus::of(cpu).unwrap_or_else(cpus::this);
        let cpu = cs.cpu;
        let old = t.cpu.swap(cpu, Ordering::AcqRel);
        core::sync::atomic::fence(Ordering::SeqCst);
        if matches!(why, Why::Requeue | Why::Preempted) && old != cpu {
            // Requeued on another CPU (its pin or mask changed): carry its place over as a lag.
            if let Some(o) = cpus::of(old) {
                let min = o.rq.lock().min_vruntime;
                t.lag.store(lag_of(t.vruntime.load(Ordering::Relaxed), min), Ordering::Relaxed);
            }
            why = Why::Migrate;
        }
        if old != cpu && old != crate::NO_CPU {
            t.migrations.fetch_add(1, Ordering::Relaxed);
            cs.migrations_in.fetch_add(1, Ordering::Relaxed);
        }
        let queued = {
            let mut rq = cs.rq.lock();
            if pin == crate::NO_CPU && tries < ENQUEUE_TRIES && !t.may_run_on(cpu) {
                None
            } else {
                let r = if t.class() == Class::KernelRt {
                    rq.push_rt(t.rt_prio(), why == Why::Preempted, t.id, t.clone())
                } else {
                    let lag = t.lag.load(Ordering::Relaxed);
                    let v = match why {
                        Why::Requeue | Why::Preempted => t.vruntime.load(Ordering::Relaxed),
                        // SC-7a: an Interactive thread keeps its debt like every fair thread (forgiving it on each
                        // wake-up let a run/short-sleep loop take its CPU); its wake-to-run preference is the full
                        // slice of credit here and `INTERACTIVE_WAKE_NS` in `check_preempt`/`pick_instead`.
                        Why::Wake => rq.min_vruntime.saturating_add_signed(lag.max(-(crate::sleeper_credit(t.class()) as i64))),
                        Why::Migrate => rq.min_vruntime.saturating_add_signed(lag),
                    };
                    t.vruntime.store(v, Ordering::Relaxed);
                    rq.push_fair(v, t.id, t.clone())
                };
                cs.nr_queued.store(rq.len(), Ordering::Release);
                Some(r.is_ok())
            }
        };
        match queued {
            Some(true) => break cs,
            Some(false) => {
                no_room(cs, &t);
                return;
            }
            None => {
                // The mask changed under us (a concurrent `set_affinity`): place it again.
                tries += 1;
                ENQUEUE_MASK_RETRIES.inc();
                klog::event!(Debug, "sched", "enqueue.mask_changed", id = t.id, cpu = cpu, tries = tries);
                want = cpus::select_for(&t);
            }
        }
    };
    if why != Why::Requeue && why != Why::Preempted {
        check_preempt(cs, &t);
    }
    // SC-4: a second runnable thread there needs that CPU's tick (also a requeue in `after_switch`: `schedule`
    // armed nothing while the leaving thread was not queued yet).
    crate::timers::work_queued(cs);
}

/// Placements of one `enqueue` when the affinity mask keeps changing under it.
const ENQUEUE_TRIES: u32 = 4;

/// SC-7a: how far (virtual time) an Interactive thread may be ahead of a Normal/Background one and still take
/// its CPU on a wake-up or a preemption. Beyond it the weights decide (3121 : 1024), so nothing starves.
pub(crate) const INTERACTIVE_WAKE_NS: u64 = crate::SLICE_NS;
klog::counter!(pub ENQUEUE_MASK_RETRIES, "sched.enqueue_mask_retries");

pub(crate) fn lag_of(v: u64, min: u64) -> i64 {
    (v as i128 - min as i128).clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// Asks `cs` to reschedule when `t` (just queued there) should run before the thread running there: an idle
/// CPU always; a KernelRt thread over a fair one or a lower priority; (SC-6, Linux `check_preempt_wakeup`) any
/// non-SCHED_IDLE thread over a SCHED_IDLE one, never a SCHED_BATCH/IDLE waker; an Interactive thread over a
/// Normal or Background one unless it ran more than `INTERACTIVE_WAKE_NS` of virtual time ahead of it (its
/// latency bonus, bounded so that a thread in front that keeps waking cannot starve its CPU); otherwise a fair thread whose virtual runtime is clearly behind.
pub(crate) fn check_preempt(cs: &CpuSched, t: &Thread) {
    let cc = Class::from_u8(cs.curr_class.load(Ordering::Relaxed));
    let idle_policy = crate::attr::Policy::Idle as u8;
    let (tp, cp) = (t.policy.load(Ordering::Relaxed), cs.curr_policy.load(Ordering::Relaxed));
    let preempt = cs.in_idle.load(Ordering::Acquire)
        || match t.class() {
            Class::KernelRt => cc != Class::KernelRt || t.rt_prio() > cs.curr_prio.load(Ordering::Relaxed),
            _ if cc == Class::KernelRt => false,
            _ if cp == idle_policy && tp != idle_policy => true,
            _ if !Thread::wakeup_preempts(tp) => false,
            Class::Interactive if cc > Class::Interactive => t.vruntime.load(Ordering::Relaxed) < cs.curr_vruntime.load(Ordering::Relaxed).saturating_add(INTERACTIVE_WAKE_NS),
            _ => t.vruntime.load(Ordering::Relaxed).saturating_add(crate::WAKEUP_GRAN_NS) < cs.curr_vruntime.load(Ordering::Relaxed),
        };
    if preempt {
        cpus::kick(cs, t.class() == Class::KernelRt);
    }
}

klog::counter!(pub QUEUE_NO_ROOM, "sched.queue_no_room");

/// A Runnable thread the queue of `cs` had no memory for: it waits on that CPU's lock-free wake list (no
/// allocation; its `wake_next` link is free, as it is on no list and in no queue) and is queued again at the
/// CPU's next drain. It stays listed in the table, so the drain finds it.
fn no_room(cs: &CpuSched, t: &Arc<Thread>) {
    if QUEUE_NO_ROOM.get() == 0 {
        klog::event!(Warn, "sched", "queue.no_room", id = t.id, cpu = cs.cpu; "no memory to grow a run queue; the thread waits on the wake list");
    }
    QUEUE_NO_ROOM.inc();
    push_wake(cs, t);
    cpus::kick(cs, false);
}

/// Treiber push of `t` onto the wake list of `cs` (lock- and allocation-free; any context).
fn push_wake(cs: &CpuSched, t: &Thread) {
    let mut h = cs.wake_head.load(Ordering::Acquire);
    loop {
        t.wake_next.store(h, Ordering::Relaxed);
        match cs.wake_head.compare_exchange_weak(h, t.id, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(now) => h = now,
        }
    }
}

/// Wakes `t` if it sleeps or is about to. Returns whether this call woke it. `irq`: called from a hard-interrupt
/// handler (the thread goes onto the chosen CPU's lock-free wake list instead of its queue: nothing allocates).
pub(crate) fn wake_thread(t: &Arc<Thread>, irq: bool) -> bool {
    loop {
        match t.state() {
            // Still on its CPU between `prepare_to_block` and the switch: it will not sleep.
            State::Blocking => {
                if t.cas_state(State::Blocking, State::Running) {
                    return true;
                }
            }
            State::Blocked => {
                if t.cas_state(State::Blocked, State::Runnable) {
                    break;
                }
            }
            _ => return false,
        }
    }
    t.wakeups.fetch_add(1, Ordering::Relaxed);
    crate::interactive::woken(t);
    let cpu = cpus::select_for(t);
    if irq {
        let cs = cpus::of(cpu).unwrap_or_else(cpus::this);
        // This thread is on no other list (only the winner of Blocked -> Runnable pushes it, and it stays
        // Runnable until the target drains the list).
        push_wake(cs, t);
        cs.irq_wakes.fetch_add(1, Ordering::Relaxed);
        cpus::kick(cs, t.class() == Class::KernelRt);
        crate::timers::work_queued(cs);
    } else {
        enqueue(t.clone(), cpu, Why::Wake);
    }
    true
}

/// Moves this CPU's interrupt-time wake-ups into its run queue (thread context).
pub(crate) fn drain_wakes(cs: &CpuSched) {
    let mut id = cs.wake_head.swap(0, Ordering::AcqRel);
    while id != 0 {
        let Some(t) = table::get(id) else {
            // A Runnable thread cannot leave the table; a broken chain would strand the rest.
            crate::LOST_WAKES.inc();
            return;
        };
        id = t.wake_next.swap(0, Ordering::AcqRel);
        enqueue(t, cs.cpu, Why::Wake);
    }
}

/// The next thread for a yield or a preemption of `me`: a KernelRt `me` gives way only to a higher priority (or
/// an equal one on an explicit yield: round robin); a fair `me` to any KernelRt thread, on an explicit yield to
/// the next fair one, and when preempted (slice used up, `need_resched`) only to a fair thread with less virtual
/// runtime (an Interactive one over a Normal/Background `me`: with `INTERACTIVE_WAKE_NS` of bonus), so the
/// weights decide who runs longer.
fn pick_instead(rq: &mut crate::rq::RunQueue<Arc<Thread>>, me: &Thread, how: Leave) -> Option<Arc<Thread>> {
    if me.is_idle() {
        return rq.pop_best();
    }
    let explicit = matches!(how, Leave::Yield | Leave::Move);
    if let Some(p) = rq.best_rt_prio() {
        let take = me.class() != Class::KernelRt || p > me.rt_prio() || (p == me.rt_prio() && explicit);
        return if take { rq.pop_rt() } else { None };
    }
    if me.class() == Class::KernelRt {
        return None;
    }
    let (v, class) = rq.peek_fair().map(|t| (t.vruntime.load(Ordering::Relaxed), t.class()))?;
    let bonus = if class == Class::Interactive && me.class() > Class::Interactive { INTERACTIVE_WAKE_NS } else { 0 };
    let wins = v < me.vruntime.load(Ordering::Relaxed).saturating_add(bonus);
    if explicit || wins { rq.pop_fair() } else { None }
}

/// Leaves the running thread for the next one (interrupts off: callers hold an `IrqGuard`). `Ok(true)`: a
/// switch happened and this thread runs again; `Ok(false)`: nothing to switch to (or, for a block, it was
/// woken meanwhile); `Err`: hal refused the switch and everything was rolled back (a block ends as a spurious
/// wake-up, an exit retries). No `Arc<Thread>` stays on this stack across the switch: an exiting thread's stack
/// is abandoned, and the leaving thread travels in the CPU's `after` slot instead.
pub(crate) fn schedule(how: Leave) -> Result<bool, hal::switch::SwitchError> {
    let cs = cpus::this();
    let Some(me) = crate::current_thread() else { return Ok(false) };
    // SC-6: a thread whose affinity no longer allows this CPU leaves it even with nothing else to run (its
    // requeue in `after_switch` goes to an allowed CPU).
    let how = if matches!(how, Leave::Yield | Leave::Preempt) && !me.is_idle() && !me.may_run_on(cs.cpu) { Leave::Move } else { how };
    let now = time::now_ns();
    drain_wakes(cs);
    account(cs, &me, now);
    cs.need_resched.store(false, Ordering::Relaxed);
    match how {
        Leave::Block if me.state() != State::Blocking => return Ok(false),
        Leave::Exit => me.set_state(State::Dead),
        _ => {}
    }
    let picked = {
        let mut rq = cs.rq.lock();
        let curr = (!me.is_idle() && me.class() != Class::KernelRt && how != Leave::Exit && how != Leave::Block).then(|| me.vruntime.load(Ordering::Relaxed));
        rq.update_min(curr);
        let n = match how {
            Leave::Yield | Leave::Preempt | Leave::Idle | Leave::Move => pick_instead(&mut rq, &me, how),
            Leave::Block | Leave::Exit => rq.pop_best(),
        };
        cs.nr_queued.store(rq.len(), Ordering::Release);
        n
    };
    let next = match picked {
        Some(t) => t,
        None if matches!(how, Leave::Block | Leave::Exit | Leave::Move) => match cs.idle.get() {
            Some(i) if !core::ptr::eq(&**i, &*me) => i.clone(),
            _ => {
                // No idle thread (its creation failed): a block returns as a spurious wake-up.
                if how == Leave::Exit {
                    return Ok(false);
                }
                me.cas_state(State::Blocking, State::Running);
                return Ok(false);
            }
        },
        None => {
            cs.set_curr(&me, now);
            tick_update(cs);
            return Ok(false);
        }
    };
    let next_id = next.id;
    // SC-6: `me` stops running here (readers of its CPU time stop adding the open slice) and counts the switch
    // as Linux does: voluntary when it blocks, involuntary when it was preempted or yielded while runnable.
    me.set_exec_start(0);
    match how {
        Leave::Block => {
            me.nvcsw.fetch_add(1, Ordering::Relaxed);
        }
        Leave::Yield | Leave::Preempt | Leave::Move if !me.is_idle() => {
            me.nivcsw.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
    next.set_state(State::Running);
    next.set_exec_start(now);
    crate::interactive::switched_in(&next, now);
    next.cpu.store(cs.cpu, Ordering::Relaxed);
    cs.set_curr(&next, now);
    cs.switches.fetch_add(1, Ordering::Relaxed);
    cs.beat();
    tick_update(cs);
    klog::trace::switch(me.id as u64, next_id as u64);
    *cs.after.lock() = Some((me, how));
    let irq = ksync::take_irq_state();
    let r = hal::switch::switch_to(next, how == Leave::Exit);
    ksync::put_irq_state(irq);
    match r {
        Ok(()) => {
            after_switch();
            Ok(true)
        }
        Err(e) => {
            roll_back(cs, next_id, now);
            if crate::EXPECTED_SWITCH_FAIL.load(core::sync::atomic::Ordering::Relaxed) == next_id as u64 {
                // sc.sched.rollback's own unstarted thread: refused on purpose, not an error of the system.
                klog::event!(Debug, "sched", "switch.failed", error = alloc::format!("{e:?}"), to = next_id; "rolled back (test target)");
            } else {
                klog::event!(Error, "sched", "switch.failed", error = alloc::format!("{e:?}"), to = next_id; "rolled back");
            }
            Err(e)
        }
    }
}

/// SC-4: arms this CPU's tick for what runs and waits here now, unless the caller holds a spin lock (arming
/// may take `time.timers`; the next preemption point arms it then).
fn tick_update(cs: &CpuSched) {
    if ksync::preempt_count() == 0 {
        crate::timers::tick_update(cs);
    }
}

/// Undoes `schedule` after hal refused the switch (nothing ran in between: interrupts are off, same CPU).
fn roll_back(cs: &CpuSched, next_id: u64, now: u64) {
    let Some((me, how)) = cs.after.lock().take() else { return };
    if let Some(t) = table::get(next_id)
        && t.state() == State::Running
        && !t.ctx.is_on_cpu()
    {
        t.set_state(State::Runnable);
        t.set_exec_start(0);
        if !t.is_idle() && !t.ctx.is_dead() {
            let r = {
                let mut rq = cs.rq.lock();
                // It came out of this queue a moment ago, so the room is there; `no_room` covers the rest.
                let r = if t.class() == Class::KernelRt {
                    rq.push_rt(t.rt_prio(), true, t.id, t.clone())
                } else {
                    rq.push_fair(t.vruntime.load(Ordering::Relaxed), t.id, t.clone())
                };
                cs.nr_queued.store(rq.len(), Ordering::Release);
                r
            };
            if r.is_err() {
                no_room(cs, &t);
            }
        }
    }
    match how {
        // Still current and listed, still Dead: `exit` retries.
        Leave::Exit => {}
        Leave::Block => {
            me.cas_state(State::Blocking, State::Running);
        }
        Leave::Yield | Leave::Preempt | Leave::Idle | Leave::Move => me.set_state(State::Running),
    }
    me.set_exec_start(now);
    match how {
        Leave::Block => {
            me.nvcsw.fetch_sub(1, Ordering::Relaxed);
        }
        Leave::Yield | Leave::Preempt | Leave::Move if !me.is_idle() => {
            me.nivcsw.fetch_sub(1, Ordering::Relaxed);
        }
        _ => {}
    }
    cs.set_curr(&me, now);
}

/// The second half of a switch, on the CPU it happened on, run by the thread that came next (after
/// `switch_to` returns, or first thing in a new thread). Interrupts are off.
pub(crate) fn after_switch() {
    let cs = cpus::this();
    let prev = cs.after.lock().take();
    if let Some((p, how)) = prev {
        match how {
            Leave::Idle => p.set_state(State::Runnable),
            Leave::Yield | Leave::Preempt | Leave::Move => {
                if p.is_idle() {
                    p.set_state(State::Runnable);
                } else {
                    // Blocking counts as running: a waiter whose condition came true before `block` yields.
                    if !p.cas_state(State::Running, State::Runnable) {
                        p.cas_state(State::Blocking, State::Runnable);
                        // A waker turning Blocking into Running in between: try once more.
                        p.cas_state(State::Running, State::Runnable);
                    }
                    enqueue(p, cs.cpu, if how == Leave::Preempt { Why::Preempted } else { Why::Requeue });
                }
            }
            Leave::Block => {
                let min = cs.rq.lock().min_vruntime;
                p.lag.store(lag_of(p.vruntime.load(Ordering::Relaxed), min), Ordering::Relaxed);
                if !p.cas_state(State::Blocking, State::Blocked) {
                    // Woken after `schedule` decided to switch: it is Running again, so it goes back in line.
                    p.set_state(State::Runnable);
                    enqueue(p, cs.cpu, Why::Requeue);
                }
            }
            Leave::Exit => {
                drop(table::remove(p.id));
                crate::reap::bury(p);
                crate::EXITING.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }
    if hal::switch::retired_pending() || crate::reap::graves_waiting() {
        crate::reap::kick();
    }
}
