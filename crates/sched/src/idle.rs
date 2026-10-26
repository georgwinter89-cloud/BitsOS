//! Idle CPUs, poll duty and housekeeping (SC-3).
//!
//! Every CPU has an idle thread (the BSP's is spawned at `init`, an AP's is the code CP-4 hands over on its boot
//! stack). It is never queued: a CPU runs it when its queue is empty. In it the CPU drains interrupt-time
//! wake-ups, steals work from busy CPUs, and otherwise either keeps **poll duty** or halts:
//!
//! - One idle CPU at a time holds poll duty: it spins, runs the registered pollers (devices without interrupts,
//!   the hrtimer queue, which fires the scheduler's sleep timer and ticks: `timers`) and reports lock-order
//!   violations. When it finds work it gives the duty up and wakes a halted CPU to take it over. Since CP-5 the
//!   duty exists only while hrtimers are polled (`time::interrupt_driven()` false: early boot, `hrtimer=poll`, a
//!   CPU without a live LAPIC timer); with every CPU's LAPIC timer live the duty goes back to the poll thread and
//!   every idle CPU halts.
//! - Every other idle CPU halts (`sti; hlt`) until an interrupt: a reschedule IPI when work is placed on it, or
//!   the reaper's sync IPI.
//! - With no CPU idle (all busy), the KernelRt `poll` thread takes over polling every millisecond, and busy
//!   threads wake overdue sleepers (`timers::fallback`), arm their tick and push work around at their
//!   preemption points (`housekeeping`).
//!
//! SC-4 (RB-3): every idle-loop pass bumps the CPU's heartbeat; the start of an idle period and every halt
//! write the idle-entry record (time, IF).
//!
//! Pollers therefore run on one CPU at a time, from thread context, as before SC-3.

use crate::cpus::{self, CpuSched};
use crate::{Leave, balance, run, timers};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use ksync::WaitQueue;

const NO_DUTY: usize = usize::MAX;
static DUTY: AtomicUsize = AtomicUsize::new(NO_DUTY);
static POLL_BUSY: AtomicBool = AtomicBool::new(false);
static LAST_POLL: AtomicU64 = AtomicU64::new(0);
/// The poll thread waits here while some idle CPU holds the duty.
static POLL_WQ: WaitQueue = WaitQueue::new();

klog::counter!(pub HALTS, "sched.halts");
klog::counter!(pub DUTY_HANDOFFS, "sched.poll_duty_handoffs");

/// Which CPU holds poll duty (None: no idle CPU, the poll thread polls).
pub fn duty_cpu() -> Option<usize> {
    let d = DUTY.load(Ordering::Acquire);
    (d != NO_DUTY).then_some(d)
}

/// Runs the pollers once unless another CPU is running them; also reports lock-order violations.
pub(crate) fn poll_pass(now: u64) {
    if POLL_BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    hal::vectors::poll_all();
    LAST_POLL.store(now, Ordering::Relaxed);
    POLL_BUSY.store(false, Ordering::Release);
    crate::report_lock_violations();
    cpus::rt_watchdog(now);
}

fn take_duty(cpu: usize) -> bool {
    DUTY.compare_exchange(NO_DUTY, cpu, Ordering::AcqRel, Ordering::Acquire).is_ok()
}

/// Gives the duty up (this CPU found work) and hands it to a halted CPU, or to the poll thread.
fn release_duty(cpu: usize) {
    if DUTY.compare_exchange(cpu, NO_DUTY, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return;
    }
    DUTY_HANDOFFS.inc();
    if let Some(h) = cpus::online().find(|c| c.cpu != cpu && c.halted.load(Ordering::Acquire)) {
        cpus::send_resched(h.cpu);
    } else {
        POLL_WQ.wake_one();
    }
}

/// Halts this idle CPU until an interrupt, unless work or the free duty arrived meanwhile.
fn halt(cs: &CpuSched) {
    hal::cpu::disable_interrupts();
    cs.halted.store(true, Ordering::SeqCst);
    // Pairs with `cpus::kick`: either it sees `halted` and sends the IPI, or this sees its flag.
    // CP-5: with every CPU's timers on its LAPIC timer nobody needs to keep the duty (the poll thread polls).
    if cs.has_work() || (DUTY.load(Ordering::SeqCst) == NO_DUTY && !time::interrupt_driven()) {
        cs.halted.store(false, Ordering::SeqCst);
        hal::cpu::enable_interrupts();
        return;
    }
    HALTS.inc();
    // A halt is an idle entry with interrupts on (`sti; hlt` below): what RB-3's watchdog counts as alive.
    cs.record_idle(time::now_ns(), true);
    // `sti; hlt`: an interrupt arriving after the checks above still ends the halt (the sti shadow).
    hal::cpu::enable_and_halt();
    cs.halted.store(false, Ordering::SeqCst);
}

/// The idle thread of `cpu` (interrupts on; never returns).
pub(crate) fn idle_loop(cpu: usize) -> ! {
    let Some(cs) = cpus::of(cpu) else { hal::panic::halt_forever() };
    // Start of an idle period (first pass, and after every run of other threads): the idle-entry record.
    let mut entered = false;
    loop {
        hal::switch::quiescent_point();
        crate::catch_up_interrupts();
        cs.beat();
        let now = time::now_ns();
        if !entered {
            cs.record_idle(now, hal::cpu::interrupts_enabled());
            entered = true;
        }
        let polled = !time::interrupt_driven();
        let mut duty = DUTY.load(Ordering::Acquire) == cpu;
        if duty && !polled {
            // CP-5: timers fire by interrupt now; the duty goes back to the poll thread, which polls every POLL_NS.
            if DUTY.compare_exchange(cpu, NO_DUTY, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                DUTY_HANDOFFS.inc();
                POLL_WQ.wake_one();
            }
            duty = false;
        }
        if duty {
            poll_pass(now);
        }
        if cs.has_work() || balance::try_steal(cs) {
            release_duty(cpu);
            let _irq = hal::cpu::IrqGuard::new();
            // A refused switch was rolled back; the idle thread simply looks again.
            if let Ok(true) = run::schedule(Leave::Idle) {
                entered = false;
            }
            continue;
        }
        if duty || (polled && take_duty(cpu)) {
            hal::cpu::spin_hint();
            continue;
        }
        halt(cs);
    }
}

/// Light work for busy CPUs at their preemption points (rate-limited): the heartbeat, overdue sleepers, the
/// tick when a second thread was queued here meanwhile, pushing a waiting thread to an idle CPU. Thread
/// context with no spin lock held.
pub(crate) fn housekeeping(cs: &CpuSched, now: u64) {
    cs.beat();
    timers::fallback(now);
    timers::tick_update(cs);
    if now.saturating_sub(cs.last_hk.load(Ordering::Relaxed)) < crate::HOUSEKEEP_NS {
        return;
    }
    cs.last_hk.store(now, Ordering::Relaxed);
    balance::push(cs, now);
}

/// The poll thread (KernelRt): polls every millisecond while no idle CPU holds poll duty.
pub(crate) fn poll_loop() {
    loop {
        crate::catch_up_interrupts();
        if duty_cpu().is_some() {
            // B4: bounded wait even though the duty CPU will wake us when it gives the duty up.
            POLL_WQ.wait_until_timeout(|| DUTY.load(Ordering::Acquire) == NO_DUTY, 100_000_000);
            continue;
        }
        let now = time::now_ns();
        poll_pass(now);
        timers::fallback(now);
        balance::push_all(now);
        crate::sleep_ns(crate::POLL_NS);
    }
}
