//! Exclusive sections (Linux `stop_machine` in small): run a function on one CPU while every other online CPU
//! spins with interrupts off in its stopper thread. For consistency checks that must see no concurrent change
//! (allocator statistics in tests) and rare global changes. Stopper threads are KernelRt priority 99, pinned,
//! created on first use; they reach a CPU at its next preemption point, so the CPUs they stop hold no spin
//! lock. `f` must not take a `Mutex` (a stopped thread may hold it), wait for another CPU (IPIs are not
//! answered meanwhile) or sleep.

use crate::{Class, SpawnOpts};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use ksync::{Mutex, SpinLock, WaitQueue};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExclusiveError {
    /// Not every other CPU reached its stopper within the timeout (a thread there did not reach a preemption
    /// point); nothing ran.
    Timeout { arrived: usize, others: usize },
    /// No memory for a stopper thread.
    NoStopper,
}

/// One exclusive section at a time.
static SECTION: Mutex<()> = Mutex::named("sched.exclusive", ());
/// CPUs that have a stopper thread.
static STOPPERS: SpinLock<Vec<usize>> = SpinLock::named("sched.stoppers", Vec::new());
static WQ: WaitQueue = WaitQueue::new();
static GEN: AtomicU64 = AtomicU64::new(0);
static RELEASED: AtomicU64 = AtomicU64::new(0);
static OWNER: AtomicUsize = AtomicUsize::new(usize::MAX);
/// Stoppers that arrived and that left, each as (generation tag << 32 | count): a stopper counts only for the
/// section it serves, so one arriving late for a section that timed out is never counted for the next one.
static ARRIVED: AtomicU64 = AtomicU64::new(0);
static LEFT: AtomicU64 = AtomicU64::new(0);
/// How long a stopper of the current section may spin: the owner's arrival timeout plus `F_MARGIN_NS`.
static HOLD_LIMIT_NS: AtomicU64 = AtomicU64::new(F_MARGIN_NS);
/// The last section a stopper gave up on (its owner held it longer than `HOLD_LIMIT_NS`).
static GAVE_UP: AtomicU64 = AtomicU64::new(0);
/// Time `f` may take before stoppers give up on it.
const F_MARGIN_NS: u64 = 5_000_000_000;

fn tag(g: u64) -> u64 {
    (g & 0xFFFF_FFFF) << 32
}

/// Counts one more in `c` if it still belongs to section `g`. False: that section is over.
fn count_for(c: &AtomicU64, g: u64) -> bool {
    let mut v = c.load(Ordering::Acquire);
    loop {
        if v & !0xFFFF_FFFF != tag(g) {
            return false;
        }
        match c.compare_exchange_weak(v, v + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(now) => v = now,
        }
    }
}

fn counted(c: &AtomicU64, g: u64) -> usize {
    let v = c.load(Ordering::Acquire);
    if v & !0xFFFF_FFFF == tag(g) { (v & 0xFFFF_FFFF) as usize } else { 0 }
}

/// `seen`: the generation when it was created (a section that starts before this thread first runs is still
/// served).
fn stopper(cpu: usize, mut seen: u64) {
    loop {
        // B4: bounded waits; nothing to do between sections.
        WQ.wait_until_timeout(|| GEN.load(Ordering::Acquire) > seen, 1_000_000_000);
        let g = GEN.load(Ordering::Acquire);
        if g <= seen {
            continue;
        }
        seen = g;
        if OWNER.load(Ordering::Acquire) == cpu || crate::current_cpu() != cpu {
            continue;
        }
        let _irq = hal::cpu::IrqGuard::new();
        if !count_for(&ARRIVED, g) {
            // Late for a section that is over (it timed out); a newer one is served on the next round.
            continue;
        }
        let limit = HOLD_LIMIT_NS.load(Ordering::Acquire);
        let t0 = time::now_ns();
        while RELEASED.load(Ordering::Acquire) < g && time::now_ns() - t0 < limit {
            core::hint::spin_loop();
        }
        if RELEASED.load(Ordering::Acquire) < g {
            GAVE_UP.store(g, Ordering::Release);
            klog::event!(Warn, "sched", "exclusive.stopper_gave_up", cpu = cpu, held_ms = limit / 1_000_000; "the section's owner held this CPU too long");
        }
        count_for(&LEFT, g);
    }
}

fn ensure_stoppers() -> Result<(), ExclusiveError> {
    for cs in crate::cpus::online() {
        if STOPPERS.lock().contains(&cs.cpu) {
            continue;
        }
        let c = cs.cpu;
        let g = GEN.load(Ordering::Acquire);
        let opts = SpawnOpts { cpu: Some(c), pin: true, rt_prio: crate::RT_PRIO_MAX };
        crate::spawn_with("stopper", Class::KernelRt, opts, move || stopper(c, g)).map_err(|_| ExclusiveError::NoStopper)?;
        STOPPERS.lock().push(c);
    }
    Ok(())
}

/// Runs `f` on the calling CPU while all other online CPUs are held (see the module doc). Gives up after
/// `timeout_ns` if a CPU does not come; the caller stays on its CPU throughout.
pub fn run_exclusive<R>(timeout_ns: u64, f: impl FnOnce() -> R) -> Result<R, ExclusiveError> {
    let _section = SECTION.lock();
    ensure_stoppers()?;
    let me = crate::current_thread();
    let old_pin = me.as_ref().and_then(|t| t.pinned());
    let cpu = crate::current_cpu();
    crate::pin_current(Some(cpu));
    let cpu = crate::current_cpu();
    let others = crate::cpus::online().filter(|c| c.cpu != cpu).count();
    // One section at a time (SECTION), so the next generation is known before it is announced: the counters
    // are tagged with it first, and only stoppers serving it can count.
    let g = GEN.load(Ordering::Acquire) + 1;
    ARRIVED.store(tag(g), Ordering::Release);
    LEFT.store(tag(g), Ordering::Release);
    HOLD_LIMIT_NS.store(timeout_ns.saturating_add(F_MARGIN_NS), Ordering::Release);
    OWNER.store(cpu, Ordering::Release);
    GEN.store(g, Ordering::Release);
    WQ.wake_all();
    let t0 = time::now_ns();
    while counted(&ARRIVED, g) < others && time::now_ns() - t0 < timeout_ns {
        core::hint::spin_loop();
    }
    let arrived = counted(&ARRIVED, g);
    let r = if arrived == others && GAVE_UP.load(Ordering::Acquire) != g {
        // CP-5: the tick preempts at interrupt exit now; threads queued on this CPU must not run during `f`.
        let _np = ksync::PreemptGuard::new();
        Some(f())
    } else {
        None
    };
    if r.is_some() && GAVE_UP.load(Ordering::Acquire) == g {
        klog::event!(Warn, "sched", "exclusive.broken", took_more_than_ms = F_MARGIN_NS / 1_000_000; "a stopper left while the exclusive function ran");
    }
    RELEASED.store(g, Ordering::Release);
    let t1 = time::now_ns();
    while counted(&LEFT, g) < counted(&ARRIVED, g) && time::now_ns() - t1 < F_MARGIN_NS {
        core::hint::spin_loop();
    }
    OWNER.store(usize::MAX, Ordering::Release);
    crate::pin_current(old_pin);
    match r {
        Some(r) => Ok(r),
        None => {
            klog::event!(Warn, "sched", "exclusive.timeout", arrived = arrived, others = others; "a CPU did not reach a preemption point");
            Err(ExclusiveError::Timeout { arrived, others })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_count_only_for_their_section() {
        let c = AtomicU64::new(tag(7));
        assert!(count_for(&c, 7) && count_for(&c, 7));
        assert_eq!(counted(&c, 7), 2);
        // A stopper late for section 6 is not counted for 7, and 8 sees nothing of 7.
        assert!(!count_for(&c, 6));
        assert_eq!(counted(&c, 7), 2);
        assert_eq!(counted(&c, 8), 0);
        c.store(tag(8), Ordering::Relaxed);
        assert!(!count_for(&c, 7));
        assert_eq!(counted(&c, 8), 0);
    }
}
