//! SEM_UNDO (linux-7.3.1/ipc/sem.c:1877 `find_alloc_undo`, :2366 `copy_semundo`, :2391 `exit_sem`). A list is shared by
//! the threads of CLONE_SYSVSEM; its values live in the arrays it touched (Linux's `sem_undo`, reached there from both
//! sides), so an array's end takes them along and the list only remembers which arrays to visit.

use super::sem::SemArray;
use super::Caller;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::SpinLock;
use uapi::ab::sysv::EIDRM;
use vfs::Errno;

static NEXT_LIST: AtomicU64 = AtomicU64::new(1);
klog::counter!(APPLIED, "ipc.sem.undo_applied");

/// `struct sem_undo_list`.
pub struct UndoList {
    pub id: u64,
    arrays: SpinLock<Vec<Arc<SemArray>>>,
}

impl UndoList {
    fn new() -> Arc<UndoList> {
        Arc::new(UndoList { id: NEXT_LIST.fetch_add(1, Ordering::Relaxed), arrays: SpinLock::named("ipc.undo", Vec::new()) })
    }
    /// An array joins; arrays that went meanwhile leave, so a list never grows with removed arrays.
    fn remember(&self, a: &Arc<SemArray>) -> Result<(), Errno> {
        let mut cur = core::mem::take(&mut *self.arrays.lock());
        cur.retain(|x| x.alive());
        let room = cur.try_reserve(1);
        if room.is_ok() {
            cur.push(a.clone());
        }
        let mut g = self.arrays.lock();
        // A sibling thread may have added its own while this list was out.
        if !g.is_empty() && cur.try_reserve(g.len()).is_ok() {
            cur.append(&mut g);
        }
        *g = cur;
        room.map_err(super::nomem)
    }
}

/// `task_struct.sysvsem`.
pub struct UndoSlot(SpinLock<Option<Arc<UndoList>>>);

impl UndoSlot {
    pub(crate) const fn new() -> UndoSlot {
        UndoSlot(SpinLock::named("ipc.undoslot", None))
    }
    /// `get_undo_list`.
    fn get(&self) -> Arc<UndoList> {
        self.0.lock().get_or_insert_with(UndoList::new).clone()
    }
    /// `copy_semundo`: what a new thread starts with (the creator's list with CLONE_SYSVSEM, else none).
    pub(crate) fn inherit(&self, creator: &UndoSlot, clone_sysvsem: bool) {
        *self.0.lock() = clone_sysvsem.then(|| creator.get());
    }
    /// What kcmp(KCMP_SYSVSEM) compares.
    pub fn list_ptr(&self) -> usize {
        self.0.lock().as_ref().map_or(0, |l| Arc::as_ptr(l) as usize)
    }
}

/// `find_alloc_undo`: the list of the caller and its values for `semid`, made when missing. EINVAL for a bad id,
/// EIDRM when it went meanwhile.
pub(super) fn find_alloc(c: &Caller<'_>, slot: &UndoSlot, semid: i32) -> Result<Arc<UndoList>, Errno> {
    let list = slot.get();
    let a = c.ns.tables.sem.ids.read().obtain_check(semid)?;
    if a.inner.lock().undo_at(list.id).is_some() {
        return Ok(list);
    }
    let mut adj = Vec::new();
    adj.try_reserve_exact(a.nsems).map_err(super::nomem)?;
    adj.resize(a.nsems, 0i16);
    let new = {
        let mut g = a.inner.lock();
        if g.perm.deleted {
            return Err(EIDRM);
        }
        match g.undo.binary_search_by_key(&list.id, |e| e.0) {
            Ok(_) => false,
            Err(at) => {
                g.undo.try_reserve(1).map_err(super::nomem)?;
                g.undo.insert(at, (list.id, adj));
                true
            }
        }
    };
    if new {
        list.remember(&a)?;
    }
    Ok(list)
}

/// `exit_sem`: drops the slot's reference; the last one applies the values, newest array first (Linux's list order
/// without its move-to-front on every operation). Values are clamped into 0..=SEMVMX, sempid becomes the leaver.
pub(crate) fn exit_sem(slot: &UndoSlot, tgid: u32) {
    let taken = slot.0.lock().take();
    let Some(list) = taken.and_then(Arc::into_inner) else { return };
    let arrays = core::mem::take(&mut *list.arrays.lock());
    for a in arrays.iter().rev() {
        let mut g = a.inner.lock();
        if g.perm.deleted {
            continue;
        }
        let Ok(i) = g.undo.binary_search_by_key(&list.id, |e| e.0) else { continue };
        let (_, adj) = g.undo.remove(i);
        for (s, &v) in g.sems.iter_mut().zip(&adj) {
            if v != 0 {
                s.val = (s.val + v as i32).clamp(0, uapi::ab::sysv::SEMVMX);
                s.pid = tgid;
            }
        }
        g.smart_update(None, true);
        g.settle();
        APPLIED.inc();
    }
}
