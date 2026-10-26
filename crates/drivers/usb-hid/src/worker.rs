//! The driver's one worker thread ("usb-hid", Interactive): what the report path must not do in a controller's
//! event context, because it waits for the device. It sends keyboard LED reports (`leds::service`) and restarts
//! interrupt pipes the controller gave up (`HidIface::restart_if_due`). It wakes on `kick` and once a second
//! (bounded wait, B4), which is also the retry delay of a failed LED report and the tick of pipe restarts.

use crate::{iface, leds};
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::WaitQueue;

pub const TICK_NS: u64 = 1_000_000_000;

static WORK: WaitQueue = WaitQueue::new();
static PENDING: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);

/// Has the worker look at every interface soon. Callable from any context (never sleeps).
pub fn kick() {
    PENDING.store(true, Ordering::Release);
    WORK.wake_all();
}

/// Starts the worker once (from a probe: thread context). False if the thread could not be started.
pub fn ensure_started() -> bool {
    if STARTED.swap(true, Ordering::AcqRel) {
        return true;
    }
    match sched::spawn("usb-hid", sched::Class::Interactive, run) {
        Ok(_) => true,
        Err(e) => {
            STARTED.store(false, Ordering::Release);
            klog::event!(Error, "input", "hid.worker_failed", error = alloc::format!("{e:?}");
                "keyboard LEDs stay as they are, given-up pipes are not restarted");
            false
        }
    }
}

fn run() {
    loop {
        // Bounded (B4); the timeout also catches a kick that raced with the flag.
        WORK.wait_until_timeout(|| PENDING.load(Ordering::Acquire), TICK_NS);
        PENDING.store(false, Ordering::Release);
        let now = time::now_ns();
        for h in iface::all() {
            h.restart_if_due(now);
            leds::service(&h);
        }
    }
}
