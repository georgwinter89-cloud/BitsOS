//! BIGDIR: memory a file system keeps beside the page cache to be fast (ext4's directory indexes) and can make
//! again from its disk. Under memory pressure it is asked to give it back, what was unused longest first; the
//! file system decides what that is. V2's dentries and inodes are not such memory (they live while used).
//!
//! Called from the background reclaim thread and from drop_caches only (thread context, no lock held): an owner
//! may take its own locks but should not wait long (ext4 only tries its volume lock).
//!
//! The caller never ends up owning an owner. A reference taken here for the call could be the last one when an
//! unmount drops its own meanwhile, and then the volume's last commit (journal, device writes, flush) would run
//! in the reclaim thread while memory is short. So an owner is listed through a `Registration` that its mount
//! keeps beside its own reference and drops first: the drop waits for a call in flight and takes the owner off
//! the list, and a call gives its reference back before it lets go of the entry.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use ksync::Mutex;

/// How hard memory is wanted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// A light background round: what nobody used for a while.
    Idle,
    /// A thorough round (memory is short): everything not in use right now.
    Hard,
    /// drop_caches: everything.
    All,
}

pub trait MetaShrink: Send + Sync {
    /// Gives memory back; returns the bytes.
    fn shrink_meta(&self, level: Level) -> u64;
}

/// One listed owner. The lock is held over a call of the owner and by the registration's drop.
struct Entry {
    owner: Mutex<Option<Weak<dyn MetaShrink>>>,
}

static OWNERS: Mutex<Vec<Arc<Entry>>> = Mutex::named("vfs.meta_shrink", Vec::new());

klog::counter!(CALLS, "vfs.meta_shrink.calls");
klog::counter!(BYTES, "vfs.meta_shrink.bytes");
klog::counter!(NOT_LISTED, "vfs.meta_shrink.not_listed");

/// An owner's place on the list. Whoever registers keeps it next to a strong reference to the owner and drops
/// it BEFORE that reference (a struct field declared above it): see the module docs.
#[must_use = "dropping it takes the owner off the list"]
pub struct Registration {
    entry: Arc<Entry>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        // Waits for a call in flight (short: the owner does not wait in it); none starts after this.
        *self.entry.owner.lock() = None;
        OWNERS.lock().retain(|e| !Arc::ptr_eq(e, &self.entry));
    }
}

/// Lists an owner (a mounted volume) until the registration is dropped.
pub fn register(owner: Weak<dyn MetaShrink>) -> Registration {
    let entry = Arc::new(Entry { owner: Mutex::named("vfs.meta_shrink.owner", Some(owner)) });
    let mut l = OWNERS.lock();
    if l.try_reserve(1).is_ok() {
        l.push(entry.clone());
    } else {
        // Its memory is then only given back by its own rules (ext4: a refused charge, unmount).
        NOT_LISTED.inc();
    }
    Registration { entry }
}

/// Asks every owner; returns the bytes given back. Takes no memory (the reclaim thread calls it).
pub fn run(level: Level) -> u64 {
    let mut bytes = 0;
    let mut i = 0;
    loop {
        // The list is unlocked over the call: a mount elsewhere must not wait for a volume.
        let Some(e) = OWNERS.lock().get(i).cloned() else { break };
        i += 1;
        let owner = e.owner.lock();
        if let Some(o) = owner.as_ref().and_then(Weak::upgrade) {
            bytes += o.shrink_meta(level);
            // Given back under the entry's lock: the registration's holder still has its own reference.
            drop(o);
        }
    }
    CALLS.inc();
    BYTES.add(bytes);
    bytes
}
