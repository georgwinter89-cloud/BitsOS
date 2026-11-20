//! SYS-K: the newest round for readers that must answer at once (hwmon attributes, `/proc/bitsos/metrics`): the
//! sampler sets it after each round, a read copies a few numbers out. Older than `STALE_NS` (the sampler stalled
//! or stopped): no value, never an old one passed off as current.

use super::store::Unit;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;

/// Three missed rounds.
pub const STALE_NS: u64 = 3 * super::PERIOD_NS;

#[derive(Clone, Default)]
pub struct Latest {
    pub at_ns: u64,
    /// The k10temp sensors by the sampler's name (`tctl`, `ccd0`, ...), thousandths of a degree.
    pub temps: Vec<(String, u64)>,
    /// Package power averaged over the round (mW).
    pub pkg_mw: Option<u64>,
    /// Every series of the round with its unit (`/proc/bitsos/metrics`).
    pub values: Vec<(String, Unit, u64)>,
}

/// A shared round: a reader takes a reference under the spin lock, never a copy (tens of KiB of names), and the
/// round a `set` replaces is freed after the unlock. Under a spin lock the heap can neither reclaim nor fall back
/// to single frames (ksync spare.rs), and any uid 1000 program can read `/proc/bitsos/metrics` as often as it likes.
static LATEST: SpinLock<Option<Arc<Latest>>> = SpinLock::named("rb.metrics.latest", None);

pub fn set(l: Latest) {
    let l = Arc::new(l);
    let old = LATEST.lock().replace(l);
    drop(old);
}

/// The newest round if it is fresh at `now_ns`.
pub fn get(now_ns: u64) -> Option<Arc<Latest>> {
    let l = LATEST.lock().clone()?;
    (now_ns.saturating_sub(l.at_ns) <= STALE_NS).then_some(l)
}

/// One sensor's value if fresh.
pub fn temp(name: &str, now_ns: u64) -> Option<u64> {
    let g = LATEST.lock();
    let l = g.as_ref().filter(|l| now_ns.saturating_sub(l.at_ns) <= STALE_NS)?;
    l.temps.iter().find(|(n, _)| n == name).map(|(_, v)| *v)
}

pub fn pkg_mw(now_ns: u64) -> Option<u64> {
    LATEST.lock().as_ref().filter(|l| now_ns.saturating_sub(l.at_ns) <= STALE_NS)?.pkg_mw
}
