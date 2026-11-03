//! PF-5: `Retire`, the reference a lockless index gives up (dropped only through the grace queue).

use super::queue::{arc_held, drop_arc, push, Deferred};
use alloc::sync::Arc;

/// One strong reference of an `Arc<T>` that a lockless index gave up: readers in sections may still use
/// the value, so the reference is dropped only after a grace period (`defer`, or its `Drop`). Clones of the
/// `Arc` may be taken (they only keep the value longer).
/// (No bounds on the type itself: a `Drop` with bounds is refused by the drop-bounds gate, rust-lang/rust#164250;
/// a `Retire` is only made by `rcu_index`, whose values are `Send + Sync + 'static`.)
pub struct Retire<T> {
    ptr: *const T,
}

// SAFETY: it owns one strong count of an Arc<T> with T: Send + Sync.
unsafe impl<T: Send + Sync + 'static> Send for Retire<T> {}
// SAFETY: as above; shared access only reads T.
unsafe impl<T: Send + Sync + 'static> Sync for Retire<T> {}

impl<T: Send + Sync + 'static> Retire<T> {
    /// SAFETY: `ptr` came from `Arc::into_raw` and the caller hands over that strong count.
    pub(crate) unsafe fn from_raw(ptr: *const T) -> Retire<T> {
        Retire { ptr }
    }
    /// Strong references now (this one included).
    pub fn strong_count(&self) -> usize {
        // SAFETY: `ptr` holds a strong count, so the Arc is alive; ManuallyDrop keeps the count unchanged.
        let a = core::mem::ManuallyDrop::new(unsafe { Arc::from_raw(self.ptr) });
        Arc::strong_count(&a)
    }
    /// A new reference (keeps the value alive past this one's deferred drop).
    pub fn arc(&self) -> Arc<T> {
        // SAFETY: as in `strong_count`; the clone takes a count of its own.
        let a = core::mem::ManuallyDrop::new(unsafe { Arc::from_raw(self.ptr) });
        Arc::clone(&a)
    }
    /// Hands the reference to the grace queue; `bytes` as `defer_arc`. Above the bound the caller waits for
    /// its grace period (when it holds no spin lock).
    pub fn defer(self, bytes: u64) {
        let p = self.ptr;
        core::mem::forget(self);
        push(Deferred { cookie: 0, ptr: p as *mut (), drop: drop_arc::<T>, bytes, held: arc_held::<T>() }, true);
    }
    /// `defer` that never waits: for reclaim, which runs under a cache's lock that other reclaimers try (the
    /// allocation that asked waits afterwards, `expedite`, with no cache lock held).
    pub fn defer_nowait(self, bytes: u64) {
        let p = self.ptr;
        core::mem::forget(self);
        push(Deferred { cookie: 0, ptr: p as *mut (), drop: drop_arc::<T>, bytes, held: arc_held::<T>() }, false);
    }
}

impl<T: Send + Sync + 'static> core::ops::Deref for Retire<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the strong count this owns keeps the value alive while `self` is borrowed.
        unsafe { &*self.ptr }
    }
}

impl<T> Drop for Retire<T> {
    fn drop(&mut self) {
        // T is Send + Sync + 'static (only `rcu_index` makes a Retire, `from_raw`), so the queue may drop it on
        // any thread, later.
        push(Deferred { cookie: 0, ptr: self.ptr as *mut (), drop: drop_arc::<T>, bytes: 0, held: arc_held::<T>() }, true);
    }
}
