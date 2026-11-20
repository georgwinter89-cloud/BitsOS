//! FSR: when an open file's position is locked (`fpos`, Linux 7 `fdget_pos`). Judged by order, never by a time a
//! busy machine may miss: a second user that must wait is still waiting while the first holds the position, one
//! that must not finishes while it is held.

extern crate std;

use crate::devfs::{Mem, MemDev};
use crate::fd::{FdTable, OpenFile, OpenFlags};
use crate::fpos::{FdUse, FilePos, Kind};
use crate::mount_ns::MountNs;
use alloc::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// `f` must finish while the caller still holds what it must not wait for (10 s: it waits, or the machine stalls).
fn finishes_while_held(f: impl FnOnce() + Send + 'static) -> bool {
    let (tx, rx) = mpsc::channel();
    let h = thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    let done = rx.recv_timeout(Duration::from_secs(10)).is_ok();
    if done {
        h.join().unwrap();
    }
    done
}

/// `f` must still be waiting 100 ms after it began (a right answer never comes early, also on a busy machine) and
/// finish once `release` let go of what it waits for.
fn waits_until_released(f: impl FnOnce() + Send + 'static, release: impl FnOnce()) -> bool {
    let (tx, rx) = mpsc::channel();
    let h = thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    let early = rx.recv_timeout(Duration::from_millis(100)).is_ok();
    release();
    if !early {
        rx.recv_timeout(Duration::from_secs(10)).expect("the waiter never got the position");
    }
    h.join().unwrap();
    !early
}

#[test]
fn a_device_position_is_never_locked() {
    let p = Arc::new(FilePos::new(Kind::Free));
    let mut g = p.guard(false);
    *g = 7;
    let q = p.clone();
    assert!(finishes_while_held(move || drop(q.guard(false))));
    drop(g);
}

#[test]
fn a_shared_regular_file_waits_and_sees_the_new_position() {
    let p = Arc::new(FilePos::new(Kind::Regular));
    let mut g = p.guard(false);
    let q = p.clone();
    let (tx, rx) = mpsc::channel();
    let waited = waits_until_released(move || tx.send(*q.guard(false)).unwrap(), || {
        *g = 42;
        drop(g);
    });
    assert!(waited, "a second user of a shared description did not wait for the first");
    assert_eq!(rx.recv().unwrap(), 42, "the waiter read the position from before the first user's end");
}

#[test]
fn a_regular_file_of_one_user_takes_no_lock() {
    let p = Arc::new(FilePos::new(Kind::Regular));
    let g = p.guard(false);
    let q = p.clone();
    assert!(finishes_while_held(move || drop(q.guard(true))), "the only user waited for the position lock");
    drop(g);
}

#[test]
fn a_directory_is_always_locked() {
    let p = Arc::new(FilePos::new(Kind::Dir));
    let g = p.guard(true);
    let q = p.clone();
    assert!(waits_until_released(move || drop(q.guard(true)), || drop(g)), "getdents of a directory must hold its position");
}

#[test]
fn alone_needs_one_counted_thread_and_one_slot() {
    let ns = MountNs::new_empty();
    ns.create_file("/f", 0o644, alloc::vec![1u8; 10]).unwrap();
    let t = FdTable::new();
    let fd = t.install(OpenFile::new(ns.open("/f").unwrap(), OpenFlags { read: true, ..Default::default() })).unwrap();
    assert!(!t.get(fd).unwrap().alone(&t), "a table nobody counts belongs to no one thread");
    let one = FdUse::new(&t);
    assert!(t.get(fd).unwrap().alone(&t));
    let dup = t.install(t.get(fd).unwrap()).unwrap();
    assert!(!t.get(fd).unwrap().alone(&t), "a dup shares the description");
    t.close(dup).unwrap();
    assert!(t.get(fd).unwrap().alone(&t));
    let held = t.get(fd).unwrap();
    assert!(!t.get(fd).unwrap().alone(&t), "another holder (an epoll item, a mapping, a descriptor in flight)");
    drop(held);
    let two = FdUse::new(&t);
    assert!(!t.get(fd).unwrap().alone(&t), "a second thread on the table");
    drop((one, two));
    assert!(!t.get(fd).unwrap().alone(&t));
}

#[test]
fn only_regular_files_and_directories_lock() {
    let ns = MountNs::new_empty();
    ns.create_file("/f", 0o644, alloc::vec![1u8; 10]).unwrap();
    let null = OpenFile::new(Arc::new(MemDev(Mem::Null)), OpenFlags { write: true, ..Default::default() });
    let reg = OpenFile::new(ns.open("/f").unwrap(), OpenFlags { read: true, ..Default::default() });
    // Another thread is inside a call on each of them: only the device's calls go on meanwhile.
    let (n2, r2) = (null.clone(), reg.clone());
    let (go, wait) = mpsc::channel::<()>();
    let (held_tx, held_rx) = mpsc::channel();
    let holder = thread::spawn(move || {
        n2.with_pos(|_| {
            r2.with_pos(|_| {
                held_tx.send(()).unwrap();
                wait.recv().unwrap();
            })
        })
    });
    held_rx.recv().unwrap();
    let (n3, n4, r3) = (null.clone(), null.clone(), reg.clone());
    assert!(finishes_while_held(move || assert_eq!(n3.write(b"x"), Ok(1))), "/dev/null's write waited");
    assert!(finishes_while_held(move || assert_eq!(n4.seek(0, crate::abi::SEEK_SET), Ok(0))), "/dev/null's lseek waited");
    assert!(waits_until_released(move || assert_eq!(r3.seek(0, crate::abi::SEEK_SET), Ok(0)), || go.send(()).unwrap()), "a shared regular file's lseek did not wait");
    holder.join().unwrap();
}

/// Fixer round: each description counts the calls that took its lock (the ktest's measure of one description,
/// unmixed with the kernel-wide lock class).
#[test]
fn each_description_counts_its_own_locked_calls() {
    let (dev, reg, dir) = (FilePos::new(Kind::Free), FilePos::new(Kind::Regular), FilePos::new(Kind::Dir));
    for alone in [false, true] {
        drop(dev.guard(alone));
        drop(reg.guard(alone));
        drop(dir.guard(alone));
    }
    assert_eq!((dev.locks(), reg.locks(), dir.locks()), (0, 1, 2));
    let other = FilePos::new(Kind::Regular);
    assert_eq!(other.locks(), 0, "another description's calls were counted");
}
