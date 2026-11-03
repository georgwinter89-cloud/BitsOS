//! PF-5: a sparse index from 64-bit numbers to `Arc<T>` that readers walk without a lock (a radix tree of
//! 64-slot nodes, as vfs's `PageIndex` and Linux's XArray). Changes go through the write side of its own lock
//! (`write`), which also keeps out readers that take the read side (`read`: the page cache's locked path,
//! which takes references). A lockless reader (`get` with a `grace::Section`) gets a `&T` that lives as long
//! as its section: values and nodes taken out are dropped only after a grace period (`grace::Retire`,
//! `grace::defer_box`), never while a section that could have found them is open.
//!
//! Publication: a node is filled before the pointer to it is stored (Release), a reader loads with Acquire. The
//! root word carries the tree's height in its low bits (nodes are 64-byte aligned), so a reader sees a root
//! and its height of one moment. Growth puts the old root under a new one (the old stays valid for readers
//! that hold it); empty nodes leave on removal (deferred); an empty tree frees its root.
//!
//! Node memory is fallible (`NoRoom`, R2); no count is fixed (R1): the tree grows a level whenever an index
//! does not fit, up to the 64-bit space (11 levels; the path arrays below are that bound of the index math).

use crate::grace::{self, Retire, Section};
use alloc::sync::Arc;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering::*};
use ksync::{ReadGuard, RwSpinLock, WriteGuard};

const BITS: u32 = 6;
const FAN: usize = 1 << BITS;
const MASK: u64 = FAN as u64 - 1;
/// Levels of a tree that holds every 64-bit index.
const MAX_LEVELS: usize = 11;
const HEIGHT_MASK: usize = 0x3F;

/// The heap had no room for a node; nothing changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoRoom;

/// Slots of one node: at level 1 `Arc<T>` pointers (`Arc::into_raw`), above it child nodes.
#[repr(align(64))]
struct Node {
    slots: [AtomicPtr<()>; FAN],
}

/// Host tests: a dropped node poisons its slots, so a reader that walks a freed node fails loudly instead of
/// silently reading the heap's next tenant (the prepare-order test).
#[cfg(test)]
pub(crate) const POISON: usize = 0xDEAD_0000;
#[cfg(test)]
impl Drop for Node {
    fn drop(&mut self) {
        for s in self.slots.iter() {
            s.store(POISON as *mut (), SeqCst);
        }
    }
}

fn new_node() -> Result<*mut Node, NoRoom> {
    let n = grace::try_box(Node { slots: [const { AtomicPtr::new(core::ptr::null_mut()) }; FAN] }).ok_or(NoRoom)?;
    Ok(alloc::boxed::Box::into_raw(n))
}

fn digit(i: u64, level: u32) -> usize {
    let shift = BITS * (level - 1);
    if shift >= 64 { 0 } else { ((i >> shift) & MASK) as usize }
}

fn fits(i: u64, height: u32) -> bool {
    let bits = BITS * height;
    bits >= 64 || i >> bits == 0
}

/// The least height whose tree holds index `i`.
fn height_for(i: u64) -> u32 {
    let mut h = 1;
    while !fits(i, h) {
        h += 1;
    }
    h
}

/// A value of the index as a locked reader or the writer sees it (valid while the guard is borrowed).
pub struct Entry<'a, T> {
    ptr: *const T,
    _l: PhantomData<&'a T>,
}

impl<T> Entry<'_, T> {
    /// Strong references now (the index's included).
    pub fn strong_count(&self) -> usize {
        // SAFETY: the index holds a strong count while the guard is borrowed; ManuallyDrop keeps it unchanged.
        let a = core::mem::ManuallyDrop::new(unsafe { Arc::from_raw(self.ptr) });
        Arc::strong_count(&a)
    }
    /// A reference of the caller's own.
    pub fn arc(&self) -> Arc<T> {
        // SAFETY: as in `strong_count`; the clone takes its own count.
        let a = core::mem::ManuallyDrop::new(unsafe { Arc::from_raw(self.ptr) });
        Arc::clone(&a)
    }
}

impl<T> core::ops::Deref for Entry<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: as in `strong_count`.
        unsafe { &*self.ptr }
    }
}

/// (No bounds on the type itself: the drop-bounds gate; every method needs `T: Send + Sync + 'static`.)
pub struct RcuIndex<T> {
    lock: RwSpinLock<()>,
    /// The root node's address | height (0: empty).
    root: AtomicUsize,
    len: AtomicUsize,
    _t: PhantomData<Arc<T>>,
}

// SAFETY: the index owns Arc<T>s and nodes; values are shared across threads only as &T (T: Sync) and handed
// over as Arc<T> (T: Send); changes are serialised by the write lock, reads are atomic loads.
unsafe impl<T: Send + Sync + 'static> Send for RcuIndex<T> {}
// SAFETY: as above.
unsafe impl<T: Send + Sync + 'static> Sync for RcuIndex<T> {}

/// The tree as one walk sees it.
#[derive(Clone, Copy)]
struct Root {
    node: *mut Node,
    height: u32,
}

impl<T: Send + Sync + 'static> RcuIndex<T> {
    /// `lock`: the lock class of its lock (the page cache's `vfs.inode.index`).
    pub const fn named(lock: &'static str) -> Self {
        RcuIndex { lock: RwSpinLock::named(lock, ()), root: AtomicUsize::new(0), len: AtomicUsize::new(0), _t: PhantomData }
    }

    /// The slot of `i` at level 1, if the nodes on the way exist.
    fn leaf_slot(&self, i: u64) -> Option<&AtomicPtr<()>> {
        let r = self.root()?;
        if !fits(i, r.height) {
            return None;
        }
        let mut n = r.node;
        let mut level = r.height;
        loop {
            // SAFETY: `n` is a node reachable from the root now; it is freed only after a grace period (the
            // caller is in a section) or never while a guard of this index is held (writers are excluded).
            let slot = unsafe { &(*n).slots[digit(i, level)] };
            if level == 1 {
                return Some(slot);
            }
            let p = slot.load(Acquire);
            #[cfg(test)]
            assert_ne!(p as usize, POISON, "a reader walked a freed node");
            if p.is_null() {
                return None;
            }
            n = p as *mut Node;
            level -= 1;
        }
    }

    fn value(&self, i: u64) -> Option<*const T> {
        let p = self.leaf_slot(i)?.load(Acquire);
        #[cfg(test)]
        assert_ne!(p as usize, POISON, "a reader read a slot of a freed node");
        (!p.is_null()).then_some(p as *const T)
    }

    /// The lockless look-up: the value lives as long as the section (and the borrow of the index).
    pub fn get<'s>(&'s self, _s: &'s Section, i: u64) -> Option<&'s T> {
        // SAFETY: a value taken out of the index is dropped only after a grace period, which cannot end
        // while `_s` is open.
        self.value(i).map(|p| unsafe { &*p })
    }

    /// Values now (exact under a guard, a snapshot otherwise).
    pub fn len(&self) -> usize {
        self.len.load(Relaxed)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn read(&self) -> Locked<'_, T, ReadGuard<'_, ()>> {
        Locked { ix: self, _g: self.lock.read() }
    }
    pub fn write(&self) -> Locked<'_, T, WriteGuard<'_, ()>> {
        Locked { ix: self, _g: self.lock.write() }
    }

    /// The first present index at or after `from` (u128: the top level of an 11-level tree spans more than
    /// 64 bits).
    fn next_raw(&self, from: u64) -> Option<(u64, *const T)> {
        fn rec<T>(n: *mut Node, base: u128, level: u32, from: u64) -> Option<(u64, *const T)> {
            let span_bits = BITS * (level - 1);
            for d in 0..FAN {
                let lo = base + ((d as u128) << span_bits);
                if lo > u64::MAX as u128 {
                    return None;
                }
                let hi = lo + (1u128 << span_bits) - 1;
                if hi < from as u128 {
                    continue;
                }
                // SAFETY: as in `leaf_slot` (the caller holds a guard).
                let p = unsafe { (*n).slots[d].load(Acquire) };
                if p.is_null() {
                    continue;
                }
                if level == 1 {
                    return Some((lo as u64, p as *const T));
                }
                if let Some(r) = rec::<T>(p as *mut Node, lo, level - 1, from) {
                    return Some(r);
                }
            }
            None
        }
        let r = self.root()?;
        if !fits(from, r.height) {
            return None;
        }
        rec::<T>(r.node, 0, r.height, from)
    }
}

impl<T> RcuIndex<T> {
    fn root(&self) -> Option<Root> {
        let r = self.root.load(Acquire);
        (r != 0).then(|| Root { node: (r & !HEIGHT_MASK) as *mut Node, height: (r & HEIGHT_MASK) as u32 })
    }
}

impl<T> Drop for RcuIndex<T> {
    fn drop(&mut self) {
        // No reader can hold anything of it (their references borrow the index), so everything goes now.
        fn rec<T>(n: *mut Node, level: u32) {
            for d in 0..FAN {
                // SAFETY: `&mut self`: no other access; every node and value is owned by the tree.
                let p = unsafe { (*n).slots[d].load(Relaxed) };
                if p.is_null() {
                    continue;
                }
                if level == 1 {
                    // SAFETY: a leaf slot owns one strong count (`Arc::into_raw`).
                    drop(unsafe { Arc::from_raw(p as *const T) });
                } else {
                    rec::<T>(p as *mut Node, level - 1);
                }
            }
            // SAFETY: nodes come from `new_node` (Box::into_raw) and are freed once, here.
            drop(unsafe { alloc::boxed::Box::from_raw(n) });
        }
        if let Some(r) = self.root() {
            rec::<T>(r.node, r.height);
        }
    }
}

/// The index under its lock (`G`: the read or the write side).
pub struct Locked<'a, T, G> {
    ix: &'a RcuIndex<T>,
    _g: G,
}

impl<'a, T: Send + Sync + 'static, G> Locked<'a, T, G> {
    pub fn get(&self, i: u64) -> Option<Entry<'_, T>> {
        self.ix.value(i).map(|ptr| Entry { ptr, _l: PhantomData })
    }
    pub fn contains(&self, i: u64) -> bool {
        self.ix.value(i).is_some()
    }
    pub fn len(&self) -> usize {
        self.ix.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ix.is_empty()
    }
    pub fn next_from(&self, from: u64) -> Option<(u64, Entry<'_, T>)> {
        self.ix.next_raw(from).map(|(i, ptr)| (i, Entry { ptr, _l: PhantomData }))
    }
    /// Calls `f` for every entry in index order (stops when `f` returns false).
    pub fn for_each(&self, f: &mut dyn FnMut(u64, &Entry<'_, T>) -> bool) {
        let mut at = 0u64;
        while let Some((i, e)) = self.next_from(at) {
            if !f(i, &e) {
                return;
            }
            match i.checked_add(1) {
                Some(n) => at = n,
                None => return,
            }
        }
    }
}

impl<'a, T: Send + Sync + 'static> Locked<'a, T, WriteGuard<'a, ()>> {
    fn set_root(&self, node: *mut Node, height: u32) {
        let w = if node.is_null() { 0 } else { node as usize | height as usize };
        self.ix.root.store(w, Release);
    }

    /// Defers a node that no reader can find any more (the grace cookie is stamped now, so it must be unlinked
    /// before: a reader that enters after the stamp must not reach it).
    fn defer_unlinked(&self, n: *mut Node) {
        debug_assert!(self.ix.root().is_none_or(|r| r.node != n), "a node deferred while it is still the root");
        defer_node(n);
    }

    /// Room for an insert at `i`: grows the tree and makes the nodes on the way (all fallible). After `Ok`,
    /// `insert(i, ..)` cannot fail.
    pub fn prepare(&mut self, i: u64) -> Result<(), NoRoom> {
        // A new root is made at the height `i` needs and published once (never an empty root that is published
        // and replaced at once).
        let mut r = match self.ix.root() {
            Some(r) => r,
            None => {
                let n = new_node()?;
                let r = Root { node: n, height: height_for(i) };
                self.set_root(n, r.height);
                r
            }
        };
        if !fits(i, r.height) && node_empty(r.node) {
            // An empty root (left by a failed prepare) is replaced, not kept under the new one. Unpublished
            // first, deferred after: the grace cookie must be stamped once no new reader can find the node (a
            // reader that entered after a cookie taken while the node was still the root would not hold back
            // its grace period).
            let n = new_node()?;
            let old = r.node;
            r = Root { node: n, height: height_for(i) };
            self.set_root(n, r.height);
            self.defer_unlinked(old);
        }
        while !fits(i, r.height) {
            // The root is not empty here (a node with an empty root was replaced above; a grown root holds the
            // old one in slot 0).
            let n = new_node()?;
            // SAFETY: `n` is not published yet; the old root moves under it (it stays valid for readers).
            unsafe { (*n).slots[0].store(r.node as *mut (), Relaxed) };
            r = Root { node: n, height: r.height + 1 };
            self.set_root(n, r.height);
        }
        let mut n = r.node;
        let mut level = r.height;
        while level > 1 {
            // SAFETY: a node of the tree; the write guard keeps every other writer out.
            let slot = unsafe { &(*n).slots[digit(i, level)] };
            let mut p = slot.load(Acquire);
            if p.is_null() {
                p = new_node()? as *mut ();
                slot.store(p, Release);
            }
            n = p as *mut Node;
            level -= 1;
        }
        Ok(())
    }

    /// Puts `v` at `i`; the value that was there leaves through a grace period (returned for the caller to
    /// account and defer).
    pub fn insert(&mut self, i: u64, v: Arc<T>) -> Result<Option<Retire<T>>, NoRoom> {
        self.prepare(i)?;
        let slot = self.ix.leaf_slot(i).ok_or(NoRoom)?;
        let old = slot.swap(Arc::into_raw(v) as *mut (), AcqRel);
        if old.is_null() {
            self.ix.len.fetch_add(1, Relaxed);
            return Ok(None);
        }
        // SAFETY: the slot owned that strong count; it is handed over to the Retire.
        Ok(Some(unsafe { Retire::from_raw(old as *const T) }))
    }

    /// Takes the value at `i` out; empty nodes on the way leave too (after a grace period).
    pub fn remove(&mut self, i: u64) -> Option<Retire<T>> {
        let r = self.ix.root()?;
        if !fits(i, r.height) {
            return None;
        }
        let mut path: [(*mut Node, usize); MAX_LEVELS] = [(core::ptr::null_mut(), 0); MAX_LEVELS];
        let mut n = r.node;
        let mut level = r.height;
        let mut depth = 0usize;
        loop {
            let d = digit(i, level);
            path[depth] = (n, d);
            depth += 1;
            // SAFETY: as in `prepare`.
            let slot = unsafe { &(*n).slots[d] };
            if level == 1 {
                break;
            }
            let p = slot.load(Acquire);
            if p.is_null() {
                return None;
            }
            n = p as *mut Node;
            level -= 1;
        }
        let (leaf, d) = path[depth - 1];
        // SAFETY: as in `prepare`.
        let old = unsafe { (*leaf).slots[d].swap(core::ptr::null_mut(), AcqRel) };
        if old.is_null() {
            return None;
        }
        let left = self.ix.len.fetch_sub(1, Relaxed) - 1;
        if left == 0 {
            // The whole tree goes: unpublish the root, then its nodes (no values left, but a prepare may have
            // left empty branches) after a grace period.
            self.set_root(core::ptr::null_mut(), 0);
            defer_subtree(r.node, r.height);
        } else {
            // Prune empty nodes bottom-up (never the root).
            for k in (1..depth).rev() {
                let (node, _) = path[k];
                if !node_empty(node) {
                    break;
                }
                let (parent, pd) = path[k - 1];
                // SAFETY: as in `prepare`.
                unsafe { (*parent).slots[pd].store(core::ptr::null_mut(), Release) };
                self.defer_unlinked(node);
            }
        }
        // SAFETY: the slot owned that strong count; it is handed over.
        Some(unsafe { Retire::from_raw(old as *const T) })
    }

    /// Removes every entry at or after `from`, handing each to `f` in index order. Never allocates (index
    /// nodes leave through the grace queue, which may).
    pub fn drain_from(&mut self, from: u64, f: &mut dyn FnMut(u64, Retire<T>)) {
        while let Some((i, _)) = self.ix.next_raw(from) {
            if let Some(v) = self.remove(i) {
                f(i, v);
            }
        }
    }
}

fn node_empty(n: *mut Node) -> bool {
    // SAFETY: a node of the tree, read under the write guard.
    unsafe { (*n).slots.iter().all(|s| s.load(Relaxed).is_null()) }
}

/// An unlinked, empty node: freed after a grace period.
fn defer_node(n: *mut Node) {
    // SAFETY: the node came from `new_node`, is unlinked (no new reader finds it) and owns no slot values; the
    // Box is dropped once, by the grace queue.
    grace::defer_box(unsafe { alloc::boxed::Box::from_raw(n) }, 0);
}

/// An unlinked subtree without values: every node freed after a grace period.
fn defer_subtree(n: *mut Node, level: u32) {
    if level > 1 {
        for d in 0..FAN {
            // SAFETY: a node of the unlinked tree, under the write guard.
            let p = unsafe { (*n).slots[d].load(Relaxed) };
            if !p.is_null() {
                defer_subtree(p as *mut Node, level - 1);
            }
        }
    }
    defer_node(n);
}
