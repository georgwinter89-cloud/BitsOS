//! FS-17: the lost wake-up of a watched epoll (bitscomp, 27.10.): an outer epoll looking at an inner one
//! (`readiness`) took a stale item off the inner ready list although the item's object had become ready and
//! woken it after the look. The object here makes that window happen every time: its first look answers "not
//! ready" and only then makes itself ready and wakes its queue, as a peer's write landing between the look and
//! the removal on another CPU.

use crate::epoll::{self, Epoll, EPOLLET, EPOLLONESHOT, EPOLL_CTL_ADD, EPOLL_CTL_MOD};
use crate::fd::OpenFile;
use crate::node::File;
use crate::poll::{Events, PollQueue};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const IN: u32 = Events::IN;

/// A socket-like object: `mask` is its readiness; with `race` set, the next look that sees nothing makes it
/// ready and wakes it right after reading the mask (the peer's write between the look and what follows).
struct Racy {
    mask: AtomicU32,
    race: AtomicBool,
    pq: PollQueue,
}

impl Racy {
    fn new() -> Arc<Racy> {
        Arc::new(Racy { mask: AtomicU32::new(0), race: AtomicBool::new(false), pq: PollQueue::new() })
    }
    fn set(&self, m: u32) {
        self.mask.store(m, Ordering::SeqCst);
        self.pq.wake_all();
    }
    /// A reader took everything (no wake, as a recvmsg).
    fn drain(&self) {
        self.mask.store(0, Ordering::SeqCst);
    }
}

impl File for Racy {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, crate::Errno> {
        Ok(0)
    }
    fn write_at(&self, _pos: u64, b: &[u8]) -> Result<usize, crate::Errno> {
        Ok(b.len())
    }
    fn mode(&self) -> u32 {
        crate::abi::S_IFSOCK | 0o777
    }
    fn events(&self) -> Events {
        let m = self.mask.load(Ordering::SeqCst);
        if m == 0 && self.race.swap(false, Ordering::SeqCst) {
            self.set(IN);
        }
        Events(m)
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        Some(&self.pq)
    }
    fn can_poll(&self) -> bool {
        true
    }
}

fn of(f: Arc<dyn File>) -> Arc<OpenFile> {
    OpenFile::anon(f, crate::abi::O_RDWR)
}

fn add(ep: &Arc<OpenFile>, f: &Arc<OpenFile>, fd: i32, events: u32, data: u64) {
    Epoll::ctl(ep, EPOLL_CTL_ADD, f, fd, (events, data), false).unwrap();
}

fn now_events(ep: &Arc<OpenFile>) -> Vec<(u32, u64)> {
    let mut out: Vec<(u32, u64)> = Vec::new();
    ep.file.as_epoll().unwrap().wait(&mut out, 16, Some(0), &|| false).unwrap();
    out
}

/// One inner epoll with the racy object, one outer epoll on the inner one (`outer_ev`, `inner_ev`): the item
/// goes stale (ready, woken, then read empty), the outer epoll looks at the inner one while the object becomes
/// ready again: afterwards both epolls must report it.
fn stale_item_woken_during_the_outer_look(inner_ev: u32, outer_ev: u32) {
    let s = Racy::new();
    let sf = of(s.clone());
    let inner = epoll::new_epoll();
    let outer = epoll::new_epoll();
    add(&inner, &sf, 3, inner_ev, 7);
    add(&outer, &inner, 4, outer_ev, 9);
    // Data came and was read before anyone harvested the inner epoll: its item stays queued, stale.
    s.set(IN);
    s.drain();
    // The outer harvest looks at the inner epoll; the peer writes between the look and the cleanup.
    s.race.store(true, Ordering::SeqCst);
    let first = now_events(&outer);
    assert!(!s.race.load(Ordering::SeqCst), "the outer harvest did not look at the object");
    // Whatever that harvest said (it may have seen nothing yet), the object is ready now and woke its queue.
    let again = now_events(&outer);
    assert!(
        first == vec![(IN, 9)] || again == vec![(IN, 9)],
        "outer epoll ({outer_ev:#x} on inner {inner_ev:#x}) lost the wake: {first:?} then {again:?}"
    );
    assert_eq!(now_events(&inner), vec![(IN, 7)], "inner epoll ({inner_ev:#x}) lost the item");
}

#[test]
fn a_stale_item_woken_during_an_outer_look_stays_queued() {
    for inner_ev in [IN, IN | EPOLLET, IN | EPOLLONESHOT] {
        for outer_ev in [IN, IN | EPOLLET, IN | EPOLLONESHOT] {
            stale_item_woken_during_the_outer_look(inner_ev, outer_ev);
        }
    }
}

/// The same window through poll(2) on the inner epoll (the same look, `ep_eventpoll_poll`).
#[test]
fn a_stale_item_woken_during_a_poll_of_the_epoll_stays_queued() {
    let s = Racy::new();
    let sf = of(s.clone());
    let inner = epoll::new_epoll();
    add(&inner, &sf, 3, IN, 7);
    s.set(IN);
    s.drain();
    s.race.store(true, Ordering::SeqCst);
    let _ = crate::poll::file_mask(&*inner.file);
    assert!(!s.race.load(Ordering::SeqCst));
    assert_eq!(crate::poll::file_mask(&*inner.file) & IN, IN, "the epoll fd does not show readable");
    assert_eq!(now_events(&inner), vec![(IN, 7)]);
}

/// A stale item that nobody woke during the look still leaves the list (Linux drops it in
/// `__ep_eventpoll_poll`): the epoll fd then shows not readable and an EPOLLONESHOT outer item re-armed by MOD
/// sees nothing.
#[test]
fn a_stale_item_nobody_woke_still_leaves_the_list() {
    let s = Racy::new();
    let sf = of(s.clone());
    let inner = epoll::new_epoll();
    let outer = epoll::new_epoll();
    add(&inner, &sf, 3, IN, 7);
    add(&outer, &inner, 4, IN | EPOLLONESHOT, 9);
    s.set(IN);
    s.drain();
    assert_eq!(crate::poll::file_mask(&*inner.file) & IN, 0);
    assert_eq!(now_events(&outer), vec![]);
    Epoll::ctl(&outer, EPOLL_CTL_MOD, &inner, 4, (IN | EPOLLONESHOT, 9), false).unwrap();
    assert_eq!(now_events(&outer), vec![]);
    s.set(IN);
    assert_eq!(now_events(&outer), vec![(IN, 9)]);
    assert_eq!(now_events(&inner), vec![(IN, 7)]);
}
