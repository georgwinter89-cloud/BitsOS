//! The thread table (SC-3): every thread by id, for wake-ups by `ThreadRef`, listings and the reaper.
//!
//! Interrupt-time wake-ups look threads up here, so the table's lock is an `IrqSpinLock` under which nothing is
//! ever allocated or freed: the vector grows outside the lock (a bigger one is made first and the entries moved
//! over), and a removed entry is dropped by the caller after the lock is released. A hard-interrupt handler only
//! clones an `Arc` out of it; the last reference of an exited thread is dropped by the reaper after a sync
//! generation, never in a handler (`reap`).

use crate::Thread;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::IrqSpinLock;

/// Sorted by thread id (ids only grow, so new threads go to the end).
static TABLE: IrqSpinLock<Vec<Arc<Thread>>> = IrqSpinLock::named("sched", Vec::new());

/// Adds a thread. `Err`: no memory to grow the table.
pub(crate) fn insert(t: Arc<Thread>) -> Result<(), Arc<Thread>> {
    loop {
        let need = {
            let mut v = TABLE.lock();
            if v.len() < v.capacity() {
                let pos = v.partition_point(|x| x.id < t.id);
                v.insert(pos, t);
                return Ok(());
            }
            v.capacity().max(32) * 2
        };
        let mut fresh: Vec<Arc<Thread>> = Vec::new();
        if fresh.try_reserve_exact(need).is_err() {
            return Err(t);
        }
        let old = {
            let mut v = TABLE.lock();
            if v.capacity() < need {
                // Moves the Arcs into memory reserved above: no allocation under the lock.
                fresh.extend(v.drain(..));
                core::mem::replace(&mut *v, fresh)
            } else {
                fresh
            }
        };
        // The emptied (or unused) vector is freed here, outside the lock.
        drop(old);
    }
}

/// Removes thread `id`; the caller drops the returned reference outside any scheduler lock.
pub(crate) fn remove(id: u64) -> Option<Arc<Thread>> {
    let mut v = TABLE.lock();
    let i = v.binary_search_by_key(&id, |t| t.id).ok()?;
    Some(v.remove(i))
}

/// The thread with this id (IRQ-safe: a clone, nothing allocated).
pub(crate) fn get(id: u64) -> Option<Arc<Thread>> {
    let v = TABLE.lock();
    v.binary_search_by_key(&id, |t| t.id).ok().map(|i| v[i].clone())
}

/// As `get`, but gives up if the table is locked (fault and panic paths).
pub(crate) fn try_get(id: u64) -> Option<Arc<Thread>> {
    let v = TABLE.try_lock()?;
    v.binary_search_by_key(&id, |t| t.id).ok().map(|i| v[i].clone())
}

/// The first thread after id `after` for which `pred` holds (a scan under the lock, nothing allocated).
pub(crate) fn next_where(after: u64, pred: impl Fn(&Thread) -> bool) -> Option<Arc<Thread>> {
    let v = TABLE.lock();
    let i = v.partition_point(|t| t.id <= after);
    v[i..].iter().find(|t| pred(t)).cloned()
}

pub(crate) fn len() -> usize {
    TABLE.lock().len()
}

/// Every thread (the copy is made in memory reserved outside the lock).
pub(crate) fn snapshot() -> Vec<Arc<Thread>> {
    let mut out: Vec<Arc<Thread>> = Vec::new();
    loop {
        let n = len();
        if out.try_reserve(n + 8).is_err() {
            return out;
        }
        {
            let v = TABLE.lock();
            if v.len() <= out.capacity() {
                out.extend(v.iter().cloned());
                break;
            }
        }
    }
    out
}
