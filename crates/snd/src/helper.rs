//! SN-12: one request in a thread of its own (`/sys/bitsos/snd_output`: a write(2) holds the file's position
//! lock while the attribute stores, and the request's locks sit outside every subsystem lock). The writer waits
//! for it like a synchronous store: a failure it reports means nothing changed, as Linux's sysfs stores do. The
//! request calls [`Ticket::begin`] once it holds what it needs; until then the writer may withdraw it (bound
//! passed, or a signal); after it the writer waits for the outcome, bounded again by `bound_ns` (the request's
//! own steps have their own bounds: a miss there is a fault, reported as `Late`, outcome in the log).

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use ksync::{SpinLock, WaitQueue};

const PENDING: u8 = 0;
const RUNNING: u8 = 1;
const CANCELLED: u8 = 2;

/// Slice of the writer's wait: how often it looks at signals and the clock.
const SLICE_NS: u64 = 100_000_000;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Waited<T> {
    Done(T),
    /// Withdrawn before it began: nothing was done. `signal`: a signal came, else the bound passed first.
    Cancelled { signal: bool },
    /// It began and did not finish within the bound: it runs on, its outcome is in the log.
    Late,
    /// No thread could be started: nothing was done.
    NoThread,
}

/// The request's side of the withdraw race.
pub(crate) struct Ticket(AtomicU8);

impl Ticket {
    /// True: the request may go on and the writer waits for it. False: the writer withdrew it; do nothing.
    pub(crate) fn begin(&self) -> bool {
        self.0.compare_exchange(PENDING, RUNNING, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    fn cancel(&self) -> bool {
        self.0.compare_exchange(PENDING, CANCELLED, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    fn running(&self) -> bool {
        self.0.load(Ordering::Acquire) == RUNNING
    }
}

struct Slot<T> {
    ticket: Ticket,
    v: SpinLock<Option<T>>,
    done: AtomicBool,
    wq: WaitQueue,
}

/// Runs `f` in a new Normal thread `name` and waits for it as the module says. `f` returns None when
/// [`Ticket::begin`] said no.
pub(crate) fn run<T: Send + 'static>(name: &str, bound_ns: u64, f: impl FnOnce(&Ticket) -> Option<T> + Send + 'static) -> Waited<T> {
    let spawn = |name: &str, job: Box<dyn FnOnce() + Send>| sched::spawn(name, sched::Class::Normal, job).is_ok();
    run_with(spawn, vfs::dentry::interrupted, name, bound_ns, f)
}

/// [`run`] with the thread start and the signal check given (the ktests reach every branch through them).
pub(crate) fn run_with<T: Send + 'static>(
    spawn: impl FnOnce(&str, Box<dyn FnOnce() + Send>) -> bool,
    interrupted: impl Fn() -> bool,
    name: &str,
    bound_ns: u64,
    f: impl FnOnce(&Ticket) -> Option<T> + Send + 'static,
) -> Waited<T> {
    let slot = Arc::new(Slot {
        ticket: Ticket(AtomicU8::new(PENDING)),
        v: SpinLock::named("snd.helper", None),
        done: AtomicBool::new(false),
        wq: WaitQueue::new(),
    });
    let s = slot.clone();
    let job: Box<dyn FnOnce() + Send> = Box::new(move || {
        let v = f(&s.ticket);
        *s.v.lock() = v;
        s.done.store(true, Ordering::Release);
        s.wq.wake_all();
    });
    if !spawn(name, job) {
        return Waited::NoThread;
    }
    let start = time::now_ns();
    let mut running_since = None;
    loop {
        if slot.done.load(Ordering::Acquire) {
            // Done with nothing in it only when begin said no, which only this writer's withdraw causes.
            return slot.v.lock().take().map_or(Waited::Cancelled { signal: false }, Waited::Done);
        }
        let now = time::now_ns();
        if slot.ticket.running() {
            let since = *running_since.get_or_insert(now);
            if now.saturating_sub(since) >= bound_ns {
                return Waited::Late;
            }
        } else {
            if interrupted() && slot.ticket.cancel() {
                return Waited::Cancelled { signal: true };
            }
            if now.saturating_sub(start) >= bound_ns && slot.ticket.cancel() {
                return Waited::Cancelled { signal: false };
            }
        }
        slot.wq.wait_until_timeout(|| slot.done.load(Ordering::Acquire), SLICE_NS.min(bound_ns.max(1)));
    }
}
