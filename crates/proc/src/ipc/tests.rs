//! SX-IPC host tests: ids as Linux hands them out, and the queue order of sem.c on one array without threads.

use super::ids::{Ids, Object, Perm};
use super::sem::{Queue, SemArray, SemBuf};
use crate::cred::Cred;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;

struct Obj(SpinLock<Perm>);

impl Object for Obj {
    fn with_perm<R>(&self, f: impl FnOnce(&mut Perm) -> R) -> R {
        f(&mut self.0.lock())
    }
}

fn obj(key: i32) -> Arc<Obj> {
    Arc::new(Obj(SpinLock::named("ipc.test", Perm::new(key, 0o600, &Cred::new(1000, 1000)))))
}

#[test]
fn ids_cycle_like_linux() {
    let mut ids: Ids<Obj> = Ids::new();
    assert_eq!(ids.max_idx(), -1);
    assert_eq!(ids.add(&obj(0), 100), Ok(0));
    assert_eq!(ids.add(&obj(0), 100), Ok(1));
    assert_eq!(ids.max_idx(), 1);
    ids.remove(0, 0);
    assert_eq!(ids.add(&obj(0), 100), Ok(2), "the cursor moves on");
    assert!(ids.obtain_check(0).is_err());
    assert!(ids.obtain_check(1).is_ok());
    assert!(ids.obtain_check(1 + (1 << 15)).is_err(), "a stale sequence number");
    assert!(ids.obtain(1 + (1 << 15)).is_ok(), "the index alone");
    ids.remove(2, 0);
    assert_eq!(ids.max_idx(), 1, "the slots end at the last object");
    // Through the 64 of the minimum cycle: the sequence number grows when an index comes back.
    let mut last = 0;
    for _ in 3..=64 {
        last = ids.add(&obj(0), 1000).unwrap();
        ids.remove(last, 0);
    }
    assert_eq!(last, 1 << 15, "after index 63 the next is index 0 again, with the next sequence number");
}

#[test]
fn ids_limit_and_keys() {
    let mut ids: Ids<Obj> = Ids::new();
    assert_eq!(ids.add(&obj(7), 2), Ok(0));
    assert_eq!(ids.add(&obj(9), 2), Ok(1));
    assert_eq!(ids.add(&obj(11), 2), Err(uapi::errno::ENOSPC));
    assert!(ids.find_key_for_test(7).is_some());
    ids.remove(0, 7);
    assert!(ids.find_key_for_test(7).is_none());
    assert!(ids.find_key_for_test(9).is_some());
    ids.drop_key(9);
    assert!(ids.find_key_for_test(9).is_none());
    ids.next_id = (5 << 15) + 40;
    assert_eq!(ids.add(&obj(0), 100), Ok((5 << 15) + 40), "the asked id");
    assert_eq!(ids.next_id, -1);
}

fn q(sops: &[(u16, i16)]) -> Arc<Queue> {
    let sops: Vec<SemBuf> = sops.iter().map(|&(num, op)| SemBuf { num, op, flg: 0 }).collect();
    Queue::for_test(sops)
}

fn array(n: usize) -> Arc<SemArray> {
    SemArray::for_test(n)
}

#[test]
fn single_waiters_run_in_order_on_their_semaphore() {
    let a = array(2);
    let mut g = a.inner.lock();
    let (w1, w2) = (q(&[(0, -1)]), q(&[(0, -1)]));
    g.enqueue(w1.clone()).unwrap();
    g.enqueue(w2.clone()).unwrap();
    g.sems[0].val = 1;
    g.smart_update(None, false);
    assert_eq!(w1.status_for_test(), 0);
    assert!(w2.pending_for_test());
    assert_eq!(g.sems[0].val, 0);
}

#[test]
fn a_complex_waiter_keeps_arrival_order_on_the_merged_queue() {
    let a = array(2);
    let mut g = a.inner.lock();
    let (s0, cx, s1) = (q(&[(0, -1)]), q(&[(0, -1), (1, -1)]), q(&[(0, -1)]));
    g.enqueue(s0.clone()).unwrap();
    g.enqueue(cx.clone()).unwrap();
    g.enqueue(s1.clone()).unwrap();
    assert_eq!((g.complex, g.alter.len()), (1, 3), "all on the global list");
    g.sems[0].val = 1;
    g.smart_update(None, false);
    assert_eq!(s0.status_for_test(), 0, "first come");
    assert!(cx.pending_for_test() && s1.pending_for_test());
    g.sems[0].val = 1;
    g.sems[1].val = 1;
    g.smart_update(None, false);
    assert_eq!(cx.status_for_test(), 0, "the complex list before the later single one");
    assert!(s1.pending_for_test());
    assert_eq!(g.complex, 0);
    g.settle();
    assert_eq!((g.alter.len(), g.sems[0].alter.len()), (0, 1), "back on the semaphore's list");
}

#[test]
fn zero_waiters_go_with_the_value_and_lists_are_all_or_nothing() {
    let a = array(1);
    let mut g = a.inner.lock();
    let z = q(&[(0, 0)]);
    g.sems[0].val = 2;
    g.enqueue(z.clone()).unwrap();
    let mut blocking = 0;
    let list = [SemBuf { num: 0, op: -1, flg: 0 }, SemBuf { num: 0, op: -5, flg: 0 }];
    assert_eq!(g.perform(&list, None, 1, true, &mut blocking), 1, "the second step must wait");
    assert_eq!(blocking, 1);
    assert_eq!(g.sems[0].val, 2, "rolled back");
    g.sems[0].val = 0;
    g.smart_update(None, false);
    assert_eq!(z.status_for_test(), 0);
}

#[test]
fn removal_fails_every_waiter() {
    let a = array(2);
    let mut g = a.inner.lock();
    let (w, cx, z) = (q(&[(0, -1)]), q(&[(0, -1), (1, -1)]), q(&[(1, 0)]));
    g.sems[1].val = 1;
    g.enqueue(w.clone()).unwrap();
    g.enqueue(cx.clone()).unwrap();
    g.enqueue(z.clone()).unwrap();
    g.free_queues();
    let eidrm = -(uapi::ab::sysv::EIDRM as i32);
    assert_eq!((w.status_for_test(), cx.status_for_test(), z.status_for_test()), (eidrm, eidrm, eidrm));
    assert_eq!(g.complex, 0);
}
