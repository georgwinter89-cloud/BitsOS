//! Moving work between CPUs (SC-3): idle CPUs steal from the busiest queue, and busy CPUs push a waiting thread
//! to an idle (or much less busy) CPU every `BALANCE_NS` at their preemption points (halted CPUs do not steal,
//! so pushing is what wakes them for work that piled up elsewhere). A CPU whose thread reaches no preemption
//! point is balanced from outside: the idle CPU on poll duty steals from it, and while every CPU is busy the
//! poll thread pushes for CPUs whose housekeeping went stale (`push_all`). With CP-5's timer, `timer_tick` can
//! drive it. Never two queue locks at once: a thread is taken out under the source's lock and queued under the
//! target's.

use crate::cpus::{self, CpuSched};
use crate::run::{self, Why};
use crate::{Thread, place};
use alloc::sync::Arc;
use core::sync::atomic::Ordering;

klog::counter!(pub STEALS, "sched.steals");
klog::counter!(pub PUSHES, "sched.pushes");
klog::counter!(pub MISFITS, "sched.misfit_moves");

/// Takes one movable thread out of `from`'s queue (keeping its place as a lag), or None.
fn take_from(from: &CpuSched, to: usize) -> Option<Arc<Thread>> {
    let mut rq = from.rq.lock();
    // Its pin and (SC-6) its affinity mask must allow the target.
    let movable = |t: &Arc<Thread>| t.may_run_on(to);
    let (t, taken) = rq.take_movable(movable)?;
    if let crate::rq::Taken::Fair(v) = taken {
        t.lag.store(run::lag_of(v, rq.min_vruntime), Ordering::Relaxed);
    }
    from.nr_queued.store(rq.len(), Ordering::Release);
    Some(t)
}

/// An idle CPU looks for work elsewhere. True if it queued a thread on itself.
pub(crate) fn try_steal(me: &CpuSched) -> bool {
    let waiting = |c: usize| cpus::of(c).filter(|s| s.online.load(Ordering::Acquire)).map_or(0, |s| s.nr_queued.load(Ordering::Acquire));
    let Some(v) = place::steal_victim(cpus::count(), &cpus::view_of, &waiting, me.cpu) else { return false };
    let Some(from) = cpus::of(v) else { return false };
    let Some(t) = take_from(from, me.cpu) else { return false };
    run::enqueue(t, me.cpu, Why::Migrate);
    me.steals.fetch_add(1, Ordering::Relaxed);
    STEALS.inc();
    true
}

/// A busy CPU gives one waiting thread away if another CPU is idle or runs at least two fewer threads.
pub(crate) fn push(cs: &CpuSched, now: u64) {
    if cs.nr_queued.load(Ordering::Acquire) == 0 || now.saturating_sub(cs.last_balance.load(Ordering::Relaxed)) < crate::BALANCE_NS {
        return;
    }
    cs.last_balance.store(now, Ordering::Relaxed);
    let Some(to) = place::push_target(cpus::count(), &cpus::view_of, cs.cpu) else { return };
    let Some(t) = take_from(cs, to) else { return };
    run::enqueue(t, to, Why::Migrate);
    cs.pushes.fetch_add(1, Ordering::Relaxed);
    PUSHES.inc();
}

/// One balancing pass on behalf of busy CPUs whose thread reaches no preemption point (no housekeeping there for
/// `STALE_NS`): the same rate-limited `push` they would do themselves. From the poll thread, which runs when no
/// CPU is idle. CPUs that do their own housekeeping are left alone (no extra migrations).
pub(crate) fn push_all(now: u64) {
    for cs in cpus::online() {
        let stale = now.saturating_sub(cs.last_hk.load(Ordering::Relaxed)) > STALE_NS;
        if stale && !cs.in_idle.load(Ordering::Acquire) {
            push(cs, now);
        }
    }
}

const STALE_NS: u64 = 4 * crate::BALANCE_NS;

/// GP-CPU2: Linux's misfit pull on a hybrid CPU, from the poll thread: a thread that has run without a break on a
/// slower core and no longer fits it (or is Interactive) is asked to move to an idle, clearly faster core
/// (`hybrid::misfit`, `place::misfit_target`). The move happens at its next preemption point, which the
/// reschedule IPI makes immediate (`run::schedule` turns it into `Leave::Move`). Each idle core takes one thread per
/// pass. Nothing on a uniform CPU.
pub(crate) fn pull_misfits(now: u64) {
    if !cpus::hybrid() || now.saturating_sub(LAST_MISFIT.load(Ordering::Relaxed)) < crate::BALANCE_NS {
        return;
    }
    LAST_MISFIT.store(now, Ordering::Relaxed);
    let mut claimed = [crate::NO_CPU; 8];
    let mut k = 0;
    for cs in cpus::online() {
        if k == claimed.len() {
            break;
        }
        if cs.in_idle.load(Ordering::Acquire) || cs.curr_pinned.load(Ordering::Relaxed) || cs.misfit_id.load(Ordering::Acquire) != 0 {
            continue;
        }
        let class = crate::Class::from_u8(cs.curr_class.load(Ordering::Relaxed));
        let run = now.saturating_sub(cs.curr_start.load(Ordering::Relaxed));
        let view = cs.view();
        if !crate::hybrid::misfit(class, cs.curr_util.load(Ordering::Relaxed), run, view.capacity) {
            continue;
        }
        let free = |c: usize| if claimed.contains(&c) { None } else { cpus::view_of(c) };
        let Some(to) = place::misfit_target(cpus::count(), &free, cs.cpu) else { continue };
        let id = cs.curr_id.load(Ordering::Relaxed);
        cs.misfit_to.store(to, Ordering::Release);
        cs.misfit_id.store(id, Ordering::Release);
        claimed[k] = to;
        k += 1;
        MISFITS.inc();
        cpus::kick(cs, true);
    }
}

static LAST_MISFIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
