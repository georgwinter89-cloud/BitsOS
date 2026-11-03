//! RB-15: the order of the steps that end this kernel by a reset (MCP `reboot`, the console's `reboot` and
//! `poweroff`), and the console mirror's part in it. Here because platform, mcp and the network drivers all
//! depend on klog, while the mirror lives in klogd (which depends on mcp).
//!
//! The order (Linux `kernel_restart_prepare` -> `device_shutdown`: the netconsole stops and the network cards
//! shut down before the reset; linux-6.6/kernel/reboot.c, drivers/base/core.c `device_shutdown`):
//! 1. [`Step::MirrorPaused`]: the mirror stops sending through the network stack and its thread (no datagram of
//!    it half-way through smoltcp or the card's NAPI path when the CPUs are parked); from now on what is logged
//!    goes out on the raw path (`NetDevice::poll_tx`) at each [`checkpoint`], and the card is asked
//!    ([`TxDone`]) until it has sent it.
//! 2. [`Step::Storage`]: `block::shutdown::stop_all`.
//! 3. [`Step::Parked`]: the other CPUs parked.
//! 4. [`Step::Quiesce`]: `devmodel::quiesce_all`: the mirror's rest and its end datagram, the card's done, then
//!    the cards (newest first: the mirror before the network cards).
//! 5. [`Step::Reset`]: the platform reset.
//!
//! Each step is a `shutdown step` event (a Warn when it comes out of order) and a counter. RAM does not survive
//! the Mini-PC's reset (rb-RB-13), so the next start cannot read them: the mirror's listener (which gets every
//! line up to step 4 before the end datagram) and the console are where "how far it came" is read.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};
use ksync::{OnceRef, SpinLock};

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    None = 0,
    MirrorPaused = 1,
    Storage = 2,
    Parked = 3,
    Quiesce = 4,
    Reset = 5,
}

impl Step {
    pub fn as_str(self) -> &'static str {
        match self {
            Step::None => "none",
            Step::MirrorPaused => "mirror_paused",
            Step::Storage => "storage",
            Step::Parked => "parked",
            Step::Quiesce => "quiesce",
            Step::Reset => "reset",
        }
    }
    pub fn from_u8(v: u8) -> Step {
        match v {
            1 => Step::MirrorPaused,
            2 => Step::Storage,
            3 => Step::Parked,
            4 => Step::Quiesce,
            5 => Step::Reset,
            _ => Step::None,
        }
    }
}

counter!(STEP_PAUSED, "shutdown.mirror_paused");
counter!(STEP_STORAGE, "shutdown.storage");
counter!(STEP_PARKED, "shutdown.parked");
counter!(STEP_QUIESCE, "shutdown.quiesce");
counter!(STEP_RESET, "shutdown.reset");
counter!(OUT_OF_ORDER, "shutdown.out_of_order");

/// The last step this kernel reached (0: none).
static REACHED: AtomicU8 = AtomicU8::new(0);

/// The last step reached.
pub fn reached() -> Step {
    Step::from_u8(REACHED.load(Ordering::Acquire))
}

/// Back to "nothing reached" (a power-off that did not happen, ktests).
pub fn reset_steps() {
    REACHED.store(0, Ordering::Release);
}

/// Records `step` of the shutdown `what` (thread context: it logs). Returns whether it came in order (every step
/// after the one before; a step may be skipped, e.g. no mirror). Lock-free besides the event.
pub fn mark(step: Step, what: &'static str) -> bool {
    let before = Step::from_u8(REACHED.swap(step as u8, Ordering::AcqRel));
    let in_order = step > before;
    match step {
        Step::MirrorPaused => STEP_PAUSED.inc(),
        Step::Storage => STEP_STORAGE.inc(),
        Step::Parked => STEP_PARKED.inc(),
        Step::Quiesce => STEP_QUIESCE.inc(),
        Step::Reset => STEP_RESET.inc(),
        Step::None => {}
    }
    if in_order {
        crate::event!(Info, "shutdown", "step", what = what, step = step.as_str(), after = before.as_str());
    } else {
        OUT_OF_ORDER.inc();
        crate::event!(Warn, "shutdown", "step", what = what, step = step.as_str(), after = before.as_str(), in_order = false;
            "a shutdown step came out of order");
    }
    in_order
}

// ----------------------------------------------------------------------------------------------------------
// The mirror's hook (klogd installs it when the mirror goes live)

/// What the mirror's pause did (logged by the caller's `mirror_paused` event, checked by tests).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PauseReport {
    /// The mirror was live (else nothing happened).
    pub was_live: bool,
    /// The mirror's thread was still inside a send when the deadline came (it goes on beside the raw path;
    /// the listener drops what it gets twice).
    pub thread_busy: bool,
    /// Raw frames the card took.
    pub frames: u32,
    /// The card refused a frame at least once (its TX lock busy, the ring full) within the budget.
    pub dropped: bool,
    /// The card said it had sent everything (TDH == TDT and every descriptor written back).
    pub done: bool,
    /// The card could not say (no probe, not running).
    pub done_unknown: bool,
}

/// The mirror's side of the shutdown (klogd's `Mirror`). Thread context; waits through `sleep`.
pub trait MirrorHook: Sync {
    /// Step 1: the normal path off (the thread out of its send), what waits out raw, the card's done.
    fn pause(&self, sleep: &dyn Fn(u64)) -> PauseReport;
    /// After a step: what was logged since out raw, the card's done.
    fn checkpoint(&self, sleep: &dyn Fn(u64)) -> PauseReport;
    /// The normal path back (a power-off that did not happen; tests).
    fn unpause(&self);
}

static MIRROR: OnceRef<dyn MirrorHook> = OnceRef::new();

/// klogd: the live mirror (the first call wins).
pub fn set_mirror(h: &'static dyn MirrorHook) -> bool {
    MIRROR.set(h)
}

/// Step 1 for `what`: the mirror's pause (None: no mirror), then the step mark. Feeds the deferred sinks first,
/// so the lines logged so far are in the mirror's ring when it sends raw.
pub fn pause_mirror(what: &'static str, sleep: &dyn Fn(u64)) -> Option<PauseReport> {
    pause_marked(what, sleep).0
}

/// `pause_mirror`, and whether its mark came in order.
fn pause_marked(what: &'static str, sleep: &dyn Fn(u64)) -> (Option<PauseReport>, bool) {
    let r = MIRROR.get().map(|m| {
        let _ = crate::drain_deferred(DRAIN_BUDGET);
        m.pause(sleep)
    });
    match r {
        Some(r) if r.was_live => {
            crate::event!(Info, "shutdown", "mirror_paused", what = what, frames = r.frames, dropped = r.dropped,
                done = r.done, done_unknown = r.done_unknown, thread_busy = r.thread_busy);
        }
        _ => crate::event!(Info, "shutdown", "mirror_paused", what = what, live = false),
    }
    let in_order = mark(Step::MirrorPaused, what);
    (r, in_order)
}

/// Records `step` and sends what was logged up to it raw (thread context; a no-op without a paused mirror).
/// Feeds the deferred sinks first (bounded rounds: a drainer parked half-way leaves them, the quiesce sends
/// what the ring has).
pub fn step(step: Step, what: &'static str, sleep: &dyn Fn(u64)) -> Option<PauseReport> {
    step_marked(step, what, sleep).0
}

/// `step`, and whether its mark came in order.
fn step_marked(step: Step, what: &'static str, sleep: &dyn Fn(u64)) -> (Option<PauseReport>, bool) {
    let in_order = mark(step, what);
    let Some(m) = MIRROR.get() else { return (None, in_order) };
    for _ in 0..DRAIN_ROUNDS {
        if !crate::pending() {
            break;
        }
        if crate::drain_deferred(DRAIN_BUDGET) == 0 {
            sleep(DRAIN_RETRY_NS);
        }
    }
    (Some(m.checkpoint(sleep)), in_order)
}

/// What [`run`] hands back: the park (dropping it releases the CPUs: a power-off that did not happen), the
/// quiesce's result, and whether every step came in order.
pub struct Ended<P, Q> {
    pub park: P,
    pub quiesced: Q,
    pub in_order: bool,
}

/// The whole order up to the reset, one place for every path that ends in a platform reset (MCP `reboot`,
/// the console's `reboot` and `poweroff`; RB-15 fixer: the order is host-tested here, not repeated per
/// caller): the mirror's pause, `storage` (`block::shutdown::stop_all`), its step, `park`, its step, the
/// quiesce step, `quiesce` (`devmodel::quiesce_all` and its event), the reset mark. Thread context. The
/// caller resets next; when the reset does not happen it calls [`unpause_mirror`] and [`reset_steps`].
pub fn run<P, Q>(
    what: &'static str,
    sleep: &dyn Fn(u64),
    storage: impl FnOnce(),
    park: impl FnOnce() -> P,
    quiesce: impl FnOnce() -> Q,
) -> Ended<P, Q> {
    // RB-16: a crumb before each step (the wire line on the listener, the watchdog's count across the reset).
    crate::crumb::leave(crate::crumb::Stage::Paused, what);
    let (_, mut in_order) = pause_marked(what, sleep);
    crate::crumb::leave(crate::crumb::Stage::Storage, what);
    storage();
    in_order &= step_marked(Step::Storage, what, sleep).1;
    crate::crumb::leave(crate::crumb::Stage::Parked, what);
    let park = park();
    in_order &= step_marked(Step::Parked, what, sleep).1;
    in_order &= step_marked(Step::Quiesce, what, sleep).1;
    crate::crumb::leave(crate::crumb::Stage::Quiesce, what);
    let quiesced = quiesce();
    crate::crumb::leave(crate::crumb::Stage::Reset, what);
    in_order &= mark(Step::Reset, what);
    Ended { park, quiesced, in_order }
}

/// The normal path back (a power-off that did not reach the quiesce; ktests).
pub fn unpause_mirror() {
    if let Some(m) = MIRROR.get() {
        m.unpause();
    }
}

const DRAIN_BUDGET: usize = 4096;
const DRAIN_ROUNDS: usize = 8;
const DRAIN_RETRY_NS: u64 = 1_000_000;

// ----------------------------------------------------------------------------------------------------------
// Has the card sent it? (network drivers register a probe; the mirror asks before the quiesce goes on)

/// A network card's "everything handed to me is on the wire": `Some(true)` when the transmit ring is empty and
/// every descriptor was written back, `Some(false)` while it still holds frames, `None` when it cannot say (not
/// running, gone). Lock-free and silent: the mirror asks it inside the quiesce.
pub trait TxDone: Send + Sync {
    fn name(&self) -> &str;
    fn tx_done(&self) -> Option<bool>;
}

/// Taken in thread context only (registration, the mirror's route look-up); a leaf.
static PROBES: SpinLock<Vec<Arc<dyn TxDone>>> = SpinLock::named("klog.shutdown.probes", Vec::new());

/// A network driver, once per card (at probe).
pub fn register_tx_done(p: Arc<dyn TxDone>) {
    PROBES.lock().push(p);
}

/// The probe of the card `name` (thread context; the mirror keeps it with its route).
pub fn tx_done_probe(name: &str) -> Option<Arc<dyn TxDone>> {
    PROBES.lock().iter().find(|p| p.name() == name).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_in_order_and_out_of_order() {
        let _g = crate::tests::serial_limited();
        reset_steps();
        assert!(mark(Step::MirrorPaused, "test"));
        assert!(mark(Step::Parked, "test"), "a skipped step is still in order");
        let before = OUT_OF_ORDER.get();
        assert!(!mark(Step::Storage, "test"));
        assert_eq!(OUT_OF_ORDER.get(), before + 1);
        assert_eq!(reached(), Step::Storage);
        reset_steps();
        assert_eq!(reached(), Step::None);
        assert_eq!(Step::from_u8(9), Step::None);
        for s in [Step::MirrorPaused, Step::Storage, Step::Parked, Step::Quiesce, Step::Reset] {
            assert_eq!(Step::from_u8(s as u8), s);
        }
    }

    #[test]
    fn run_keeps_the_order_and_marks_every_step() {
        use core::cell::RefCell;
        let _g = crate::tests::serial_limited();
        reset_steps();
        let log: RefCell<Vec<(&str, Step)>> = RefCell::new(Vec::new());
        let crumbs: RefCell<Vec<crate::crumb::Stage>> = RefCell::new(Vec::new());
        let crumb = || crumbs.borrow_mut().push(crate::crumb::last().unwrap().1);
        let before = OUT_OF_ORDER.get();
        let e = run(
            "test",
            &|_| {},
            || {
                crumb();
                log.borrow_mut().push(("storage", reached()))
            },
            || {
                crumb();
                log.borrow_mut().push(("park", reached()))
            },
            || {
                crumb();
                log.borrow_mut().push(("quiesce", reached()));
                7u32
            },
        );
        // RB-16: each step's crumb is left before the step runs; the reset's after the quiesce.
        use crate::crumb::Stage;
        assert_eq!(*crumbs.borrow(), [Stage::Storage, Stage::Parked, Stage::Quiesce]);
        assert_eq!(crate::crumb::last().unwrap().1, Stage::Reset);
        assert!(e.in_order);
        assert_eq!(e.quiesced, 7);
        assert_eq!(OUT_OF_ORDER.get(), before);
        // Each callback runs after the step before it was marked and before its own.
        assert_eq!(*log.borrow(), [("storage", Step::MirrorPaused), ("park", Step::Storage), ("quiesce", Step::Quiesce)]);
        assert_eq!(reached(), Step::Reset);
        // A second shutdown without reset_steps (a power-off that did not happen and forgot it) is out of order.
        let e = run("test", &|_| {}, || {}, || {}, || {});
        assert!(!e.in_order);
        reset_steps();
    }

    struct P(&'static str);
    impl TxDone for P {
        fn name(&self) -> &str {
            self.0
        }
        fn tx_done(&self) -> Option<bool> {
            Some(true)
        }
    }

    #[test]
    fn probes_by_name() {
        register_tx_done(Arc::new(P("shutdown-test0")));
        assert_eq!(tx_done_probe("shutdown-test0").and_then(|p| p.tx_done()), Some(true));
        assert!(tx_done_probe("shutdown-none").is_none());
    }
}
