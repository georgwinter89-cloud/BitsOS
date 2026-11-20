//! The last failed probe of each function, kept after `bind::release_failed` has freed the binding record. The
//! Gaming-PC's report page (plan/spec/gaming-pc.md §8) needs it: "a driver tried and gave up, for this reason" must
//! not read like "no driver exists", or a photo of the page does not say where the keyboard's controller stopped.
//!
//! Memory only. A note is written when a probe returns an error and dropped when the function is offered to the
//! drivers again; nothing here reads or writes the device, and only the report reads the notes.

use crate::Loc;
use alloc::collections::BTreeMap;
use alloc::string::String;
use ksync::SpinLock;

/// A probe that ended in an error: the driver and what it said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub driver: &'static str,
    pub error: String,
}

/// A leaf: taken with no other PCI lock held, and nothing is taken under it.
static FAILED: SpinLock<BTreeMap<Loc, Failure>> = SpinLock::named("pci.failed", BTreeMap::new());

/// `driver`'s probe of `at` failed with `error`. One note per function: the last driver that tried.
pub(crate) fn note(at: Loc, driver: &'static str, error: &str) {
    let new = Failure { driver, error: String::from(error) };
    let old = FAILED.lock().insert(at, new);
    // Freed with the lock released.
    drop(old);
}

/// The function is offered to the drivers again, or one bound it: the old note no longer describes it.
pub(crate) fn forget(at: Loc) {
    let old = FAILED.lock().remove(&at);
    drop(old);
}

/// The last failed probe of a function no driver took since.
pub fn last(at: Loc) -> Option<Failure> {
    FAILED.lock().get(&at).cloned()
}
