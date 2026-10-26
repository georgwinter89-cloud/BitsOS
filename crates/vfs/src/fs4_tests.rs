//! FS-4 host tests: the poll core (keyed wakes, subscriptions), epoll and poll/select through the syscalls with
//! the answers CachyOS gave for host/tests/fs/progs/fs-epoll.c (the same sequences, checked line by line), and
//! threads: waits woken from another thread, nested wakes, and a lost-wakeup stress on many pipes.

extern crate std;

use crate::cases_test::{program_view, Mem, SUBJECT, USER};
use crate::epoll::{self, Epoll, EPOLLET, EPOLLEXCLUSIVE, EPOLLONESHOT, EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLL_CTL_MOD};
use crate::devfs::Mem as Dm;
use crate::fd::{FdTable, OpenFile};
use crate::mount_ns::MountNs;
use crate::node::File;
use crate::pipe;
use crate::poll::{Events, PollQueue, PollWaker};
use crate::sys::{self, Sys};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use uapi::errno::*;

const IN: u32 = Events::IN;
const OUT: u32 = Events::OUT;

fn sys_for<'a>(mem: &'a Mem, fds: &'a FdTable, ns: &'a Arc<MountNs>) -> Sys<'a> {
    Sys { mem, fds, ns, cred: USER, subject: SUBJECT }
}

struct H<'a> {
    s: Sys<'a>,
    mem: &'a Mem,
}

impl H<'_> {
    fn call(&self, nr: usize, a: [u64; 6]) -> i64 {
        sys::call(&self.s, nr, a)
    }
    fn pipe(&self) -> (u64, u64) {
        let b = self.mem.alloc_zero(8);
        assert_eq!(self.call(293, [b, crate::abi::O_NONBLOCK, 0, 0, 0, 0]), 0);
        let v = self.mem.get(b, 8);
        (i32::from_le_bytes(v[0..4].try_into().unwrap()) as u64, i32::from_le_bytes(v[4..8].try_into().unwrap()) as u64)
    }
    fn epoll(&self) -> u64 {
        let r = self.call(291, [0; 6]);
        assert!(r >= 0);
        r as u64
    }
    fn ctl(&self, ep: u64, op: i32, fd: u64, events: u32, data: u64) -> i64 {
        let mut e = [0u8; 12];
        e[0..4].copy_from_slice(&events.to_le_bytes());
        e[4..12].copy_from_slice(&data.to_le_bytes());
        let at = self.mem.alloc(&e);
        self.call(233, [ep, op as u64, fd, at, 0, 0])
    }
    /// epoll_wait with a zero timeout: the (events, data) pairs.
    fn wait(&self, ep: u64, max: u64) -> Vec<(u32, u64)> {
        self.wait_t(ep, max, 0)
    }
    fn wait_t(&self, ep: u64, max: u64, ms: i32) -> Vec<(u32, u64)> {
        let at = self.mem.alloc_zero(12 * max as usize);
        let n = self.call(232, [ep, at, max, ms as u32 as u64, 0, 0]);
        assert!(n >= 0, "epoll_wait: {n}");
        let b = self.mem.get(at, 12 * n as usize);
        b.chunks_exact(12).map(|c| (u32::from_le_bytes(c[0..4].try_into().unwrap()), u64::from_le_bytes(c[4..12].try_into().unwrap()))).collect()
    }
    fn write(&self, fd: u64, data: &[u8]) {
        let at = self.mem.alloc(data);
        assert_eq!(self.call(1, [fd, at, data.len() as u64, 0, 0, 0]), data.len() as i64);
    }
    fn drain(&self, fd: u64) {
        let at = self.mem.alloc_zero(4096);
        while self.call(0, [fd, at, 4096, 0, 0, 0]) > 0 {}
    }
    fn revents(&self, fd: u64, events: u16) -> i64 {
        let mut e = [0u8; 8];
        e[0..4].copy_from_slice(&(fd as i32).to_le_bytes());
        e[4..6].copy_from_slice(&events.to_le_bytes());
        let at = self.mem.alloc(&e);
        let r = self.call(7, [at, 1, 0, 0, 0, 0]);
        if r < 0 { r } else { u16::from_le_bytes(self.mem.get(at + 6, 2).try_into().unwrap()) as i64 }
    }
    /// A memory device as /dev would open it (the host view has no devfs).
    fn dev(&self, m: Dm) -> u64 {
        let f = OpenFile::new(Arc::new(crate::devfs::MemDev(m)), crate::fd::OpenFlags { read: true, write: true, ..Default::default() });
        self.s.fds.install(f).unwrap() as u64
    }
    fn close(&self, fd: u64) {
        assert_eq!(self.call(3, [fd, 0, 0, 0, 0, 0]), 0);
    }
}

fn with<R>(f: impl FnOnce(&H<'_>) -> R) -> R {
    let (ns, fds) = program_view();
    let mem = Mem::new();
    let h = H { s: sys_for(&mem, &fds, &ns), mem: &mem };
    f(&h)
}

struct Count(AtomicUsize, AtomicU64);

impl PollWaker for Count {
    fn poll_wake(&self, key: u32, _depth: u32) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst);
        self.1.store(key as u64, Ordering::SeqCst);
        true
    }
}

#[test]
fn poll_queue_passes_keys_to_interested_subscribers_only() {
    let q = PollQueue::new();
    let c = Arc::new(Count(AtomicUsize::new(0), AtomicU64::new(0)));
    q.subscribe(1, IN | Events::ERR | Events::HUP, c.clone()).unwrap();
    assert_eq!(q.subscribers(), 1);
    assert_eq!(q.waiter_count(), 1, "a subscriber counts as a waiter (wakers may skip empty queues)");
    q.wake_poll(OUT);
    assert_eq!(c.0.load(Ordering::SeqCst), 0, "a key outside the interest passes the subscriber by");
    q.wake_poll(IN | Events::RDNORM);
    assert_eq!(c.0.load(Ordering::SeqCst), 1);
    assert_eq!(c.1.load(Ordering::SeqCst), (IN | Events::RDNORM) as u64);
    q.wake_all();
    assert_eq!(c.0.load(Ordering::SeqCst), 2, "a keyless wake reaches everyone");
    // Many subscribers: the list grows outside its lock.
    for id in 2..100 {
        q.subscribe(id, OUT, c.clone()).unwrap();
    }
    q.wake_poll(OUT);
    assert_eq!(c.0.load(Ordering::SeqCst), 2 + 98);
    for id in 1..100 {
        q.unsubscribe(id);
    }
    q.unsubscribe(7);
    q.wake_all();
    assert_eq!(c.0.load(Ordering::SeqCst), 100);
    assert_eq!(Arc::strong_count(&c), 1, "the queue let go of every subscription");
}

/// fs-epoll `basic`: argument rules and the epoll file (CachyOS transcript).
#[test]
fn epoll_basic_rules() {
    with(|h| {
        assert_eq!(h.call(213, [0, 0, 0, 0, 0, 0]), -EINVAL);
        assert_eq!(h.call(213, [(-1i64) as u64, 0, 0, 0, 0, 0]), -EINVAL);
        let a = h.call(213, [1, 0, 0, 0, 0, 0]) as u64;
        assert_eq!(h.call(291, [1, 0, 0, 0, 0, 0]), -EINVAL);
        assert_eq!(h.call(291, [crate::abi::O_NONBLOCK, 0, 0, 0, 0, 0]), -EINVAL);
        let b = h.call(291, [epoll::EPOLL_CLOEXEC, 0, 0, 0, 0, 0]) as u64;
        assert_eq!(h.call(72, [b, 1, 0, 0, 0, 0]), 1, "F_GETFD: close-on-exec");
        assert_eq!(h.call(72, [a, 1, 0, 0, 0, 0]), 0);
        assert_eq!(h.call(72, [b, 3, 0, 0, 0, 0]), 0o2, "F_GETFL: O_RDWR, no O_LARGEFILE");
        let buf = h.mem.alloc_zero(16);
        assert_eq!(h.call(0, [b, buf, 16, 0, 0, 0]), -EINVAL);
        assert_eq!(h.call(1, [b, buf, 8, 0, 0, 0]), -EINVAL);
        assert_eq!(h.call(8, [b, 5, 0, 0, 0, 0]), 0);
        let (r, w) = h.pipe();
        assert_eq!(h.ctl(99, EPOLL_CTL_ADD, r, IN, 1), -EBADF);
        assert_eq!(h.ctl(b, EPOLL_CTL_ADD, 99, IN, 1), -EBADF);
        assert_eq!(h.ctl(b, EPOLL_CTL_ADD, (-1i64) as u64, IN, 1), -EBADF);
        assert_eq!(h.ctl(w, EPOLL_CTL_ADD, r, IN, 1), -EINVAL);
        assert_eq!(h.ctl(b, EPOLL_CTL_ADD, b, IN, 1), -EINVAL);
        assert_eq!(h.call(233, [b, EPOLL_CTL_ADD as u64, r, 1 << 63, 0, 0]), -EFAULT);
        assert_eq!(h.call(233, [b, EPOLL_CTL_DEL as u64, r, 1 << 63, 0, 0]), -ENOENT);
        assert_eq!(h.ctl(b, 0, r, IN, 1), -EINVAL);
        assert_eq!(h.ctl(b, 4, r, IN, 1), -EINVAL);
        assert_eq!(h.ctl(b, EPOLL_CTL_MOD, r, IN, 1), -ENOENT);
        assert_eq!(h.ctl(b, EPOLL_CTL_ADD, r, IN, 1), 0);
        assert_eq!(h.ctl(b, EPOLL_CTL_ADD, r, IN, 1), -EEXIST);
        assert_eq!(h.ctl(b, 4, r, IN, 1), -EINVAL);
        assert_eq!(h.call(233, [b, EPOLL_CTL_DEL as u64, r, 0, 0, 0]), 0);
        assert_eq!(h.call(233, [b, EPOLL_CTL_DEL as u64, r, 0, 0, 0]), -ENOENT);
        let f = h.call(2, [h.mem.cstr(b"/tmp/ep-file"), crate::abi::O_CREAT | crate::abi::O_RDWR, 0o600, 0, 0, 0]) as u64;
        assert_eq!(h.ctl(b, EPOLL_CTL_ADD, f, IN, 1), -EPERM);
        assert_eq!(h.ctl(b, EPOLL_CTL_DEL, f, IN, 1), -EPERM);
        for m in [Dm::Null, Dm::Zero, Dm::Urandom] {
            let d = h.dev(m);
            assert_eq!(h.ctl(b, EPOLL_CTL_ADD, d, IN, 1), -EPERM, "{m:?}");
        }
        let rnd = h.dev(Dm::Random);
        assert_eq!(h.ctl(b, EPOLL_CTL_ADD, rnd, IN, 1), 0);
        assert_eq!(h.wait(b, 4), vec![(IN, 1)]);
        assert_eq!(h.ctl(b, EPOLL_CTL_DEL, rnd, 0, 0), 0);
        let opath = h.call(2, [h.mem.cstr(b"/tmp"), crate::abi::O_PATH, 0, 0, 0, 0]) as u64;
        assert_eq!(h.ctl(b, EPOLL_CTL_ADD, opath, IN, 1), -EBADF);
        assert_eq!(h.ctl(opath, EPOLL_CTL_ADD, r, IN, 1), -EBADF);
        let efd = h.call(290, [0, 0, 0, 0, 0, 0]) as u64;
        assert_eq!(h.ctl(b, EPOLL_CTL_ADD, efd, IN, 1), 0);
        let out = h.mem.alloc_zero(48);
        assert_eq!(h.call(232, [b, out, 0, 0, 0, 0]), -EINVAL);
        assert_eq!(h.call(232, [b, out, (-1i64) as u64, 0, 0, 0]), -EINVAL);
        assert_eq!(h.call(232, [b, out, 0x7fff_ffff, 0, 0, 0]), -EINVAL);
        assert_eq!(h.call(232, [b, out, 0x7fff_ffff / 12 + 1, 0, 0, 0]), -EINVAL);
        assert_eq!(h.call(232, [b, 0xffff_8880_0000_0000, 1, 0, 0, 0]), -EFAULT);
        assert_eq!(h.call(232, [99, out, 1, 0, 0, 0]), -EBADF);
        assert_eq!(h.call(232, [r, out, 1, 0, 0, 0]), -EINVAL);
        assert_eq!(h.call(232, [b, 0, 1, 0, 0, 0]), 0, "nothing ready: the array is never touched");
        h.write(efd, &1u64.to_le_bytes());
        assert_eq!(h.call(232, [b, 0, 1, 0, 0, 0]), -EFAULT);
        assert_eq!(h.call(232, [b, 0x1000, 1, 0, 0, 0]), -EFAULT);
        assert_eq!(h.wait(b, 4), vec![(IN, 1)], "an event that could not be copied stays ready");
        let mask = h.mem.alloc(&1u64.to_le_bytes());
        assert_eq!(h.call(281, [b, out, 1, 0, mask, 4]), -EINVAL);
        assert_eq!(h.call(281, [b, out, 1, 0, 1 << 63, 8]), -EFAULT);
        assert_eq!(h.call(281, [b, out, 1, 0, 0, 3]), 1);
    });
}

/// fs-epoll `modes` (CachyOS transcript).
#[test]
fn epoll_level_edge_oneshot_order_masks() {
    with(|h| {
        let ep = h.epoll();
        let (a0, a1) = h.pipe();
        let (b0, b1) = h.pipe();
        let (c0, c1) = h.pipe();
        assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, a0, IN, 10), 0);
        assert!(h.wait(ep, 8).is_empty());
        h.write(a1, b"x");
        assert_eq!(h.wait(ep, 8), vec![(IN, 10)]);
        assert_eq!(h.wait(ep, 8), vec![(IN, 10)]);
        h.drain(a0);
        assert!(h.wait(ep, 8).is_empty());
        assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, b0, IN | EPOLLET, 20), 0);
        h.write(b1, b"y");
        assert_eq!(h.wait(ep, 8), vec![(IN, 20)]);
        assert!(h.wait(ep, 8).is_empty());
        h.write(b1, b"y");
        assert_eq!(h.wait(ep, 8), vec![(IN, 20)], "every write is an edge for a pipe's readers");
        h.drain(b0);
        assert!(h.wait(ep, 8).is_empty());
        h.write(c1, b"z");
        assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, c0, IN | EPOLLET, 30), 0);
        assert_eq!(h.wait(ep, 8), vec![(IN, 30)], "registered after the event: still reported");
        assert!(h.wait(ep, 8).is_empty());
        assert_eq!(h.ctl(ep, EPOLL_CTL_MOD, c0, IN | EPOLLET, 31), 0);
        assert_eq!(h.wait(ep, 8), vec![(IN, 31)]);
        h.drain(c0);
        assert_eq!(h.ctl(ep, EPOLL_CTL_MOD, a0, IN | EPOLLONESHOT, 11), 0);
        h.write(a1, b"x");
        assert_eq!(h.wait(ep, 8), vec![(IN, 11)]);
        h.write(a1, b"x");
        assert!(h.wait(ep, 8).is_empty(), "oneshot: disabled after its event");
        assert_eq!(h.ctl(ep, EPOLL_CTL_MOD, a0, IN | EPOLLONESHOT, 12), 0);
        assert_eq!(h.wait(ep, 8), vec![(IN, 12)]);
        assert_eq!(h.ctl(ep, EPOLL_CTL_MOD, a0, IN, 13), 0);
        h.write(b1, b"y");
        h.write(c1, b"z");
        assert_eq!(h.ctl(ep, EPOLL_CTL_MOD, b0, IN, 21), 0);
        assert_eq!(h.ctl(ep, EPOLL_CTL_MOD, c0, IN, 32), 0);
        assert_eq!(h.wait(ep, 8), vec![(IN, 13), (IN, 21), (IN, 32)]);
        assert_eq!(h.wait(ep, 1), vec![(IN, 13)]);
        assert_eq!(h.wait(ep, 1), vec![(IN, 21)]);
        assert_eq!(h.wait(ep, 1), vec![(IN, 32)]);
        assert_eq!(h.wait(ep, 2), vec![(IN, 13), (IN, 21)]);
        assert_eq!(h.wait(ep, 2), vec![(IN, 32), (IN, 13)]);
        h.drain(a0);
        h.drain(b0);
        h.drain(c0);
        assert!(h.wait(ep, 8).is_empty());
        assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, a1, IN | OUT, 14), 0);
        assert_eq!(h.wait(ep, 8), vec![(OUT, 14)]);
        assert_eq!(h.ctl(ep, EPOLL_CTL_MOD, a1, IN | OUT | Events::RDNORM | Events::WRNORM, 15), 0);
        assert_eq!(h.wait(ep, 8), vec![(0x104, 15)]);
        assert_eq!(h.ctl(ep, EPOLL_CTL_MOD, a1, 0, 16), 0);
        assert!(h.wait(ep, 8).is_empty());
        assert_eq!(h.ctl(ep, EPOLL_CTL_MOD, a0, Events::RDNORM, 17), 0);
        h.write(a1, b"x");
        assert_eq!(h.wait(ep, 8), vec![(0x40, 17)]);
        h.drain(a0);
        assert_eq!(h.ctl(ep, EPOLL_CTL_MOD, a0, 0, 18), 0);
        h.close(a1);
        assert_eq!(h.wait(ep, 8), vec![(Events::HUP, 18)], "HUP comes without being asked");
        h.close(b0);
        assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, b1, OUT, 22), 0);
        assert_eq!(h.wait(ep, 8), vec![(Events::HUP, 18), (OUT | Events::ERR, 22)]);
        let e2 = h.epoll();
        let efd = h.call(290, [3, 0, 0, 0, 0, 0]) as u64;
        assert_eq!(h.ctl(e2, EPOLL_CTL_ADD, efd, IN | OUT, u64::MAX), 0);
        assert_eq!(h.wait(e2, 8), vec![(IN | OUT, u64::MAX)], "eventfd: no RDNORM/WRNORM");
        let buf = h.mem.alloc_zero(8);
        assert_eq!(h.call(0, [efd, buf, 8, 0, 0, 0]), 8);
        assert_eq!(h.wait(e2, 8), vec![(OUT, u64::MAX)]);
        h.write(efd, &0xffff_ffff_ffff_fffeu64.to_le_bytes());
        assert_eq!(h.wait(e2, 8), vec![(IN, u64::MAX)]);
        assert_eq!(h.call(0, [efd, buf, 8, 0, 0, 0]), 8);
        assert_eq!(h.ctl(e2, EPOLL_CTL_MOD, efd, OUT | EPOLLET, 5), 0);
        assert_eq!(h.wait(e2, 8), vec![(OUT, 5)]);
        h.write(efd, &1u64.to_le_bytes());
        assert!(h.wait(e2, 8).is_empty(), "an eventfd write wakes with EPOLLIN only");
        assert_eq!(h.call(0, [efd, buf, 8, 0, 0, 0]), 8);
        assert_eq!(h.wait(e2, 8), vec![(OUT, 5)]);
    });
}

/// fs-epoll `close`: registrations belong to the open file description (CachyOS transcript).
#[test]
fn epoll_registration_follows_the_open_file() {
    with(|h| {
        let ep = h.epoll();
        let (p0, p1) = h.pipe();
        let d = h.call(32, [p0, 0, 0, 0, 0, 0]) as u64;
        assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, p0, IN, 1), 0);
        h.write(p1, b"x");
        assert_eq!(h.wait(ep, 8), vec![(IN, 1)]);
        h.close(p0);
        assert_eq!(h.wait(ep, 8), vec![(IN, 1)], "a dup keeps the file and its registration");
        assert_eq!(h.ctl(ep, EPOLL_CTL_DEL, p0, 0, 0), -EBADF);
        assert_eq!(h.ctl(ep, EPOLL_CTL_DEL, d, 0, 0), -ENOENT);
        assert_eq!(h.ctl(ep, EPOLL_CTL_MOD, d, IN, 2), -ENOENT);
        assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, d, IN, 3), 0);
        assert_eq!(h.wait(ep, 8), vec![(IN, 1), (IN, 3)]);
        let (q0, q1) = h.pipe();
        assert_eq!(q0, p0, "the new pipe reuses the number");
        assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, q0, IN, 4), 0);
        h.write(q1, b"q");
        assert_eq!(h.wait(ep, 8), vec![(IN, 1), (IN, 3), (IN, 4)]);
        h.close(d);
        assert_eq!(h.wait(ep, 8), vec![(IN, 4)], "the last close of the file removed both registrations");
        assert_eq!(h.ctl(ep, EPOLL_CTL_DEL, q0, 0, 0), 0);
        assert_eq!(h.ctl(ep, EPOLL_CTL_DEL, q0, 0, 0), -ENOENT);
        let (r0, r1) = h.pipe();
        assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, r0, IN, 5), 0);
        h.write(r1, b"r");
        assert_eq!(h.call(33, [q1, r0, 0, 0, 0, 0]), r0 as i64);
        assert!(h.wait(ep, 8).is_empty(), "dup2 over the number closed the file");
        let ep2 = h.call(32, [ep, 0, 0, 0, 0, 0]) as u64;
        h.close(ep);
        assert_eq!(h.ctl(ep2, EPOLL_CTL_ADD, q0, IN, 6), 0);
        assert_eq!(h.wait(ep2, 8), vec![(IN, 6)]);
    });
}

/// fs-epoll `nested` (CachyOS transcript): readiness through an inner epoll, edges, loops, depth both ways.
#[test]
fn nested_epolls_loops_and_depth() {
    with(|h| {
        let inner = h.epoll();
        let outer = h.epoll();
        let (p0, p1) = h.pipe();
        assert_eq!(h.ctl(inner, EPOLL_CTL_ADD, p0, IN, 1), 0);
        assert_eq!(h.ctl(outer, EPOLL_CTL_ADD, inner, IN | OUT, 2), 0);
        assert!(h.wait(outer, 8).is_empty());
        assert_eq!(h.revents(inner, 0x45), 0);
        h.write(p1, b"x");
        assert_eq!(h.revents(inner, 0x45), 0x41);
        assert_eq!(h.wait(outer, 8), vec![(IN, 2)]);
        assert_eq!(h.wait(outer, 8), vec![(IN, 2)]);
        assert_eq!(h.wait(inner, 8), vec![(IN, 1)]);
        h.drain(p0);
        assert!(h.wait(outer, 8).is_empty());
        assert_eq!(h.revents(inner, 1), 0);
        assert_eq!(h.ctl(outer, EPOLL_CTL_MOD, inner, IN | EPOLLET, 3), 0);
        h.write(p1, b"x");
        assert_eq!(h.wait(outer, 8), vec![(IN, 3)]);
        assert!(h.wait(outer, 8).is_empty());
        h.write(p1, b"x");
        assert_eq!(h.wait(outer, 8), vec![(IN, 3)], "a second wake of the inner epoll is a second edge");
        h.drain(p0);
        assert_eq!(h.ctl(inner, EPOLL_CTL_ADD, outer, IN, 4), -ELOOP);
        let third = h.epoll();
        assert_eq!(h.ctl(third, EPOLL_CTL_ADD, outer, IN, 5), 0);
        assert_eq!(h.ctl(inner, EPOLL_CTL_ADD, third, IN, 6), -ELOOP);
        assert_eq!(h.ctl(third, EPOLL_CTL_ADD, inner, IN, 7), 0);
        // Chains: the fifth level is refused (with or without a file at the bottom).
        for bottom in [false, true] {
            let mut e = vec![h.epoll()];
            if bottom {
                assert_eq!(h.ctl(e[0], EPOLL_CTL_ADD, p0, IN, 0), 0);
            }
            for i in 1..8u64 {
                e.push(h.epoll());
                let r = h.ctl(e[i as usize], EPOLL_CTL_ADD, e[i as usize - 1], IN, i);
                assert_eq!(r, if i < 5 { 0 } else { -ELOOP }, "{} into {} (bottom file {bottom})", i - 1, i);
                if r < 0 {
                    break;
                }
            }
        }
        let [a, b, c, d, e, f, g, hh] = [(); 8].map(|_| h.epoll());
        h.ctl(a, EPOLL_CTL_ADD, b, IN, 0);
        h.ctl(b, EPOLL_CTL_ADD, c, IN, 0);
        h.ctl(d, EPOLL_CTL_ADD, e, IN, 0);
        h.ctl(f, EPOLL_CTL_ADD, g, IN, 0);
        h.ctl(g, EPOLL_CTL_ADD, hh, IN, 0);
        assert_eq!(h.ctl(c, EPOLL_CTL_ADD, d, IN, 0), 0);
        assert_eq!(h.ctl(c, EPOLL_CTL_ADD, f, IN, 0), -ELOOP);
        assert_eq!(h.ctl(b, EPOLL_CTL_ADD, f, IN, 0), 0);
        assert_eq!(h.ctl(hh, EPOLL_CTL_ADD, e, IN, 0), -ELOOP);
        assert_eq!(h.ctl(e, EPOLL_CTL_ADD, hh, IN, 0), -ELOOP);
        // A file added below a chain: the path check (EINVAL above 4 epolls).
        let ch: Vec<u64> = (0..5).map(|_| h.epoll()).collect();
        for i in 1..5 {
            assert_eq!(h.ctl(ch[i], EPOLL_CTL_ADD, ch[i - 1], IN, 0), 0);
        }
        let (x0, x1) = h.pipe();
        assert_eq!(h.ctl(ch[0], EPOLL_CTL_ADD, x0, IN, 0), 0);
        assert_eq!(h.ctl(ch[1], EPOLL_CTL_ADD, x0, IN, 0), 0);
        h.write(x1, b"w");
        assert_eq!(h.wait(ch[4], 8), vec![(IN, 0)], "the wake climbs four epolls");
    });
}

/// WY-1a CR1 (wayland-backend's epoll inside calloop's): after the inner epoll was harvested by its own
/// epoll_wait and its file drained, the next write must wake the outer epoll again, every time.
#[test]
fn inner_epoll_harvested_by_its_owner_still_wakes_the_outer() {
    with(|h| {
        let b = h.epoll();
        let a = h.epoll();
        assert_eq!(h.ctl(a, EPOLL_CTL_ADD, b, IN, 9), 0);
        let (s0, s1) = h.pipe();
        assert_eq!(h.ctl(b, EPOLL_CTL_ADD, s0, IN, 1), 0);
        for round in 0..4 {
            h.write(s1, b"x");
            assert_eq!(h.wait(a, 1), vec![(IN, 9)], "round {round}: the outer epoll");
            assert_eq!(h.wait(b, 8), vec![(IN, 1)], "round {round}: the inner epoll");
            h.drain(s0);
            assert!(h.wait(b, 8).is_empty(), "round {round}: inner after the drain");
        }
        // Without a harvest of the outer one in between.
        for round in 0..4 {
            h.write(s1, b"x");
            assert_eq!(h.wait(b, 8), vec![(IN, 1)], "round {round}: inner first");
            h.drain(s0);
            h.write(s1, b"y");
            assert_eq!(h.wait(a, 1), vec![(IN, 9)], "round {round}: outer after inner's own harvest");
            h.drain(s0);
        }
        // calloop's way (the `polling` crate): the outer item one-shot, re-armed with MOD after each event; and
        // an item added to the inner epoll while its file is ready already.
        for mode in [IN | EPOLLONESHOT, IN | EPOLLET] {
            assert_eq!(h.ctl(a, EPOLL_CTL_MOD, b, mode, 9), 0);
            assert!(h.wait(a, 1).is_empty() || mode & EPOLLET != 0);
            for round in 0..4u64 {
                let (t0, t1) = h.pipe();
                h.write(t1, b"z");
                assert_eq!(h.ctl(b, EPOLL_CTL_ADD, t0, IN, 100 + round), 0);
                assert_eq!(h.wait(a, 1), vec![(IN, 9)], "mode {mode:#x} round {round}: an added ready file");
                assert_eq!(h.ctl(a, EPOLL_CTL_MOD, b, mode, 9), 0);
                assert_eq!(h.wait(b, 8), vec![(IN, 100 + round)]);
                h.drain(t0);
                h.write(s1, b"x");
                assert_eq!(h.wait(a, 1), vec![(IN, 9)], "mode {mode:#x} round {round}: the next write");
                assert_eq!(h.ctl(a, EPOLL_CTL_MOD, b, mode, 9), 0);
                assert_eq!(h.wait(b, 8), vec![(IN, 1)]);
                h.drain(s0);
                h.write(t1, b"w");
                assert_eq!(h.wait(a, 1), vec![(IN, 9)], "mode {mode:#x} round {round}: the added file again");
                assert_eq!(h.ctl(a, EPOLL_CTL_MOD, b, mode, 9), 0);
                assert_eq!(h.wait(b, 8), vec![(IN, 100 + round)]);
                h.drain(t0);
                assert_eq!(h.ctl(b, EPOLL_CTL_DEL, t0, 0, 0), 0);
            }
        }
    });
}

#[test]
fn exclusive_wakeup_and_odd_bits() {
    with(|h| {
        let ep = h.epoll();
        let other = h.epoll();
        let (p0, p1) = h.pipe();
        assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, p0, EPOLLEXCLUSIVE | IN, 1), 0);
        assert_eq!(h.ctl(ep, EPOLL_CTL_MOD, p0, IN, 2), -EINVAL);
        assert_eq!(h.ctl(ep, EPOLL_CTL_MOD, p0, IN | EPOLLEXCLUSIVE, 2), -EINVAL);
        h.write(p1, b"x");
        assert_eq!(h.wait(ep, 8), vec![(IN, 1)]);
        assert_eq!(h.ctl(ep, EPOLL_CTL_DEL, p0, 0, 0), 0);
        assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, p0, EPOLLEXCLUSIVE | IN | EPOLLONESHOT, 3), -EINVAL);
        assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, p0, EPOLLEXCLUSIVE | IN | Events::RDNORM, 3), -EINVAL);
        assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, p0, EPOLLEXCLUSIVE | IN | EPOLLET | Events::ERR | Events::HUP | OUT, 3), 0);
        assert_eq!(h.ctl(ep, EPOLL_CTL_DEL, p0, 0, 0), 0);
        assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, other, EPOLLEXCLUSIVE | IN, 4), -EINVAL);
        assert_eq!(h.ctl(ep, EPOLL_CTL_MOD, p1, EPOLLEXCLUSIVE | IN, 4), -EINVAL);
        assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, p0, epoll::EPOLLWAKEUP | IN, 5), 0);
        assert_eq!(h.wait(ep, 8), vec![(IN, 5)]);
        assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, p1, Events::RDHUP | Events::PRI | Events::MSG | Events::RDBAND | Events::WRBAND | OUT, 6), 0);
        assert_eq!(h.wait(ep, 8), vec![(IN, 5), (OUT, 6)]);
        assert_eq!(h.ctl(other, EPOLL_CTL_ADD, p0, !EPOLLEXCLUSIVE, 7), 0);
        assert_eq!(h.wait(other, 8), vec![(0x41, 7)]);
        h.close(p1);
        assert!(h.wait(other, 8).is_empty(), "oneshot (every bit) stays disabled");
    });
}

#[test]
fn poll_masks_like_linux() {
    with(|h| {
        let efd = h.call(290, [0, 0, 0, 0, 0, 0]) as u64;
        assert_eq!(h.revents(efd, 0x7fff), 0x4);
        h.write(efd, &1u64.to_le_bytes());
        assert_eq!(h.revents(efd, 0x7fff), 0x5);
        let ep = h.epoll();
        assert_eq!(h.revents(ep, 0x7fff), 0);
        assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, efd, IN, 1), 0);
        assert_eq!(h.revents(ep, 0x7fff), 0x41);
        h.ctl(ep, EPOLL_CTL_MOD, efd, OUT | EPOLLET, 1);
        assert_eq!(h.wait(ep, 1).len(), 1);
        assert_eq!(h.revents(ep, 0x7fff), 0);
        let rnd = h.dev(Dm::Random);
        assert_eq!(h.revents(rnd, 0x7fff), 0x41);
        let null = h.dev(Dm::Null);
        assert_eq!(h.revents(null, 0x7fff), 0x145);
        let (p0, p1) = h.pipe();
        assert_eq!(h.revents(p0, OUT as u16), 0);
        assert_eq!(h.revents(p1, IN as u16), 0);
        h.close(p0);
        assert_eq!(h.revents(p1, OUT as u16), 0xc);
    });
}

#[test]
fn select_rules() {
    with(|h| {
        let (p0, p1) = h.pipe();
        let set = |bits: &[u64]| {
            let mut v = vec![0u8; 128];
            for b in bits {
                v[(*b / 8) as usize] |= 1 << (b % 8);
            }
            h.mem.alloc(&v)
        };
        let isset = |at: u64, b: u64| h.mem.get(at + b / 8, 1)[0] & (1 << (b % 8)) != 0;
        let (r, w, x) = (set(&[p0]), set(&[p1]), set(&[p0]));
        let tv = h.mem.alloc(&[0u8; 16]);
        assert_eq!(h.call(23, [p1 + 1, r, w, x, tv, 0]), 1);
        assert!(!isset(r, p0) && isset(w, p1) && !isset(x, p0));
        h.write(p1, b"s");
        let (r, w) = (set(&[p0, p1]), set(&[p1]));
        assert_eq!(h.call(23, [p1 + 1, r, w, 0, 0, 0]), 2);
        assert!(isset(r, p0) && !isset(r, p1) && isset(w, p1));
        h.drain(p0);
        let r = set(&[p0]);
        let mut t = [0u8; 16];
        t[8..16].copy_from_slice(&20_000i64.to_le_bytes());
        let tv = h.mem.alloc(&t);
        let t0 = Instant::now();
        assert_eq!(h.call(23, [p0 + 1, r, 0, 0, tv, 0]), 0);
        assert!(t0.elapsed() >= Duration::from_millis(20));
        assert!(!isset(r, p0));
        assert_eq!(h.mem.get(tv, 16), vec![0u8; 16], "time left 0 0");
        assert_eq!(h.call(23, [61, set(&[60]), 0, 0, 0, 0]), -EBADF);
        // The table holds fewer than 64 numbers: bits above 64 are neither read nor written.
        h.write(p1, b"s");
        let r = set(&[p0, 100]);
        assert_eq!(h.call(23, [1024, r, 0, 0, 0, 0]), 1);
        assert!(isset(r, 100) && isset(r, p0));
        h.drain(p0);
        assert_eq!(h.call(23, [(-1i64) as u64, 0, 0, 0, 0, 0]), -EINVAL);
        let mut t = [0u8; 16];
        t[8..16].copy_from_slice(&(-1i64).to_le_bytes());
        assert_eq!(h.call(23, [1, 0, 0, 0, h.mem.alloc(&t), 0]), -EINVAL);
        let mut t = [0u8; 16];
        t[0..8].copy_from_slice(&(-1i64).to_le_bytes());
        assert_eq!(h.call(23, [1, 0, 0, 0, h.mem.alloc(&t), 0]), -EINVAL);
        assert_eq!(h.call(23, [1, 1 << 63, 0, 0, 0, 0]), -EFAULT);
        assert_eq!(h.call(23, [1, 0, 0, 0, 1 << 63, 0]), -EFAULT);
        assert_eq!(h.call(23, [p1 + 1, 0, 0, set(&[p1]), h.mem.alloc(&[0u8; 16]), 0]), 0);
        // pselect6's { sigset *, size } pack.
        let mask = h.mem.alloc(&(1u64 << 9).to_le_bytes());
        let pack = |p: u64, n: u64| {
            let mut v = [0u8; 16];
            v[0..8].copy_from_slice(&p.to_le_bytes());
            v[8..16].copy_from_slice(&n.to_le_bytes());
            h.mem.alloc(&v)
        };
        let mut ts = [0u8; 16];
        ts[8..16].copy_from_slice(&10_000_000i64.to_le_bytes());
        let tsp = h.mem.alloc(&ts);
        assert_eq!(h.call(270, [p1 + 1, 0, set(&[p1]), 0, tsp, pack(mask, 8)]), 1);
        assert_eq!(h.call(270, [p1 + 1, 0, set(&[p1]), 0, tsp, pack(mask, 4)]), -EINVAL);
        assert_eq!(h.call(270, [p1 + 1, 0, set(&[p1]), 0, tsp, pack(1 << 63, 8)]), -EFAULT);
        assert_eq!(h.call(270, [p1 + 1, 0, set(&[p1]), 0, tsp, 1 << 63]), -EFAULT);
        let mut bad = [0u8; 16];
        bad[8..16].copy_from_slice(&1_000_000_000i64.to_le_bytes());
        assert_eq!(h.call(270, [p1 + 1, 0, set(&[p1]), 0, h.mem.alloc(&bad), 0]), -EINVAL);
    });
}

#[test]
fn select_caps_n_at_the_table_size_linux_has() {
    let t = FdTable::new();
    assert_eq!(t.max_fds(), 64);
    let ns = MountNs::new_empty();
    ns.create_file("/f", 0o644, vec![1]).unwrap();
    let of = OpenFile::new(ns.open("/f").unwrap(), crate::fd::OpenFlags { read: true, ..Default::default() });
    t.install_at(64, of.clone()).unwrap();
    assert_eq!(t.max_fds(), 128);
    t.install_at(300, of).unwrap();
    assert_eq!(t.max_fds(), 512);
    t.close(300).unwrap();
    assert_eq!(t.max_fds(), 512, "tables never shrink");
}

// ---- threads ----

fn kernel_epoll() -> Arc<OpenFile> {
    epoll::new_epoll()
}

fn add(ep: &Arc<OpenFile>, f: &Arc<OpenFile>, fd: i32, events: u32, data: u64) {
    Epoll::ctl(ep, EPOLL_CTL_ADD, f, fd, (events, data), false).unwrap();
}

fn ep_of(of: &Arc<OpenFile>) -> &Epoll {
    of.file.as_epoll().unwrap()
}

fn anon(f: Arc<pipe::PipeEnd>, status: u64) -> Arc<OpenFile> {
    OpenFile::anon(f, status)
}

#[test]
fn a_blocked_epoll_wait_wakes_on_a_write_from_another_thread() {
    let (r, w) = pipe::kernel_pipe();
    let rf = anon(r, crate::abi::O_RDONLY);
    let ep = kernel_epoll();
    add(&ep, &rf, 3, IN, 42);
    let ep2 = ep.clone();
    // INT-A2a: the clock starts before the spawn (a thread that started late measured less than the 30 ms the
    // writer waits, and the test failed on a loaded host with 23.9 ms).
    let t0 = Instant::now();
    let t = thread::spawn(move || {
        let mut out: Vec<(u32, u64)> = Vec::new();
        let n = ep_of(&ep2).wait(&mut out, 4, None, &|| false).unwrap();
        (n, out, t0.elapsed())
    });
    thread::sleep(Duration::from_millis(30));
    w.write_stream(b"go", false).unwrap();
    let (n, out, took) = t.join().unwrap();
    assert_eq!((n, out), (1, vec![(IN, 42)]));
    assert!(took >= Duration::from_millis(30) && took < Duration::from_secs(5), "{took:?}");
}

#[test]
fn a_blocked_outer_epoll_wakes_through_the_inner_one() {
    let (r, w) = pipe::kernel_pipe();
    let rf = anon(r, crate::abi::O_RDONLY);
    let inner = kernel_epoll();
    let outer = kernel_epoll();
    add(&inner, &rf, 3, IN, 1);
    add(&outer, &inner, 4, IN | EPOLLET, 2);
    let o2 = outer.clone();
    let t = thread::spawn(move || {
        let mut out: Vec<(u32, u64)> = Vec::new();
        ep_of(&o2).wait(&mut out, 4, Some(crate::poll::now() + 5_000_000_000), &|| false).map(|_| out)
    });
    thread::sleep(Duration::from_millis(30));
    w.write_stream(b"x", false).unwrap();
    assert_eq!(t.join().unwrap().unwrap(), vec![(IN, 2)]);
}

#[test]
fn poll_table_sleeps_until_a_queue_wakes() {
    let (r, w) = pipe::kernel_pipe();
    let rf: Arc<dyn File> = r;
    let t = thread::spawn(move || {
        let mut table = crate::poll::PollTable::new(true);
        let deadline = crate::poll::now() + 5_000_000_000;
        loop {
            table.arm();
            let m = table.poll(&rf, IN | Events::ERR | Events::HUP);
            if m & IN != 0 {
                return m;
            }
            table.stop_registering();
            assert!(!table.sleep(Some(deadline), &|| false), "no wake within 5 s");
        }
    });
    thread::sleep(Duration::from_millis(20));
    w.write_stream(b"x", false).unwrap();
    assert_eq!(t.join().unwrap() & IN, IN);
}

/// Many pipes, many writers, several waiters on one epoll (EPOLLET, nonblocking reads to EAGAIN): every byte
/// written is read, and no waiter ever sleeps through data (each wait has a 5 s limit; a lost wake-up would
/// leave bytes behind and the test would count them missing).
#[test]
fn no_lost_wakeups_with_many_pipes_writers_and_waiters() {
    const PIPES: usize = 16;
    const PER_WRITER: usize = 2000;
    let ep = kernel_epoll();
    let mut writers = Vec::new();
    let mut readers = Vec::new();
    for i in 0..PIPES {
        let (r, w) = pipe::kernel_pipe();
        let rf = anon(r.clone(), crate::abi::O_RDONLY | crate::abi::O_NONBLOCK);
        add(&ep, &rf, i as i32 + 10, IN | EPOLLET, i as u64);
        readers.push((r, rf));
        writers.push(w);
    }
    let readers = Arc::new(readers);
    let got = Arc::new(AtomicU64::new(0));
    let done = Arc::new(core::sync::atomic::AtomicBool::new(false));
    let total = (PIPES * PER_WRITER) as u64;
    let mut waiters = Vec::new();
    for _ in 0..4 {
        let (ep, readers, got, done) = (ep.clone(), readers.clone(), got.clone(), done.clone());
        waiters.push(thread::spawn(move || {
            let mut b = [0u8; 512];
            while !done.load(Ordering::SeqCst) {
                let mut out: Vec<(u32, u64)> = Vec::new();
                let n = ep_of(&ep).wait(&mut out, 8, Some(crate::poll::now() + 200_000_000), &|| false).unwrap();
                for (_, data) in out.iter().take(n) {
                    let r = &readers[*data as usize].0;
                    while let Ok(k) = r.read_stream(&mut b, true) {
                        if k == 0 {
                            break;
                        }
                        got.fetch_add(k as u64, Ordering::SeqCst);
                    }
                }
                if got.load(Ordering::SeqCst) >= total {
                    done.store(true, Ordering::SeqCst);
                }
            }
        }));
    }
    let mut ws = Vec::new();
    for w in writers {
        ws.push(thread::spawn(move || {
            for i in 0..PER_WRITER {
                w.write_stream(&[i as u8], false).unwrap();
                if i % 97 == 0 {
                    thread::yield_now();
                }
            }
        }));
    }
    for w in ws {
        w.join().unwrap();
    }
    let t0 = Instant::now();
    while got.load(Ordering::SeqCst) < total && t0.elapsed() < Duration::from_secs(10) {
        thread::sleep(Duration::from_millis(5));
    }
    done.store(true, Ordering::SeqCst);
    for w in waiters {
        w.join().unwrap();
    }
    assert_eq!(got.load(Ordering::SeqCst), total, "bytes left behind: a lost wake-up");
    assert_eq!(ep_of(&ep).len(), PIPES);
}

/// Edges right at the moment a waiter goes to sleep: ping-pong through two pipes with blocking epoll waits
/// (no timeout at all would hang the test on a lost wake-up; 5 s is the guard).
#[test]
fn ping_pong_through_epoll_never_misses_a_wake() {
    let (ar, aw) = pipe::kernel_pipe();
    let (br, bw) = pipe::kernel_pipe();
    let ea = kernel_epoll();
    let eb = kernel_epoll();
    // The open files must stay: their last close would take the registrations (the close semantics).
    let (af, bf) = (anon(ar.clone(), crate::abi::O_RDONLY), anon(br.clone(), crate::abi::O_RDONLY));
    add(&ea, &af, 3, IN | EPOLLET, 1);
    add(&eb, &bf, 3, IN, 2);
    const ROUNDS: usize = 3000;
    let t = thread::spawn(move || {
        let mut b = [0u8; 8];
        for _ in 0..ROUNDS {
            let mut out: Vec<(u32, u64)> = Vec::new();
            let n = ep_of(&eb).wait(&mut out, 1, Some(crate::poll::now() + 5_000_000_000), &|| false).unwrap();
            assert_eq!(n, 1, "lost wake-up on b");
            br.read_stream(&mut b, true).unwrap();
            aw.write_stream(b"a", false).unwrap();
        }
    });
    let mut b = [0u8; 8];
    for _ in 0..ROUNDS {
        bw.write_stream(b"b", false).unwrap();
        let mut out: Vec<(u32, u64)> = Vec::new();
        let n = ep_of(&ea).wait(&mut out, 1, Some(crate::poll::now() + 5_000_000_000), &|| false).unwrap();
        assert_eq!(n, 1, "lost wake-up on a");
        ar.read_stream(&mut b, true).unwrap();
    }
    t.join().unwrap();
    drop((af, bf));
}

/// DEL, close and the epoll's own end while another thread keeps waking the file: nothing stays subscribed.
#[test]
fn registrations_leave_the_queues_on_del_close_and_drop() {
    let (r, w) = pipe::kernel_pipe();
    let subs = || r.poll_head().unwrap().subscribers();
    let rf = anon(r.clone(), crate::abi::O_RDONLY);
    let ep = kernel_epoll();
    add(&ep, &rf, 3, IN, 1);
    assert_eq!(subs(), 1);
    Epoll::ctl(&ep, EPOLL_CTL_DEL, &rf, 3, (0, 0), false).unwrap();
    assert_eq!(subs(), 0);
    add(&ep, &rf, 3, IN, 1);
    add(&ep, &rf, 4, IN, 2);
    assert_eq!(subs(), 2);
    let stop = Arc::new(core::sync::atomic::AtomicBool::new(false));
    let s2 = stop.clone();
    let (r2, w2) = (r.clone(), w.clone());
    let wt = thread::spawn(move || {
        let mut b = [0u8; 64];
        while !s2.load(Ordering::SeqCst) {
            let _ = w2.write_stream(b"x", true);
            let _ = r2.read_stream(&mut b, true);
        }
    });
    drop(rf);
    assert_eq!(subs(), 0, "the last reference of the open file took both registrations");
    assert!(ep_of(&ep).is_empty());
    let rf = anon(r.clone(), crate::abi::O_RDONLY);
    add(&ep, &rf, 3, IN, 1);
    assert_eq!(subs(), 1);
    drop(ep);
    assert_eq!(subs(), 0, "the epoll's end took its registration");
    stop.store(true, Ordering::SeqCst);
    wt.join().unwrap();
    drop(w);
}

#[test]
fn epoll_wait_timeouts() {
    let ep = kernel_epoll();
    let mut out: Vec<(u32, u64)> = Vec::new();
    let t0 = Instant::now();
    assert_eq!(ep_of(&ep).wait(&mut out, 1, Some(crate::poll::now() + 30_000_000), &|| false).unwrap(), 0);
    assert!(t0.elapsed() >= Duration::from_millis(30));
    assert_eq!(ep_of(&ep).wait(&mut out, 1, Some(0), &|| false).unwrap(), 0);
}

/// An object with readiness but no `PollQueue` yet (a foreign file before its owner switched): poll and epoll
/// look at it again in slices; an EPOLLET item reports only new bits.
struct Legacy(core::sync::atomic::AtomicU32);

impl File for Legacy {
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
        Events(self.0.load(Ordering::SeqCst))
    }
}

#[test]
fn objects_without_a_poll_queue_are_looked_at_in_slices() {
    let obj = Arc::new(Legacy(core::sync::atomic::AtomicU32::new(0)));
    let of = OpenFile::anon(obj.clone(), crate::abi::O_RDWR);
    let ep = kernel_epoll();
    add(&ep, &of, 3, IN | EPOLLET, 1);
    let o2 = obj.clone();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        o2.0.store(IN, Ordering::SeqCst);
    });
    let mut out: Vec<(u32, u64)> = Vec::new();
    let n = ep_of(&ep).wait(&mut out, 4, Some(crate::poll::now() + 5_000_000_000), &|| false).unwrap();
    t.join().unwrap();
    assert_eq!((n, out), (1, vec![(IN, 1)]));
    let mut out: Vec<(u32, u64)> = Vec::new();
    assert_eq!(ep_of(&ep).wait(&mut out, 4, Some(crate::poll::now() + 40_000_000), &|| false).unwrap(), 0, "EPOLLET: no new bits, no event");
    obj.0.store(IN | OUT, Ordering::SeqCst);
    Epoll::ctl(&ep, EPOLL_CTL_MOD, &of, 3, (IN | OUT | EPOLLET, 2), false).unwrap();
    let mut out: Vec<(u32, u64)> = Vec::new();
    assert_eq!(ep_of(&ep).wait(&mut out, 4, Some(crate::poll::now() + 40_000_000), &|| false).unwrap(), 1);
    // poll(): a sleeping table looks again.
    obj.0.store(0, Ordering::SeqCst);
    let f: Arc<dyn File> = obj.clone();
    let o3 = obj.clone();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        o3.0.store(IN, Ordering::SeqCst);
    });
    let mut table = crate::poll::PollTable::new(true);
    let deadline = crate::poll::now() + 5_000_000_000;
    loop {
        table.arm();
        if table.poll(&f, IN) & IN != 0 {
            break;
        }
        table.stop_registering();
        assert!(!table.sleep(Some(deadline), &|| false), "a legacy object was never looked at again");
    }
    t.join().unwrap();
    drop(of);
}

// ---- fixer round (review findings) ----

/// Finding 1: an outer epoll (and a poll table) looking at an inner epoll while another thread harvests it is
/// told "not ready" during the scan; the harvest's end must wake it (Linux's look would wait for `ep->mtx`).
#[test]
fn a_look_during_a_harvest_is_made_good_when_the_harvest_ends() {
    let (r, w) = pipe::kernel_pipe();
    let rf = anon(r, crate::abi::O_RDONLY);
    w.write_stream(b"x", false).unwrap();
    let inner = kernel_epoll();
    let outer = kernel_epoll();
    // Level-triggered and always ready: every harvest puts it back.
    add(&inner, &rf, 3, IN, 1);
    add(&outer, &inner, 4, IN, 2);
    let stop = Arc::new(core::sync::atomic::AtomicBool::new(false));
    let (i2, s2) = (inner.clone(), stop.clone());
    let harvester = thread::spawn(move || {
        let mut n = 0u64;
        while !s2.load(Ordering::SeqCst) {
            let mut out: Vec<(u32, u64)> = Vec::new();
            n += ep_of(&i2).wait(&mut out, 4, Some(0), &|| false).unwrap() as u64;
        }
        n
    });
    let t0 = Instant::now();
    let mut rounds = 0;
    while t0.elapsed() < Duration::from_millis(400) {
        let mut out: Vec<(u32, u64)> = Vec::new();
        let n = ep_of(&outer).wait(&mut out, 1, Some(crate::poll::now() + 2_000_000_000), &|| false).unwrap();
        assert_eq!(n, 1, "round {rounds}: the outer epoll missed the inner one being ready (a look during a harvest)");
        rounds += 1;
    }
    stop.store(true, Ordering::SeqCst);
    assert!(harvester.join().unwrap() > 0);
    assert!(rounds > 10, "{rounds}");
}

/// Finding 8: EPOLLEXCLUSIVE items hang on as exclusive subscribers: a keyed wake stops after the first one that
/// woke a sleeper, a wake_all reaches all; plain subscribers are always called.
#[test]
fn exclusive_subscribers_get_one_wake_plain_ones_all() {
    struct Ex(AtomicUsize, bool);
    impl PollWaker for Ex {
        fn poll_wake(&self, _key: u32, _depth: u32) -> bool {
            self.0.fetch_add(1, Ordering::SeqCst);
            self.1
        }
    }
    let q = PollQueue::new();
    let plain = Arc::new(Ex(AtomicUsize::new(0), true));
    let idle = Arc::new(Ex(AtomicUsize::new(0), false));
    let a = Arc::new(Ex(AtomicUsize::new(0), true));
    let b = Arc::new(Ex(AtomicUsize::new(0), true));
    q.subscribe_exclusive(1, u32::MAX, idle.clone()).unwrap();
    q.subscribe_exclusive(2, u32::MAX, a.clone()).unwrap();
    q.subscribe_exclusive(3, u32::MAX, b.clone()).unwrap();
    q.subscribe(4, u32::MAX, plain.clone()).unwrap();
    q.wake_poll(IN);
    let counts = || [plain.0.load(Ordering::SeqCst), idle.0.load(Ordering::SeqCst), a.0.load(Ordering::SeqCst), b.0.load(Ordering::SeqCst)];
    assert_eq!(counts(), [1, 1, 1, 0], "one that woke nobody is passed, the next one ends the wake");
    q.wake_all();
    assert_eq!(counts(), [2, 2, 2, 1]);
}

// EPOLLEXCLUSIVE end to end (two epolls with a sleeper each, one write wakes one) is the ktest
// fs.epoll.exclusive: host threads spin instead of queueing on a WaitQueue, so no epoll has "sleepers" here.

/// Finding 6: events win over a pending signal (epoll_wait, poll); with nothing ready the wait ends with EINTR
/// at once; a kill ends even a harvest.
#[test]
fn a_pending_signal_ends_only_waits_without_events() {
    struct SigMem<'a>(&'a Mem);
    impl sys::UserMem for SigMem<'_> {
        fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
            self.0.read(addr, buf)
        }
        fn write(&self, addr: u64, data: &[u8]) -> bool {
            self.0.write(addr, data)
        }
        fn read_cstr(&self, addr: u64, max: usize) -> Result<Vec<u8>, crate::Errno> {
            self.0.read_cstr(addr, max)
        }
        fn signal_pending(&self) -> bool {
            true
        }
    }
    use crate::sys::UserMem;
    let (ns, fds) = program_view();
    let mem = Mem::new();
    let sm = SigMem(&mem);
    let s = Sys { mem: &sm, fds: &fds, ns: &ns, cred: USER, subject: SUBJECT };
    let h = H { s, mem: &mem };
    let ep = h.epoll();
    let (p0, p1) = h.pipe();
    assert_eq!(h.ctl(ep, EPOLL_CTL_ADD, p0, IN, 7), 0);
    let at = mem.alloc_zero(12 * 4);
    let t0 = Instant::now();
    assert_eq!(h.call(232, [ep, at, 4, 5000, 0, 0]), -EINTR, "nothing ready: EINTR at once");
    assert_eq!(h.call(232, [ep, at, 4, 0, 0, 0]), 0, "timeout 0: Linux returns before it looks at signals");
    let mut pfd = [0u8; 8];
    pfd[0..4].copy_from_slice(&(p0 as i32).to_le_bytes());
    pfd[4..6].copy_from_slice(&(IN as u16).to_le_bytes());
    let pa = mem.alloc(&pfd);
    assert_eq!(h.call(7, [pa, 1, 5000, 0, 0, 0]), -EINTR);
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    h.write(p1, b"x");
    assert_eq!(h.call(232, [ep, at, 4, 5000, 0, 0]), 1, "events first");
    assert_eq!(h.call(7, [pa, 1, 5000, 0, 0, 0]), 1, "poll: events first");
    // A kill ends a harvest even with events.
    let e = ep_of(&h.s.fds.get(ep as usize).unwrap()).wait(&mut Vec::new(), 4, Some(0), &|| true).map(|_| ()).unwrap_err();
    assert_eq!(e, EINTR);
}

/// Finding 9: a nameless object that names a `PollQueue` is pollable without saying so (a timerfd, signalfd).
#[test]
fn an_anonymous_object_with_a_queue_can_be_polled() {
    struct Anon(PollQueue);
    impl File for Anon {
        fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, crate::Errno> {
            Ok(0)
        }
        fn write_at(&self, _pos: u64, b: &[u8]) -> Result<usize, crate::Errno> {
            Ok(b.len())
        }
        fn mode(&self) -> u32 {
            0o600
        }
        fn events(&self) -> Events {
            Events(0)
        }
        fn poll_head(&self) -> Option<&PollQueue> {
            Some(&self.0)
        }
    }
    let of = OpenFile::anon(Arc::new(Anon(PollQueue::new())), crate::abi::O_RDWR);
    assert!(of.file.can_poll());
    let ep = kernel_epoll();
    assert_eq!(Epoll::ctl(&ep, EPOLL_CTL_ADD, &of, 3, (IN, 1), false), Ok(()));
}
