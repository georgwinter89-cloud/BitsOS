//! The reaper (SC-3): frees what threads leave behind once no CPU can still use it.
//!
//! - Kernel stacks: hal retires a finished thread's stack instead of freeing it; `hal::switch::reap_stacks`
//!   unmaps it and frees it once every CPU has flushed its TLB after the unmap (a sync generation). Until MM-6's
//!   TLB shootdown this is what lets threads move between CPUs (SC-1 change request 2).
//! - Thread records: an exited thread leaves the table at its last switch, but an interrupt-time wake-up on
//!   another CPU may still hold a reference it took from the table just before. The record goes to the
//!   graveyard and is dropped after a sync generation: by then every CPU finished the interrupt handlers it was
//!   in, so the last reference is never dropped (and memory never freed) inside a handler.
//!
//! The work runs in the `reaper` thread (woken after switches that left something behind) and in `reap()` for
//! callers that need it done now (tests).

use crate::Thread;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::{IrqSpinLock, WaitQueue};

/// Exited threads and the sync generation they wait for (0: not assigned yet).
static GRAVES: IrqSpinLock<Vec<(u64, Arc<Thread>)>> = IrqSpinLock::named("sched", Vec::new());
static WQ: WaitQueue = WaitQueue::new();
static KICKED: AtomicBool = AtomicBool::new(false);
static PASS_BUSY: AtomicBool = AtomicBool::new(false);
/// Since when the oldest pending work waits (0: nothing pending), for the stall report.
static WAITING_SINCE: AtomicU64 = AtomicU64::new(0);

klog::counter!(pub STACKS_FREED, "sched.stacks_reaped");
klog::counter!(pub THREADS_FREED, "sched.threads_reaped");
klog::counter!(pub GRAVES_LEAKED, "sched.graves_leaked");

/// Makes room in the graveyard for every thread that exists now plus one (`spawn`, before the new thread is
/// listed), so `bury` never has to allocate. `Err`: no memory (the spawn fails).
pub(crate) fn reserve_grave() -> Result<(), ()> {
    let threads = crate::table::len() + 1;
    // Capacity for len + threads: a no-op when it is there already.
    GRAVES.lock().try_reserve(threads).map_err(|_| ())
}

/// An exited thread's record (from `after_switch`; interrupts off, thread context). The room was reserved at
/// spawn; should it be missing anyway and no memory be left, the record is leaked (counted and reported) rather
/// than dropped here, where an interrupt-time wake-up on another CPU may still hold it.
pub(crate) fn bury(t: Arc<Thread>) {
    let mut g = GRAVES.lock();
    if g.len() == g.capacity() && g.try_reserve(1).is_err() {
        drop(g);
        GRAVES_LEAKED.inc();
        klog::event!(Error, "sched", "reap.grave_leaked", id = t.id; "no memory for the graveyard; thread record leaked");
        core::mem::forget(t);
        return;
    }
    g.push((0, t));
}

pub(crate) fn graves_waiting() -> bool {
    !GRAVES.lock().is_empty()
}

/// Wakes the reaper thread.
pub(crate) fn kick() {
    if !KICKED.swap(true, Ordering::AcqRel) {
        WQ.wake_one();
    }
}

/// Makes the CPUs that have not passed generation `g` reach a quiescent point (IPI; this CPU directly).
fn nudge(g: u64) {
    let me = hal::percpu::this_cpu();
    for c in hal::switch::sync_lagging(g) {
        if c == me {
            hal::switch::quiescent_point();
        } else {
            crate::cpus::send_resched(c);
        }
    }
}

/// One pass. Returns true when nothing is left waiting.
pub(crate) fn pass() -> bool {
    if PASS_BUSY.swap(true, Ordering::AcqRel) {
        return false;
    }
    {
        // Request and stamp under one hold of the lock that `bury` takes: every record stamped with `g` was
        // buried (and so left the table) before `g` was requested. A record buried after a generation was
        // requested must never get it: a CPU may have passed it before taking a reference to that record.
        let mut graves = GRAVES.lock();
        if graves.iter().any(|e| e.0 == 0) {
            let g = hal::switch::request_sync();
            for e in graves.iter_mut().filter(|e| e.0 == 0) {
                e.0 = g;
            }
        }
    }
    let r = hal::switch::reap_stacks().unwrap_or_default();
    STACKS_FREED.add(r.freed as u64);
    nudge(hal::switch::sync_generation());
    // Free the stacks the nudge let through, and the records whose generation passed.
    let r2 = hal::switch::reap_stacks().unwrap_or_default();
    STACKS_FREED.add(r2.freed as u64);
    // The graveyard keeps its memory (what `reserve_grave` set aside): the freed records move into a vector
    // reserved here, outside the lock. Generation 0: buried after the stamping above, waits for the next pass.
    let mut gone: Vec<Arc<Thread>> = Vec::new();
    let n = GRAVES.lock().len();
    if gone.try_reserve(n).is_ok() {
        let mut graves = GRAVES.lock();
        let mut i = 0;
        while i < graves.len() {
            let e = graves[i].0;
            if gone.len() < gone.capacity() && e != 0 && hal::switch::sync_passed(e) {
                gone.push(graves.swap_remove(i).1);
            } else {
                i += 1;
            }
        }
    }
    THREADS_FREED.add(gone.len() as u64);
    // The last references of these threads go here: thread context, no scheduler lock held.
    drop(gone);
    let done = !hal::switch::retired_pending() && !graves_waiting();
    PASS_BUSY.store(false, Ordering::Release);
    done
}

/// Frees everything exited threads left behind, now: waits (bounded, 2 s) until threads that called `exit` have
/// switched away and every CPU passed the needed sync generation. Returns whether all was freed.
pub fn reap() -> bool {
    let t0 = time::now_ns();
    let limit = 2_000_000_000;
    while crate::EXITING.load(Ordering::Acquire) > 0 && time::now_ns() - t0 < limit {
        crate::sleep_ms(1);
    }
    loop {
        if pass() {
            return true;
        }
        if time::now_ns() - t0 > limit {
            return false;
        }
        crate::sleep_ms(1);
    }
}

/// The reaper thread (Normal class): sleeps until kicked (or 100 ms), then passes until done; reports a stall.
pub(crate) fn reaper_loop() {
    loop {
        WQ.wait_until_timeout(|| KICKED.load(Ordering::Acquire), 100_000_000);
        KICKED.store(false, Ordering::Release);
        let mut tries = 0u32;
        loop {
            if pass() {
                WAITING_SINCE.store(0, Ordering::Relaxed);
                break;
            }
            let now = time::now_ns();
            let since = WAITING_SINCE.load(Ordering::Relaxed);
            if since == 0 {
                WAITING_SINCE.store(now, Ordering::Relaxed);
            } else if now - since > 1_000_000_000 {
                let g = hal::switch::sync_generation();
                let lagging: Vec<usize> = hal::switch::sync_lagging(g).collect();
                klog::event!(Warn, "sched", "reap.stalled", generation = g, lagging = alloc::format!("{lagging:?}"),
                    stacks = hal::switch::retired_count(); "CPUs did not pass a quiescent point for 1 s (interrupts off?); retrying");
                WAITING_SINCE.store(now, Ordering::Relaxed);
            }
            tries += 1;
            if tries > 50 {
                break;
            }
            crate::sleep_ms(2);
        }
    }
}
