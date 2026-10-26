//! Soft autorepeat for evdev clients (IN-4 fixer; Linux `input_enable_softrepeat`, input.c:2275): a key held on
//! a device with EV_REP repeats to `/dev/input/eventN` readers as EV_KEY value 2 plus SYN_REPORT, after the
//! device's delay and then every period (EVIOCGREP/EVIOCSREP, KDKBDREP). Each open client keeps its own repeat
//! state (`evdevfs::Queue`), so a grab gives the repeats to the grabbing client alone, like the presses. The
//! console's own repeat stays in the VT (`repeat.rs`), which never sees these events.
//!
//! One kernel thread ("evdev-repeat", Interactive) fires due repeats; it sleeps at most a second at a time (B4)
//! and is woken by every press that starts a repeat.

use crate::evdevfs::Queue;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::{SpinLock, WaitQueue};

/// Clients of devices with EV_REP (dropped clients are pruned on the next pass).
static QUEUES: SpinLock<Vec<Weak<Queue>>> = SpinLock::named("evdev.repeat_queues", Vec::new());
static WQ: WaitQueue = WaitQueue::new();
static KICK: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);

const IDLE_NS: u64 = 1_000_000_000;
const MIN_NS: u64 = 1_000_000;

pub fn register(q: &Arc<Queue>) {
    QUEUES.lock().push(Arc::downgrade(q));
}

/// A repeat started: the thread looks at the deadlines again.
pub fn kick() {
    KICK.store(true, Ordering::Release);
    WQ.wake_all();
}

/// Fires every due repeat; returns the earliest next deadline.
pub fn tick(now: u64) -> Option<u64> {
    let live: Vec<Arc<Queue>> = {
        let mut l = QUEUES.lock();
        l.retain(|w| w.strong_count() > 0);
        l.iter().filter_map(|w| w.upgrade()).collect()
    };
    live.iter().filter_map(|q| q.repeat_due(now)).min()
}

fn thread() {
    klog::event!(Info, "input", "evdev.repeat_started");
    loop {
        let now = time::now_ns();
        let wait = tick(now).map(|d| d.saturating_sub(now)).unwrap_or(IDLE_NS).clamp(MIN_NS, IDLE_NS);
        WQ.wait_until_timeout(|| KICK.swap(false, Ordering::AcqRel), wait);
    }
}

/// Starts the repeat thread (Service level, with the VT).
pub fn start() -> Result<(), &'static str> {
    if STARTED.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    if let Err(e) = sched::spawn("evdev-repeat", sched::Class::Interactive, thread) {
        klog::event!(Error, "input", "evdev.repeat_spawn_failed", error = alloc::format!("{e:?}"); "programs reading evdev get no key repeat");
        return Err("evdev-repeat thread");
    }
    Ok(())
}
