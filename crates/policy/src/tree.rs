//! EINS-C (K5): the assistant's process tree. BitsOS's own mark, Linux has none: a thread group is in at most one
//! tree, gets it once (its own entry, or its parent's at fork; execve keeps it) and never loses it. Who acts is
//! a `Subject`; its `tree` is the tree's number (0: none), so every check below the syscalls sees it without a
//! lookup. A tree only takes rights away (its deletions go to the trash, the guarded files stay closed to it,
//! peers know the assistant from Georg's own programs), so a process may put itself into a fresh tree, but
//! nothing puts another process in or takes one out.
//!
//! The registry keeps each live tree's facts (its first process and start time) for `/proc/<pid>/bitsos_tree`;
//! the groups hold their tree, the registry only weakly, so a tree goes with its last process.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use ksync::SpinLock;

/// One assistant tree.
#[derive(Debug)]
pub struct Tree {
    /// Its number, never 0 and never reused within a boot.
    pub id: u32,
    /// The pid that entered it (the service `assist serve`).
    pub root: u32,
    /// When it was entered (ns since boot).
    pub since_ns: u64,
}

impl Drop for Tree {
    fn drop(&mut self) {
        let id = self.id;
        let gone = {
            let mut r = REGISTRY.lock();
            let at = r.iter().position(|(i, _)| *i == id);
            at.map(|i| r.swap_remove(i))
        };
        drop(gone);
        klog::event!(Info, "policy", "tree.ended", tree = id);
    }
}

static NEXT: AtomicU32 = AtomicU32::new(1);
static REGISTRY: SpinLock<Vec<(u32, Weak<Tree>)>> = SpinLock::named("policy.tree", Vec::new());

/// A fresh tree entered by `root`. None when no room was left for its entry (the caller answers ENOMEM).
pub fn new(root: u32, now_ns: u64) -> Option<Arc<Tree>> {
    let id = NEXT.fetch_add(1, Ordering::Relaxed).max(1);
    let t = Arc::new(Tree { id, root, since_ns: now_ns });
    let mut r = REGISTRY.lock();
    r.try_reserve(1).ok()?;
    r.push((id, Arc::downgrade(&t)));
    drop(r);
    klog::event!(Info, "policy", "tree.entered", tree = id, pid = root);
    Some(t)
}

/// The live tree with number `id`.
pub fn get(id: u32) -> Option<Arc<Tree>> {
    if id == 0 {
        return None;
    }
    REGISTRY.lock().iter().find(|(i, _)| *i == id).and_then(|(_, w)| w.upgrade())
}

/// How many trees live now (a ktest's and `/proc`'s view).
pub fn live() -> usize {
    REGISTRY.lock().iter().filter(|(_, w)| w.strong_count() > 0).count()
}

/// A chat label as a tree process may set it: 1 to 64 bytes of letters, digits, `.`, `_`, `-`.
pub fn valid_chat(s: &[u8]) -> bool {
    !s.is_empty() && s.len() <= 64 && s.iter().all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}
