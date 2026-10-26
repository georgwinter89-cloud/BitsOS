//! FS-12 (B4): the I/O priority of a request comes from the thread that submits it, and `ioprio_set`/
//! `ioprio_get` (linux-6.6/block/ioprio.c) are mapped onto it.
//!
//! - No ioprio set (class NONE): the scheduling class decides: KernelRt and Interactive -> Interactive, Normal
//!   -> Normal, Background (kernel helpers, SCHED_IDLE programs) -> Background (Linux `task_nice_ioclass`).
//! - IOPRIO_CLASS_IDLE -> Background, IOPRIO_CLASS_BE -> Normal (levels are kept and reported, not used).
//! - IOPRIO_CLASS_RT -> Interactive only while the thread's scheduling class is Interactive or KernelRt (granted
//!   by policy, SC-7a), else Normal: the same rule as SCHED_FIFO in SC-7a, so a program cannot raise its disk
//!   requests over the desktop's by itself. Lowering always works.
//!
//! The value a program set lives here per scheduler thread id (Linux keeps it in the task's `io_context`); a
//! new thread gets its creator's value through `inherit` (`__copy_io`). Entries of ended threads are dropped
//! at the next `set`. The table is empty unless some program called ioprio_set, and the per-request lookup
//! skips the lock then.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
pub use crate::lift::{floor, Floor, Held, Lift, Waiting};
pub use devmodel::block::IoPriority;
use ksync::SpinLock;
use sched::Class;
use uapi::errno::{EINVAL, ENOMEM, EPERM};

pub const IOPRIO_CLASS_SHIFT: u32 = 13;
pub const IOPRIO_CLASS_NONE: u32 = 0;
pub const IOPRIO_CLASS_RT: u32 = 1;
pub const IOPRIO_CLASS_BE: u32 = 2;
pub const IOPRIO_CLASS_IDLE: u32 = 3;
/// Levels per class (`IOPRIO_NR_LEVELS`).
pub const IOPRIO_NR_LEVELS: u32 = 8;
/// `IOPRIO_BE_NORM`.
pub const IOPRIO_NORM: u32 = 4;
pub const IOPRIO_WHO_PROCESS: i32 = 1;
pub const IOPRIO_WHO_PGRP: i32 = 2;
pub const IOPRIO_WHO_USER: i32 = 3;
/// `IOPRIO_DEFAULT` = IOPRIO_PRIO_VALUE(IOPRIO_CLASS_NONE, 0) (linux-6.6/include/linux/ioprio.h:14).
pub const IOPRIO_DEFAULT: i32 = 0;

/// `IOPRIO_PRIO_VALUE(class, data)`.
pub const fn value(class: u32, data: u32) -> i32 {
    ((class << IOPRIO_CLASS_SHIFT) | (data & ((1 << IOPRIO_CLASS_SHIFT) - 1))) as i32
}

/// `IOPRIO_PRIO_CLASS`.
pub const fn class_of(ioprio: i32) -> u32 {
    ((ioprio as u32) >> IOPRIO_CLASS_SHIFT) & 7
}

/// `IOPRIO_PRIO_LEVEL`.
pub const fn level_of(ioprio: i32) -> u32 {
    (ioprio as u32) & (IOPRIO_NR_LEVELS - 1)
}

/// `ioprio_check_cap` (linux-6.6/block/ioprio.c:36): RT needs CAP_SYS_ADMIN or CAP_SYS_NICE (`privileged`;
/// EPERM), RT/BE a level below 8 (only the low three bits count, hint bits are not checked), NONE level 0, IDLE
/// anything; every other class EINVAL. Returns the negative errno.
pub fn check(ioprio: i32, privileged: bool) -> Result<(), i64> {
    let level = level_of(ioprio);
    match class_of(ioprio) {
        IOPRIO_CLASS_RT if !privileged => Err(-EPERM),
        IOPRIO_CLASS_RT | IOPRIO_CLASS_BE if level >= IOPRIO_NR_LEVELS => Err(-EINVAL),
        IOPRIO_CLASS_RT | IOPRIO_CLASS_BE | IOPRIO_CLASS_IDLE => Ok(()),
        IOPRIO_CLASS_NONE if level != 0 => Err(-EINVAL),
        IOPRIO_CLASS_NONE => Ok(()),
        _ => Err(-EINVAL),
    }
}

/// The priority requests of a thread in scheduling class `class` with ioprio `ioprio` get (module text).
pub fn map(class: Class, ioprio: i32) -> IoPriority {
    let granted = matches!(class, Class::KernelRt | Class::Interactive);
    match class_of(ioprio) {
        IOPRIO_CLASS_IDLE => IoPriority::Background,
        IOPRIO_CLASS_BE => IoPriority::Normal,
        IOPRIO_CLASS_RT if granted => IoPriority::Interactive,
        IOPRIO_CLASS_RT => IoPriority::Normal,
        _ => match class {
            Class::KernelRt | Class::Interactive => IoPriority::Interactive,
            Class::Normal => IoPriority::Normal,
            Class::Background => IoPriority::Background,
        },
    }
}

/// Values programs set, as (scheduler thread id, ioprio), sorted by id.
static TABLE: SpinLock<Vec<(u64, i32)>> = SpinLock::named("block.ioprio", Vec::new());
/// Entries in `TABLE` (read without the lock on every request).
static ENTRIES: AtomicUsize = AtomicUsize::new(0);

/// The raw value thread `tid` set, IOPRIO_DEFAULT when none (`get_task_raw_ioprio`).
pub fn raw(tid: u64) -> i32 {
    if ENTRIES.load(Ordering::Acquire) == 0 {
        return IOPRIO_DEFAULT;
    }
    let t = TABLE.lock();
    t.binary_search_by_key(&tid, |e| e.0).map(|i| t[i].1).unwrap_or(IOPRIO_DEFAULT)
}

/// `set_task_ioprio` after the caller's checks (`check`, owner and B3 policy); IOPRIO_DEFAULT removes the
/// entry. ENOMEM when the table cannot grow.
pub fn set(tid: u64, ioprio: i32) -> Result<(), i64> {
    prune(tid);
    let mut t = TABLE.lock();
    match t.binary_search_by_key(&tid, |e| e.0) {
        Ok(i) if ioprio == IOPRIO_DEFAULT => {
            t.remove(i);
        }
        Ok(i) => t[i].1 = ioprio,
        Err(_) if ioprio == IOPRIO_DEFAULT => {}
        Err(i) => {
            if t.try_reserve(1).is_err() {
                ENTRIES.store(t.len(), Ordering::Release);
                return Err(-ENOMEM);
            }
            t.insert(i, (tid, ioprio));
        }
    }
    ENTRIES.store(t.len(), Ordering::Release);
    drop(t);
    klog::event!(Debug, "block", "ioprio.set", tid = tid, class = class_of(ioprio), level = level_of(ioprio));
    Ok(())
}

/// Drops the entries of ended threads (ids are never reused, so this is only about memory). The liveness check
/// takes the scheduler's lock, so it runs without `block.ioprio` held; skipped when the heap refuses the copy.
fn prune(keep: u64) {
    let ids: Vec<u64> = {
        let t = TABLE.lock();
        let mut v = Vec::new();
        if v.try_reserve_exact(t.len()).is_err() {
            return;
        }
        v.extend(t.iter().map(|e| e.0).filter(|&id| id != keep));
        v
    };
    let dead: Vec<u64> = ids.into_iter().filter(|&id| sched::attr::nice(id).is_none()).collect();
    if dead.is_empty() {
        return;
    }
    let mut t = TABLE.lock();
    t.retain(|e| !dead.contains(&e.0));
    ENTRIES.store(t.len(), Ordering::Release);
}

/// A new thread `to` gets `from`'s value (fork/clone, Linux `__copy_io` copies a valid ioprio). Known
/// difference: clone(CLONE_IO) shares Linux's io_context, so a later ioprio_set of one thread changes the
/// other's too; here the value is copied once (glibc's pthread_create does not pass CLONE_IO).
pub fn inherit(from: u64, to: u64) {
    let v = raw(from);
    if class_of(v) != IOPRIO_CLASS_NONE && set(to, v).is_err() {
        klog::event!(Warn, "block", "ioprio.inherit_lost", from = from, to = to);
    }
}

/// `__get_task_ioprio` for IOPRIO_WHO_PGRP/USER: the set value, or one derived from the scheduling policy
/// and nice (`task_nice_ioclass`/`task_nice_ioprio`). None for an ended thread.
pub fn task_ioprio(tid: u64) -> Option<i32> {
    let nice = sched::attr::nice(tid)?;
    let (policy, _) = sched::attr::policy(tid)?;
    let v = raw(tid);
    if class_of(v) != IOPRIO_CLASS_NONE {
        return Some(v);
    }
    let class = match policy {
        sched::attr::Policy::Idle => IOPRIO_CLASS_IDLE,
        // Linux: any `task_is_realtime` (FIFO, RR and DEADLINE); a Deadline policy, once sched::attr has
        // one, belongs here too.
        sched::attr::Policy::Fifo | sched::attr::Policy::Rr => IOPRIO_CLASS_RT,
        _ => IOPRIO_CLASS_BE,
    };
    Some(value(class, ((nice + 20) / 5) as u32))
}

/// `ioprio_best`: the smaller value wins (RT before BE before IDLE).
pub fn best(a: i32, b: i32) -> i32 {
    (a as u16).min(b as u16) as i32
}

/// The priority of a request the calling thread submits now. Normal outside a scheduler thread (host tests,
/// early boot).
/// A floor the thread holds (`lift::floor`) raises it.
pub fn current() -> IoPriority {
    let own = match sched::current_thread() {
        Some(t) => map(t.class(), raw(t.id)),
        None => IoPriority::Normal,
    };
    match crate::lift::floor_of(ksync::thread_id()) {
        Some(f) => higher(own, f),
        None => own,
    }
}

/// FS-13: Interactive or Normal requests queued on any disk now; the Background preload waits while one is.
pub fn foreground_waiting() -> u64 {
    crate::queue::foreground_queued()
}

/// The higher of two priorities.
pub fn higher(a: IoPriority, b: IoPriority) -> IoPriority {
    a.min(b)
}
