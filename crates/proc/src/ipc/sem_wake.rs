//! The operations themselves and who runs next (linux-7.3.1/ipc/sem.c:627 `perform_atomic_semop_slow`, :700
//! `perform_atomic_semop`, :835 `wake_const_ops`, :942 `update_queue`, :1050 `do_smart_update`, :304 `merge_queues`,
//! :276 `unmerge_queues`). All under the array's lock.

use super::sem::{Queue, SemBuf, SemInner};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use uapi::ab::sysv::*;
use uapi::errno::*;
use vfs::Errno;

/// Which pending list.
#[derive(Clone, Copy)]
enum Pending {
    All,
    Sem(usize),
}

const ERANGE_I: i32 = -(ERANGE as i32);
const EAGAIN_I: i32 = -(EAGAIN as i32);
const EIDRM_I: i32 = -(EIDRM as i32);

impl SemInner {
    pub(super) fn undo_at(&self, id: u64) -> Option<usize> {
        self.undo.binary_search_by_key(&id, |e| e.0).ok()
    }

    /// 0 done, 1 must wait, or a negative errno; a would-block position goes to `blocking`.
    pub(super) fn perform(&mut self, sops: &[SemBuf], undo: Option<u64>, pid: u32, dupsop: bool, blocking: &mut usize) -> i32 {
        let un = undo.and_then(|u| self.undo_at(u));
        if dupsop {
            return self.perform_slow(sops, un, pid, blocking);
        }
        // Two passes: nothing is written unless the whole list can go through.
        for (i, s) in sops.iter().enumerate() {
            let val = self.sems[s.num as usize].val;
            if s.op == 0 && val != 0 || val + (s.op as i32) < 0 {
                *blocking = i;
                return if s.flg & IPC_NOWAIT as i16 != 0 { EAGAIN_I } else { 1 };
            }
            if val + s.op as i32 > SEMVMX {
                return ERANGE_I;
            }
            if s.flg & SEM_UNDO != 0
                && let Some(u) = un
                && !adj_fits(self.undo[u].1[s.num as usize] as i32 - s.op as i32)
            {
                return ERANGE_I;
            }
        }
        for s in sops {
            if s.flg & SEM_UNDO != 0
                && let Some(u) = un
            {
                let a = &mut self.undo[u].1[s.num as usize];
                *a = (*a as i32 - s.op as i32) as i16;
            }
            let c = &mut self.sems[s.num as usize];
            c.val += s.op as i32;
            c.pid = pid;
        }
        0
    }

    /// A list that names one semaphore twice: each step sees the ones before it, and is rolled back on failure.
    fn perform_slow(&mut self, sops: &[SemBuf], un: Option<usize>, pid: u32, blocking: &mut usize) -> i32 {
        let mut done = 0;
        let mut result = 0;
        for (i, s) in sops.iter().enumerate() {
            let val = self.sems[s.num as usize].val;
            let next = val + s.op as i32;
            if s.op == 0 && val != 0 || next < 0 {
                *blocking = i;
                result = if s.flg & IPC_NOWAIT as i16 != 0 { EAGAIN_I } else { 1 };
                break;
            }
            if next > SEMVMX {
                result = ERANGE_I;
                break;
            }
            if s.flg & SEM_UNDO != 0
                && let Some(u) = un
            {
                let a = self.undo[u].1[s.num as usize] as i32 - s.op as i32;
                if !adj_fits(a) {
                    result = ERANGE_I;
                    break;
                }
                self.undo[u].1[s.num as usize] = a as i16;
            }
            self.sems[s.num as usize].val = next;
            done = i + 1;
        }
        if result == 0 {
            for s in sops {
                self.sems[s.num as usize].pid = pid;
            }
            return 0;
        }
        for s in sops[..done].iter().rev() {
            self.sems[s.num as usize].val -= s.op as i32;
            if s.flg & SEM_UNDO != 0
                && let Some(u) = un
            {
                let a = &mut self.undo[u].1[s.num as usize];
                *a = (*a as i32 + s.op as i32) as i16;
            }
        }
        result
    }

    fn try_queue(&mut self, q: &Queue) -> i32 {
        let mut blocking = q.blocking.load(Ordering::Relaxed);
        let r = self.perform(&q.sops, q.undo, q.pid, q.dupsop, &mut blocking);
        q.blocking.store(blocking, Ordering::Relaxed);
        r
    }

    fn list(&mut self, which: Pending, alter: bool) -> &mut Vec<Arc<Queue>> {
        match (which, alter) {
            (Pending::All, true) => &mut self.alter,
            (Pending::All, false) => &mut self.zero,
            (Pending::Sem(i), true) => &mut self.sems[i].alter,
            (Pending::Sem(i), false) => &mut self.sems[i].zero,
        }
    }

    /// `wake_const_ops`: the wait-for-zero list; true when one completed.
    fn wake_const(&mut self, which: Pending) -> bool {
        let (mut i, mut completed) = (0, false);
        while let Some(q) = self.list(which, false).get(i).cloned() {
            let e = self.try_queue(&q);
            if e > 0 {
                i += 1;
                continue;
            }
            self.list(which, false).remove(i);
            if q.sops.len() > 1 {
                self.complex -= 1;
            }
            q.finish(e);
            completed |= e == 0;
        }
        completed
    }

    /// `do_smart_wakeup_zero`.
    fn wakeup_zero(&mut self, sops: Option<&[SemBuf]>) -> bool {
        let (mut completed, mut got_zero) = (false, false);
        match sops {
            Some(sops) => {
                for s in sops {
                    let n = s.num as usize;
                    if self.sems[n].val == 0 {
                        got_zero = true;
                        completed |= self.wake_const(Pending::Sem(n));
                    }
                }
            }
            None => {
                for n in 0..self.sems.len() {
                    if self.sems[n].val == 0 {
                        got_zero = true;
                        completed |= self.wake_const(Pending::Sem(n));
                    }
                }
            }
        }
        if got_zero {
            completed |= self.wake_const(Pending::All);
        }
        completed
    }

    /// `update_queue`: the altering waiters of one list in order; after a completed operation that may have
    /// changed what the ones before it wait for, from the start again (`check_restart`).
    fn update_queue(&mut self, which: Pending) -> bool {
        let (mut i, mut completed) = (0, false);
        while let Some(q) = self.list(which, true).get(i).cloned() {
            if let Pending::Sem(n) = which
                && self.sems[n].val == 0
            {
                break;
            }
            let e = self.try_queue(&q);
            if e > 0 {
                i += 1;
                continue;
            }
            self.list(which, true).remove(i);
            if q.sops.len() > 1 {
                self.complex -= 1;
            }
            let restart = e == 0 && {
                completed = true;
                self.wakeup_zero(Some(&q.sops));
                !self.alter.is_empty() || q.sops.len() > 1
            };
            q.finish(e);
            if restart {
                i = 0;
            }
        }
        completed
    }

    pub(super) fn set_otime(&mut self, sops: Option<&[SemBuf]>) {
        let n = sops.and_then(|s| s.first()).map_or(0, |s| s.num as usize);
        self.sems[n].otime = super::now_secs();
    }

    /// `do_smart_update`: after a change of values. `sops`: the operation, None for SETVAL, SETALL, undo.
    pub(super) fn smart_update(&mut self, sops: Option<&[SemBuf]>, mut otime: bool) {
        otime |= self.wakeup_zero(sops);
        if !self.alter.is_empty() {
            otime |= self.update_queue(Pending::All);
        } else {
            match sops {
                Some(sops) => {
                    for s in sops {
                        if s.op > 0 {
                            otime |= self.update_queue(Pending::Sem(s.num as usize));
                        }
                    }
                }
                None => {
                    for n in 0..self.sems.len() {
                        otime |= self.update_queue(Pending::Sem(n));
                    }
                }
            }
        }
        if otime {
            self.set_otime(sops);
        }
    }

    /// `merge_queues`: `list_splice_init` puts each semaphore's list in front of the global one.
    fn merge(&mut self) -> Result<(), Errno> {
        let waiting: usize = self.sems.iter().map(|s| s.alter.len()).sum();
        if waiting == 0 {
            return Ok(());
        }
        let mut all = Vec::new();
        all.try_reserve_exact(waiting + self.alter.len()).map_err(super::nomem)?;
        for s in self.sems.iter_mut().rev() {
            all.append(&mut s.alter);
        }
        all.append(&mut self.alter);
        self.alter = all;
        Ok(())
    }

    /// The queue of a call that must wait (sem.c:2112).
    pub(super) fn enqueue(&mut self, q: Arc<Queue>) -> Result<(), Errno> {
        if q.sops.len() == 1 {
            let n = q.sops[0].num as usize;
            let l = match (q.alter, self.complex != 0) {
                (true, true) => &mut self.alter,
                (true, false) => &mut self.sems[n].alter,
                (false, _) => &mut self.sems[n].zero,
            };
            l.try_reserve(1).map_err(super::nomem)?;
            l.push(q);
            return Ok(());
        }
        if self.complex == 0 {
            self.merge()?;
        }
        let l = if q.alter { &mut self.alter } else { &mut self.zero };
        l.try_reserve(1).map_err(super::nomem)?;
        l.push(q);
        self.complex += 1;
        Ok(())
    }

    /// `unlink_queue` of a waiter that gives up.
    pub(super) fn unlink(&mut self, q: &Arc<Queue>) {
        let same = |e: &Arc<Queue>| Arc::ptr_eq(e, q);
        let mut gone = false;
        let mut take = |l: &mut Vec<Arc<Queue>>| {
            if !gone && let Some(i) = l.iter().position(same) {
                l.remove(i);
                gone = true;
            }
        };
        take(&mut self.alter);
        take(&mut self.zero);
        if q.sops.len() == 1 {
            let s = &mut self.sems[q.sops[0].num as usize];
            take(&mut s.alter);
            take(&mut s.zero);
        }
        if gone && q.sops.len() > 1 {
            self.complex -= 1;
        }
    }

    /// `unmerge_queues`, what every `sem_unlock` does: no complex operation left, so the global list goes back to the
    /// semaphores'. Without room it stays global, which only keeps the merged order for longer.
    pub(super) fn settle(&mut self) {
        if self.complex != 0 || self.alter.is_empty() {
            return;
        }
        let list = core::mem::take(&mut self.alter);
        for q in &list {
            if self.sems[q.sops[0].num as usize].alter.try_reserve(list.len()).is_err() {
                self.alter = list;
                return;
            }
        }
        for q in list {
            self.sems[q.sops[0].num as usize].alter.push(q);
        }
    }

    /// `freeary`'s queues: every waiter gets EIDRM, in Linux's order.
    pub(super) fn free_queues(&mut self) {
        fn fail(l: &mut Vec<Arc<Queue>>) {
            for q in l.drain(..) {
                q.finish(EIDRM_I);
            }
        }
        fail(&mut self.zero);
        fail(&mut self.alter);
        for s in &mut self.sems {
            fail(&mut s.zero);
            fail(&mut s.alter);
        }
        self.complex = 0;
    }
}

/// sem.c:657: what an undo value may be.
fn adj_fits(a: i32) -> bool {
    (-SEMAEM - 1..=SEMAEM).contains(&a)
}
