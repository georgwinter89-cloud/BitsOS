//! SC-4: the scheduler's timers on `time::HrTimer`, the tick, and the liveness records for RB-3's watchdog
//! (spec/core.md §7.2, §9.2, §10.1).
//!
//! - **Sleep timers**: one `HrTimer` per CPU, armed at the earliest deadline of the sleepers that went to sleep on
//!   that CPU (`sleepers`). Its callback wakes every sleeper that is due (on any CPU) and arms itself at its
//!   CPU's next deadline. `sleep_ns` and every timed wait (`WaitQueue::wait_until_timeout`,
//!   `Completion::wait_timeout`, `JoinHandle::join_timeout`) end through them, so a timed-out waiter is woken
//!   when its deadline passes, not when some CPU happens to look. INT-A2a2: there used to be one sleep timer
//!   for the whole system, in the queue of whichever CPU armed it last; while that CPU had interrupts off
//!   (`rb.reload.*stuck_ap*` model a stuck CPU for seconds) no timed wait on any other CPU ended, and an idle
//!   CPU halted tickless with its sleeper overdue. Now a CPU with interrupts off delays only its own sleepers,
//!   and busy CPUs still catch those at their preemption points (`fallback`), as with Linux's per-CPU
//!   `hrtimer_sleeper`s.
//! - **Tick** (tickless): every CPU has an `HrTimer` that is armed only while the CPU runs a fair thread and at
//!   least one more waits in its queue ("more than one runnable"); it fires at the end of the running thread's
//!   slice and asks for a switch. An idle CPU, or one with a single runnable thread, has no tick: an armed tick
//!   that is no longer wanted fires once more and is not re-armed (cheaper than cancelling on every switch).
//! - **Heartbeat** per CPU, bumped on every switch, every idle-loop pass (idle return), every preemption point,
//!   every reschedule IPI and every timer tick; and the **idle-entry record** (when the CPU last went idle and
//!   whether it waits with interrupts on). RB-3's watchdog: a CPU is alive when its heartbeat moved; when it
//!   did not (a halted, tickless CPU, or one busy thread without preemption points), `probe_cpu` sends a
//!   reschedule IPI whose handler bumps the heartbeat, and only a probe that stays unanswered marks it stuck.
//!   The idle record is the CPU's own claim and only says why the heartbeat stands still.
//! - **Tick on remote work**: whoever queues work on a CPU (`work_queued`) arms that CPU's tick when it now has
//!   more than one runnable thread, so a thread placed behind a busy one does not wait for that CPU's own
//!   preemption point.
//!
//! Since CP-5 HrTimers fire from each CPU's LAPIC timer interrupt (`time::interrupt_driven()`), the tick on its
//! own CPU, and the interrupt exit preempts (`tick_preempts`). While timers are still polled (early boot,
//! `hrtimer=poll`, a CPU without a live LAPIC timer) `time`'s poller fires them from the idle CPU on poll duty or
//! the `poll` thread, a tick fires on whichever CPU polls (it kicks its CPU by IPI), and busy threads'
//! preemption points wake overdue sleepers as a fallback (`fallback`).

use crate::cpus::{self, CpuSched};
use crate::{Class, SLICE_NS, sleepers};
use core::sync::atomic::{AtomicBool, Ordering};
use time::HrTimer;

static READY: AtomicBool = AtomicBool::new(false);

klog::counter!(pub SLEEP_FIRED, "sched.sleep_timer_fired");
klog::counter!(pub SLEEP_ARMED, "sched.sleep_timer_armed");
klog::counter!(pub FALLBACK_EXPIRES, "sched.sleep_fallback_expires");
klog::counter!(pub TICKS_FIRED, "sched.ticks_fired");
klog::counter!(pub TICKS_ARMED, "sched.ticks_armed");
klog::counter!(pub TICK_KICKS, "sched.tick_kicks");
klog::counter!(pub PROBES, "sched.heartbeat_probes");
klog::counter!(pub TICK_IPIS, "sched.tick_ipis");
klog::counter!(pub SLEEP_CONTENDED, "sched.sleep_timer_contended");

/// A sleeper overdue by this much is woken at a busy thread's preemption point instead of by the timer (the
/// timer may be polled (`hrtimer=poll`, early boot); with every CPU busy nobody polls it in time).
pub const FALLBACK_SLACK_NS: u64 = 100_000;

/// A sleep-timer pass that could not get the sleeper lock tries again this much later at the earliest, so a
/// contended lock cannot turn into an interrupt storm once CP-5 fires the timer from the LAPIC interrupt.
pub const CONTENDED_RETRY_NS: u64 = 5_000;

/// Whether the tick can preempt the thread it slices: it fires on its own CPU in interrupt context and the
/// interrupt exit acts on `need_resched` (`irq_exit_preempt`): true once every CPU's LAPIC timer runs the
/// hrtimers (CP-5, `time::interrupt_driven()`).
pub fn tick_preempts() -> bool {
    time::interrupt_driven()
}

/// Creates the sleep timer and this CPU's tick (`sched::init`, thread context).
pub(crate) fn init(cs: &CpuSched) {
    init_cpu(cs);
    READY.store(true, Ordering::Release);
    klog::event!(Info, "sched", "timers.ready", sleep = "hrtimer_per_cpu", tick = "hrtimer_when_busy", fired_by = "time (LAPIC timer once live, polled before)",
        fallback_slack_us = FALLBACK_SLACK_NS / 1000, slice_ms = SLICE_NS / 1_000_000);
}

/// A CPU's tick and sleep timers (BSP at `init`, an AP when it joins).
pub(crate) fn init_cpu(cs: &CpuSched) {
    cs.tick.get_or_init(|| HrTimer::new(tick_fired, cs.cpu));
    cs.sleep.get_or_init(|| HrTimer::new(sleep_fired, cs.cpu));
}

// ---------------------------------------------------------------------------------------------------------
// Sleep timer

/// A deadline `d` was added to `sleepers` for `cpu`, the CPU this runs on (interrupts off since the `add`, so
/// both name the same CPU): its sleep timer must fire no later than `d`.
pub(crate) fn sleep_added(cpu: usize, d: u64) {
    let Some(t) = cpus::of(cpu).and_then(|cs| cs.sleep.get()) else { return };
    if t.deadline().is_some_and(|at| at <= d) {
        return;
    }
    SLEEP_ARMED.inc();
    t.arm(d);
}

/// Arms `cpu`'s sleep timer at the earliest deadline it covers, not earlier than `floor` (a retry after a
/// contended pass); a timer left armed at a deadline nobody waits for any more fires once and comes back here.
fn sync_sleep_timer(cpu: usize, floor: u64) {
    let Some(t) = cpus::of(cpu).and_then(|cs| cs.sleep.get()) else { return };
    let next = match sleepers::next_of(cpu) {
        Some(u64::MAX) => return,
        Some(n) => n.max(floor),
        // The sleeper lock stayed busy: look again a little later.
        None => time::now_ns().saturating_add(CONTENDED_RETRY_NS).max(floor),
    };
    if t.deadline() == Some(next) {
        return;
    }
    SLEEP_ARMED.inc();
    t.arm(next);
    // A sleeper of this CPU may have added an earlier deadline between the look and the arm (possible only
    // while the callback runs off this CPU: the poller before CP-5) and found the old arming early enough;
    // the arm above may have moved the timer past it.
    if let Some(n) = sleepers::next_of(cpu)
        && n.max(floor) < next
    {
        t.arm(n.max(floor));
    }
}

/// A sleep timer's callback (`cpu`'s; from its LAPIC interrupt, or the `time.timers` poller before CP-5).
fn sleep_fired(cpu: usize) {
    SLEEP_FIRED.inc();
    let now = time::now_ns();
    if sleepers::expire(now, hal::percpu::in_hard_irq()).contended {
        // The deadline is already past: re-arming there would fire again at once while the lock is held.
        SLEEP_CONTENDED.inc();
        sync_sleep_timer(cpu, now.saturating_add(CONTENDED_RETRY_NS));
    } else {
        sync_sleep_timer(cpu, 0);
    }
}

/// Wakes sleepers the timer left overdue (busy threads' preemption points; one atomic load when none is).
/// Counted (and reported once) only when it really woke a sleeper the timer had missed.
pub(crate) fn fallback(now: u64) {
    let next = sleepers::next();
    if now < next.saturating_add(FALLBACK_SLACK_NS) {
        return;
    }
    let r = sleepers::expire(now, false);
    if r.woken > 0 {
        if FALLBACK_EXPIRES.get() == 0 {
            klog::event!(Info, "sched", "timer.fallback", overdue_us = now.saturating_sub(next) / 1000, woken = r.woken;
                "sleepers woken at a preemption point: the sleep timer was late (polled hrtimers, or an interrupt held off)");
        }
        FALLBACK_EXPIRES.inc();
    }
    let _irq = hal::cpu::IrqGuard::new();
    sync_sleep_timer(hal::percpu::this_cpu(), 0);
}

// ---------------------------------------------------------------------------------------------------------
// Tick

/// When the CPU described needs its next tick (None: tickless). Pure (host-tested).
pub fn tick_deadline(in_idle: bool, queued: usize, running: Class, run_start_ns: u64) -> Option<u64> {
    // KernelRt is never sliced (strict priority, FIFO); an idle CPU or a lone thread has nobody to give way to.
    if in_idle || queued == 0 || running == Class::KernelRt {
        return None;
    }
    Some(run_start_ns.saturating_add(SLICE_NS))
}

fn deadline_of(cs: &CpuSched) -> Option<u64> {
    tick_deadline(
        cs.in_idle.load(Ordering::Acquire),
        cs.nr_queued.load(Ordering::Acquire),
        Class::from_u8(cs.curr_class.load(Ordering::Relaxed)),
        cs.curr_start.load(Ordering::Relaxed),
    )
}

/// Arms this CPU's tick if it now has more than one runnable thread (its own `schedule`, preemption points;
/// no spin lock held: arming may take `time.timers`). An already armed tick is moved with one store.
pub(crate) fn tick_update(cs: &CpuSched) {
    let Some(t) = cs.tick.get() else { return };
    let Some(d) = deadline_of(cs) else { return };
    if t.is_armed() && cs.tick_at.load(Ordering::Relaxed) == d {
        return;
    }
    cs.tick_at.store(d, Ordering::Relaxed);
    TICKS_ARMED.inc();
    t.arm(d);
}

/// Whether `cs` has more than one runnable thread (queued, or waiting on its interrupt-time wake list) under a
/// fair running thread, but no armed tick.
fn tick_missing(cs: &CpuSched) -> bool {
    !cs.in_idle.load(Ordering::Acquire)
        && Class::from_u8(cs.curr_class.load(Ordering::Relaxed)) != Class::KernelRt
        && (cs.nr_queued.load(Ordering::Acquire) > 0 || cs.wake_head.load(Ordering::Acquire) != 0)
        && !cs.tick.get().is_some_and(|t| t.is_armed())
}

/// Work was placed on `cs` (its queue or its wake list; any CPU, any context): make sure its tick comes, since
/// the running thread there may reach no preemption point for a long time (and before this, nothing but that
/// CPU's own `schedule`/preemption points armed it). With no spin lock held (thread context) the tick is armed
/// right here, for any CPU. Otherwise a remote CPU gets a reschedule IPI: CP-5's interrupt exit there arms it
/// (`irq_exit_preempt`); this CPU arms it at its next preemption point or the `schedule` already under way.
pub(crate) fn work_queued(cs: &CpuSched) {
    if !tick_missing(cs) {
        return;
    }
    if ksync::preempt_count() == 0 && !hal::percpu::in_hard_irq() {
        tick_update(cs);
        if !tick_missing(cs) {
            return;
        }
    }
    if cs.cpu != hal::percpu::this_cpu() {
        TICK_IPIS.inc();
        cpus::send_resched(cs.cpu);
    }
}

/// The tick's callback (`arg` = its CPU). The slice is over while others wait: ask that CPU to switch (the
/// reschedule IPI when it is another CPU; CP-5's interrupt exit preempts on this one) and tick again a slice
/// later; not over yet (the CPU switched meanwhile): re-arm at the new end; not wanted any more: stop.
fn tick_fired(cpu: usize) {
    TICKS_FIRED.inc();
    let Some(cs) = cpus::of(cpu) else { return };
    let Some(t) = cs.tick.get() else { return };
    if cpu == hal::percpu::this_cpu() {
        cs.beat();
    }
    let Some(d) = deadline_of(cs) else { return };
    let now = time::now_ns();
    let next = if now >= d {
        TICK_KICKS.inc();
        cpus::kick(cs, true);
        now.saturating_add(SLICE_NS)
    } else {
        d
    };
    cs.tick_at.store(next, Ordering::Relaxed);
    t.arm(next);
}

/// Whether `cpu`'s tick is armed now (tests, MCP `sched`).
pub fn tick_armed(cpu: usize) -> bool {
    cpus::of(cpu).and_then(|c| c.tick.get()).is_some_and(|t| t.is_armed())
}

// ---------------------------------------------------------------------------------------------------------
// Heartbeat and the idle-entry record (RB-3)

/// When a CPU last went idle and whether it waits there with interrupts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdleEntry {
    /// Monotonic time of the last idle entry (start of an idle period, or a halt).
    pub at_ns: u64,
    /// IF was 1 at that entry (a halt is always `sti; hlt`).
    pub interrupts_on: bool,
    /// The CPU runs its idle thread now.
    pub idle_now: bool,
    /// It sits in `hlt` now.
    pub halted_now: bool,
}

/// Packs (time, IF) into one word so a reader never sees the time of one entry with the IF of another.
pub fn pack_idle(at_ns: u64, interrupts_on: bool) -> u64 {
    (at_ns.min(u64::MAX >> 1) << 1) | u64::from(interrupts_on)
}

/// The inverse of `pack_idle` (0: never idle).
pub fn unpack_idle(w: u64) -> Option<(u64, bool)> {
    (w != 0).then_some((w >> 1, w & 1 == 1))
}

/// The idle-entry record of `cpu` (None: no such CPU, or it never was idle). The CPU's own claim, with no age
/// limit: a hint for the watchdog (why the heartbeat stands still), never proof of life. NMI-safe (loads).
pub fn last_idle_entry(cpu: usize) -> Option<IdleEntry> {
    let cs = cpus::of(cpu)?;
    let (at_ns, interrupts_on) = unpack_idle(cs.idle_entry.load(Ordering::Acquire))?;
    Some(IdleEntry { at_ns, interrupts_on, idle_now: cs.in_idle.load(Ordering::Acquire), halted_now: cs.halted.load(Ordering::Acquire) })
}

/// The heartbeat of `cpu` (None: no such CPU). Moves on every switch, idle return, preemption point,
/// reschedule IPI and timer tick there; a CPU halted with interrupts on keeps it still: it is judged alive only
/// when `probe_cpu` moves it (its `last_idle_entry` alone proves nothing). NMI-safe (one load).
pub fn heartbeat(cpu: usize) -> Option<u64> {
    cpus::of(cpu).map(|c| c.heartbeat.load(Ordering::Acquire))
}

/// Sends `cpu` a reschedule IPI; its handler bumps the heartbeat, so a CPU that takes interrupts proves it is
/// alive even while one thread keeps it busy. False for this CPU, an offline CPU or no vector.
///
/// IRQ-safe, **not NMI-safe**: an xAPIC ICR write is two stores, and an NMI between them on a CPU already
/// sending an IPI would retarget or corrupt that IPI. Call it from thread or IRQ context only; from an NMI only
/// `heartbeat` and `last_idle_entry` may be read.
///
/// For RB-3: a halted CPU's heartbeat stands still (tickless) and its idle record is only its own claim, so a
/// halted CPU counts as alive only once a probe moved its heartbeat within the watchdog's timeout.
pub fn probe_cpu(cpu: usize) -> bool {
    if cpu == hal::percpu::this_cpu() || cpus::resched_vector() == 0 || !cpus::of(cpu).is_some_and(|c| c.online.load(Ordering::Acquire)) {
        return false;
    }
    PROBES.inc();
    cpus::send_resched(cpu);
    true
}

/// Whether the timers are set up (`init` ran).
pub fn ready() -> bool {
    READY.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_only_with_more_than_one_runnable() {
        assert_eq!(tick_deadline(true, 3, Class::Normal, 100), None, "idle");
        assert_eq!(tick_deadline(false, 0, Class::Normal, 100), None, "one runnable");
        assert_eq!(tick_deadline(false, 2, Class::KernelRt, 100), None, "KernelRt is not sliced");
        assert_eq!(tick_deadline(false, 1, Class::Normal, 100), Some(100 + SLICE_NS));
        assert_eq!(tick_deadline(false, 1, Class::Background, u64::MAX), Some(u64::MAX));
    }

    #[test]
    fn idle_record_round_trip() {
        assert_eq!(unpack_idle(0), None);
        assert_eq!(unpack_idle(pack_idle(12345, true)), Some((12345, true)));
        assert_eq!(unpack_idle(pack_idle(12345, false)), Some((12345, false)));
        assert_eq!(unpack_idle(pack_idle(0, true)), Some((0, true)));
        assert_eq!(unpack_idle(pack_idle(u64::MAX, true)), Some((u64::MAX >> 1, true)));
    }
}
