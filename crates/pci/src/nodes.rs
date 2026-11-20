//! The scanned device tree as one shared snapshot behind `pci.devices`.
//!
//! Mini-PC 11.11. and 12.11.2026: every reader copied the whole list (17 KiB) while holding the spin lock, where the
//! heap may not reclaim; once the page cache had filled memory a sysfs lookup found no run of frames and the
//! kernel panicked. A reader now takes a reference under the lock and nothing else; whoever needs its own copy
//! makes it with the lock released. A writer copies, changes and swaps (writers are rare: the scan, a rescan, BARs
//! sized after a wake).

use crate::Node;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;

pub(crate) struct Nodes {
    cur: SpinLock<Option<Arc<Vec<Node>>>>,
}

/// The tree as it was when taken; reads as a slice of nodes.
#[derive(Clone)]
pub struct Snapshot(Option<Arc<Vec<Node>>>);

impl core::ops::Deref for Snapshot {
    type Target = [Node];
    fn deref(&self) -> &[Node] {
        self.0.as_ref().map_or(&[], |v| v.as_slice())
    }
}

impl Snapshot {
    /// Whether both are the same tree object (not merely equal trees).
    pub fn same(&self, other: &Snapshot) -> bool {
        match (&self.0, &other.0) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        }
    }
}

impl Nodes {
    pub(crate) const fn new() -> Self {
        Nodes { cur: SpinLock::named("pci.devices", None) }
    }

    pub(crate) fn get(&self) -> Snapshot {
        Snapshot(self.cur.lock().clone())
    }

    pub(crate) fn set(&self, nodes: Vec<Node>) {
        let new = Some(Arc::new(nodes));
        let old = core::mem::replace(&mut *self.cur.lock(), new);
        // Freed with the lock released.
        drop(old);
    }

    /// Copy, change, swap. `change` runs again on a fresh copy when another writer came between.
    pub(crate) fn update<R>(&self, mut change: impl FnMut(&mut Vec<Node>) -> R) -> R {
        loop {
            let seen = self.get();
            let mut next = seen.to_vec();
            let r = change(&mut next);
            let new = Some(Arc::new(next));
            let mut cur = self.cur.lock();
            let same = match (&*cur, &seen.0) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            };
            if same {
                let old = core::mem::replace(&mut *cur, new);
                drop(cur);
                drop(old);
                return r;
            }
            drop(cur);
        }
    }
}
