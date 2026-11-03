//! PF-3 host tests: close and dup2/dup3 take their file out of the table in the same step that changes the
//! number (Linux `close_fd_get_file`, `ksys_dup3` + `do_dup2`), so the file a caller `filp_close`s is the one
//! that left the table, and racing closes, dups and look-ups hand every installed reference back exactly once.

extern crate std;

use crate::fd::{FdTable, OpenFile, OpenFlags};
use crate::node::MountNs;
use alloc::sync::Arc;
use alloc::vec::Vec;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Barrier;
use uapi::errno::*;

std::thread_local! {
    static GROW_FAILS: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// `FdTable::grow_to` fails with ENOMEM on this thread while set (the allocation failure no host can force).
pub(crate) fn grow_fails() -> bool {
    GROW_FAILS.with(|c| c.get())
}

fn files(n: usize) -> Vec<Arc<OpenFile>> {
    let ns = MountNs::new_empty();
    ns.create_file("/f", 0o644, alloc::vec![1; 8]).unwrap();
    (0..n).map(|_| OpenFile::new(ns.open("/f").unwrap(), OpenFlags { read: true, ..Default::default() })).collect()
}

#[test]
fn close_get_hands_over_the_file_that_left() {
    let f = files(2);
    let t = FdTable::new();
    assert_eq!(t.install(f[0].clone()).unwrap(), 0);
    assert_eq!(t.install(f[1].clone()).unwrap(), 1);
    let got = t.close_get(0).unwrap();
    assert!(Arc::ptr_eq(&got, &f[0]));
    assert_eq!(t.close_get(0).err(), Some(EBADF));
    assert_eq!(t.close_get(1 << 20).err(), Some(EBADF));
    // A reserved number is not open for close (EBADF) and keeps its reservation.
    let r = t.reserve(0, false).unwrap();
    assert_eq!(r, 0);
    assert_eq!(t.close_get(r).err(), Some(EBADF));
    assert!(t.is_open(r));
    t.unreserve(r);
    // The hint went back: the closed number is the lowest free one again.
    assert_eq!(t.install(f[0].clone()).unwrap(), 0);
    drop(got);
    assert_eq!(Arc::strong_count(&f[0]), 2);
}

#[test]
fn dup_to_checks_in_ksys_dup3_order_and_returns_the_replaced_file() {
    let f = files(3);
    let t = FdTable::new();
    t.install(f[0].clone()).unwrap();
    t.install(f[1].clone()).unwrap();
    t.set_limit(64);
    assert_eq!(t.dup_to(0, 64, false).err(), Some(EBADF), "new at the limit");
    assert_eq!(t.dup_to(5, 3, false).err(), Some(EBADF), "free old");
    let r = t.reserve(0, false).unwrap();
    assert_eq!(t.dup_to(r, 3, false).err(), Some(EBADF), "reserved old is not open");
    assert_eq!(t.dup_to(0, r, false).err(), Some(EBUSY), "reserved new");
    t.commit(r, f[2].clone());
    // Over an open fd: the old file comes back, the new one carries its own close-on-exec flag.
    let old = t.dup_to(0, r, true).unwrap().unwrap();
    assert!(Arc::ptr_eq(&old, &f[2]));
    assert!(Arc::ptr_eq(&t.get(r).unwrap(), &f[0]));
    assert_eq!(t.cloexec(r), Ok(true));
    assert_eq!(t.cloexec(0), Ok(false));
    // Onto a free number far up: the table grows, nothing is replaced.
    assert!(t.dup_to(1, 40, false).unwrap().is_none());
    assert!(Arc::ptr_eq(&t.get(40).unwrap(), &f[1]));
    assert_eq!(t.max_fds(), 64);
    // dup onto itself replaces the file with itself (dup3 refuses it earlier; the table step stays sane).
    assert!(Arc::ptr_eq(&t.dup_to(1, 1, false).unwrap().unwrap(), &f[1]));
}

#[test]
fn close_on_exec_returns_what_it_closed() {
    let f = files(3);
    let t = FdTable::new();
    for x in &f {
        t.install(x.clone()).unwrap();
    }
    t.set_cloexec(0, true).unwrap();
    t.set_cloexec(2, true).unwrap();
    let gone = t.close_on_exec();
    assert_eq!(gone.len(), 2);
    assert!(Arc::ptr_eq(&gone[0], &f[0]) && Arc::ptr_eq(&gone[1], &f[2]));
    assert_eq!(t.count(), 1);
    assert!(t.close_on_exec().is_empty());
}

/// Two threads dup2 their own file onto fd 9, one closes fd 9, one looks it up: a look-up only ever sees one of
/// the two files, and every reference installed at 9 comes back exactly once (by a close, as the file a dup
/// replaced, or still in the table at the end), so `filp_close` work is never done twice or on the wrong file.
#[test]
fn racing_close_dup2_and_lookup_hand_back_each_reference_once() {
    const N: usize = 9;
    let f = files(2);
    let t = Arc::new(FdTable::new());
    t.install(f[0].clone()).unwrap();
    t.install(f[1].clone()).unwrap();
    // installs[i]: dups of file i onto N that succeeded; back[i]: references of file i that came back.
    let installs = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
    let back = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
    let which = |x: &Arc<OpenFile>, f: &[Arc<OpenFile>]| if Arc::ptr_eq(x, &f[0]) { 0 } else if Arc::ptr_eq(x, &f[1]) { 1 } else { panic!("a file that was never at fd {N}") };
    let barrier = Arc::new(Barrier::new(4));
    let mut hs = Vec::new();
    for src in 0..2usize {
        let (t, b, ins, back, f) = (t.clone(), barrier.clone(), installs.clone(), back.clone(), f.clone());
        hs.push(std::thread::spawn(move || {
            b.wait();
            for _ in 0..20000 {
                let replaced = t.dup_to(src, N, false).unwrap();
                ins[src].fetch_add(1, Ordering::SeqCst);
                if let Some(r) = replaced {
                    back[which(&r, &f)].fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }
    {
        let (t, b, back, f) = (t.clone(), barrier.clone(), back.clone(), f.clone());
        hs.push(std::thread::spawn(move || {
            b.wait();
            for _ in 0..20000 {
                if let Ok(r) = t.close_get(N) {
                    back[which(&r, &f)].fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }
    {
        let (t, b, f) = (t.clone(), barrier.clone(), f.clone());
        hs.push(std::thread::spawn(move || {
            b.wait();
            for _ in 0..20000 {
                if let Ok(r) = t.get(N) {
                    which(&r, &f);
                }
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    if let Ok(r) = t.close_get(N) {
        back[which(&r, &f)].fetch_add(1, Ordering::SeqCst);
    }
    for i in 0..2 {
        assert_eq!(installs[i].load(Ordering::SeqCst), back[i].load(Ordering::SeqCst), "file {i}");
        // The test's handle, the table's fd i, the clone in `f` the threads shared (dropped by now).
        assert_eq!(Arc::strong_count(&f[i]), 2);
    }
}

/// The lockless view: a look-up of a published number takes no lock, and every way a file leaves a number
/// makes look-ups of that number see the change, also while the file itself stays open through another fd.
#[test]
fn lockless_lookups_follow_every_change_of_the_number() {
    lookups_follow_every_change(&FdTable::new(), true);
}

/// The same with an empty view (its nodes found no room): every look-up takes the locked path.
#[test]
fn without_a_view_every_lookup_locks_and_answers_the_same() {
    let t = FdTable::new_unviewed();
    lookups_follow_every_change(&t, false);
    assert!(t.lookup_stats().locked > 0);
}

fn lookups_follow_every_change(t: &FdTable, viewed: bool) {
    let f = files(3);
    assert_eq!(t.install(f[0].clone()).unwrap(), 0);
    assert_eq!(t.dup_to(0, 1, false).unwrap().map(|_| ()), None);
    assert!(Arc::ptr_eq(&t.get(0).unwrap(), &f[0]));
    let before = t.lookup_stats();
    assert!(Arc::ptr_eq(&t.get(0).unwrap(), &f[0]));
    assert_eq!(t.lookup_stats().locked, before.locked + !viewed as u64, "a look-up locks only without the view");
    // close: fd 1 keeps the file alive, fd 0 must still be gone.
    drop(t.close_get(0).unwrap());
    assert_eq!(t.get(0).err(), Some(EBADF));
    // The number is reused by another file: the look-up finds the new one.
    assert_eq!(t.install(f[1].clone()).unwrap(), 0);
    assert!(Arc::ptr_eq(&t.get(0).unwrap(), &f[1]));
    assert!(Arc::ptr_eq(&t.get(0).unwrap(), &f[1]));
    // dup2 over a number just looked up.
    assert!(Arc::ptr_eq(&t.dup_to(1, 0, false).unwrap().unwrap(), &f[1]));
    assert!(Arc::ptr_eq(&t.get(0).unwrap(), &f[0]));
    // install_at over a number just looked up.
    t.install_at(0, f[2].clone()).unwrap();
    assert!(Arc::ptr_eq(&t.get(0).unwrap(), &f[2]));
    // close-on-exec and exit.
    t.set_cloexec(0, true).unwrap();
    assert!(Arc::ptr_eq(&t.get(0).unwrap(), &f[2]));
    drop(t.close_on_exec());
    assert_eq!(t.get(0).err(), Some(EBADF));
    assert!(Arc::ptr_eq(&t.get(1).unwrap(), &f[0]));
    t.close_all();
    assert_eq!(t.get(1).err(), Some(EBADF));
    // A reserved number is invisible until published, with a view or not.
    let r = t.reserve(0, false).unwrap();
    assert_eq!(t.get(r).err(), Some(EBADF));
    t.commit(r, f[1].clone());
    assert!(Arc::ptr_eq(&t.get(r).unwrap(), &f[1]));
    // The view holds no reference: once the table let go, only the test's handles are left.
    t.close_all();
    for x in &f {
        assert_eq!(Arc::strong_count(x), 1);
    }
}

/// fork's copy starts with a view of its own: a close in the parent does not reach the child's look-ups and
/// the other way round.
#[test]
fn a_duplicated_table_looks_up_apart() {
    let f = files(1);
    let t = FdTable::new();
    t.install(f[0].clone()).unwrap();
    assert!(t.get(0).is_ok());
    let c = t.duplicate().unwrap();
    assert!(c.get(0).is_ok());
    drop(t.close_get(0).unwrap());
    assert_eq!(t.get(0).err(), Some(EBADF));
    assert!(Arc::ptr_eq(&c.get(0).unwrap(), &f[0]));
    drop(c.close_get(0).unwrap());
    assert_eq!(c.get(0).err(), Some(EBADF));
    assert_eq!(Arc::strong_count(&f[0]), 1);
}

/// dup2 to a number the table cannot grow to: a free `old` is EBADF before the growth's ENOMEM shows
/// (ksys_dup3: `files_lookup_fd_locked` fails first), an open `old` gets ENOMEM, nothing changes, and a `new`
/// the table already covers needs no growth.
#[test]
fn dup_to_answers_enomem_only_after_the_ebadf_checks() {
    let f = files(1);
    let t = FdTable::new();
    assert_eq!(t.install(f[0].clone()).unwrap(), 0);
    let r = t.reserve(0, false).unwrap();
    assert_eq!(r, 1);
    assert!(t.dup_to(0, 3, false).unwrap().is_none());
    let len = t.capacity();
    assert!(len >= 4);
    GROW_FAILS.with(|c| c.set(true));
    let far = t.limit() - 1;
    assert_eq!(t.dup_to(2, far, false).err(), Some(EBADF), "free old");
    assert_eq!(t.dup_to(r, far, false).err(), Some(EBADF), "reserved old");
    assert_eq!(t.dup_to(0, far, false).err(), Some(ENOMEM), "open old");
    assert_eq!(t.dup_to(0, 2, false).map(|o| o.is_none()), Ok(true), "a covered new needs no growth");
    GROW_FAILS.with(|c| c.set(false));
    assert_eq!(t.capacity(), len);
    assert_eq!(t.count(), 3);
    t.unreserve(r);
    t.close_all();
    assert_eq!(Arc::strong_count(&f[0]), 1);
}

/// What `close_get` / `dup_to` changed is seen by every look-up that starts after they returned, with a view or not:
/// a reader that sees the round's "done" flag before and after its look-up must not get the old file. Round
/// r puts file 0 at N, then closes N (even r) or dups file 1 over it (odd r); the phase is odd while N changes.
#[test]
fn lookups_after_a_close_or_dup_returned_never_see_the_old_file() {
    const N: usize = 7;
    const ROUNDS: u64 = 4000;
    let f = files(2);
    let t = Arc::new(FdTable::new());
    t.install(f[0].clone()).unwrap();
    t.install(f[1].clone()).unwrap();
    let phase = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let (stale, checks) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let mut hs = Vec::new();
    for _ in 0..3 {
        let (t, f, phase, stop, stale, checks) = (t.clone(), f.clone(), phase.clone(), stop.clone(), stale.clone(), checks.clone());
        hs.push(std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let p = phase.load(Ordering::SeqCst);
                let got = t.get(N);
                if p == 0 || p % 2 == 1 || phase.load(Ordering::SeqCst) != p {
                    continue;
                }
                checks.fetch_add(1, Ordering::Relaxed);
                let round = p / 2 - 1;
                let ok = if round % 2 == 0 { got.err() == Some(EBADF) } else { got.is_ok_and(|g| Arc::ptr_eq(&g, &f[1])) };
                if !ok {
                    stale.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }
    for r in 0..ROUNDS {
        phase.store(2 * r + 1, Ordering::SeqCst);
        drop(t.dup_to(0, N, false).unwrap());
        for _ in 0..50 {
            assert!(t.get(N).is_ok());
        }
        if r % 2 == 0 {
            drop(t.close_get(N).unwrap());
        } else {
            drop(t.dup_to(1, N, false).unwrap());
        }
        phase.store(2 * r + 2, Ordering::SeqCst);
        std::thread::yield_now();
    }
    stop.store(true, Ordering::SeqCst);
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(stale.load(Ordering::SeqCst), 0, "look-ups after the change returned saw the old state");
    assert!(checks.load(Ordering::Relaxed) > 0, "no look-up ran in a settled phase");
}

/// fork's copy (`duplicate`) while close and dup2 change one number: the child holds the number with one of
/// the two files or not at all, keeps the other numbers, and every reference comes back once the copies go.
#[test]
fn duplicate_racing_close_and_dup_copies_a_whole_state() {
    const N: usize = 9;
    let f = files(2);
    let t = Arc::new(FdTable::new());
    t.install(f[0].clone()).unwrap();
    t.install(f[1].clone()).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let mut hs = Vec::new();
    for role in 0..3usize {
        let (t, stop) = (t.clone(), stop.clone());
        hs.push(std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match role {
                    0 | 1 => drop(t.dup_to(role, N, role == 1).unwrap()),
                    _ => drop(t.close_get(N)),
                }
            }
        }));
    }
    let mut seen = [0u64; 3];
    for _ in 0..2000 {
        let c = t.duplicate().unwrap();
        assert!(Arc::ptr_eq(&c.get(0).unwrap(), &f[0]) && Arc::ptr_eq(&c.get(1).unwrap(), &f[1]));
        match c.get(N) {
            Ok(g) if Arc::ptr_eq(&g, &f[0]) => {
                assert_eq!(c.cloexec(N), Ok(false), "file 0 came with its own flag");
                seen[0] += 1;
            }
            Ok(g) if Arc::ptr_eq(&g, &f[1]) => {
                assert_eq!(c.cloexec(N), Ok(true), "file 1 came with its own flag");
                seen[1] += 1;
            }
            Ok(_) => panic!("a file never put at {N}"),
            Err(e) => {
                assert_eq!(e, EBADF);
                seen[2] += 1;
            }
        }
        assert!(c.count() == 2 || c.count() == 3);
    }
    stop.store(true, Ordering::SeqCst);
    for h in hs {
        h.join().unwrap();
    }
    drop(t.close_get(N));
    // The test's handle, the table's fd 0/1, the clone in `f`; every child copy is gone.
    for x in &f {
        assert_eq!(Arc::strong_count(x), 2);
    }
    assert!(seen.iter().sum::<u64>() == 2000);
}

/// PF-6 fixer: one thread closes the number and opens the next of many files there, round after round, while
/// readers look it up. A reader first reads how many rounds' closes have returned (`closed`), then looks up: it
/// must find nothing (EBADF) or a file installed after that close, never one already closed. Every file stays
/// alive (the test holds it), so a view entry left behind by a close would still upgrade and be caught.
#[test]
fn close_and_reopen_on_one_number_never_hands_out_a_closed_file() {
    const K: usize = 32;
    const ROUNDS: u64 = 4000;
    const READERS: usize = 4;
    const NUM: usize = 5;
    let f = Arc::new(files(K));
    let t = Arc::new(FdTable::new());
    // gen_of[k]: the round that last installed file k (stored before the install).
    let gen_of: Arc<Vec<AtomicU64>> = Arc::new((0..K).map(|_| AtomicU64::new(0)).collect());
    let closed = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let bad = Arc::new(AtomicU64::new(0));
    let found = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for _ in 0..READERS {
        let (t, f, gen_of, closed, stop, bad, found) = (t.clone(), f.clone(), gen_of.clone(), closed.clone(), stop.clone(), bad.clone(), found.clone());
        hs.push(std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let d = closed.load(Ordering::SeqCst);
                match t.get(NUM) {
                    Err(e) => assert_eq!(e, EBADF),
                    Ok(g) => {
                        found.fetch_add(1, Ordering::Relaxed);
                        let k = f.iter().position(|x| Arc::ptr_eq(x, &g)).expect("a file the test never installed");
                        if gen_of[k].load(Ordering::SeqCst) <= d {
                            bad.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                }
            }
        }));
    }
    for r in 1..=ROUNDS {
        let k = r as usize % K;
        gen_of[k].store(r, Ordering::SeqCst);
        t.install_at(NUM, f[k].clone()).unwrap();
        for _ in 0..50 {
            std::hint::spin_loop();
        }
        drop(t.close_get(NUM).unwrap());
        closed.store(r, Ordering::SeqCst);
    }
    stop.store(true, Ordering::SeqCst);
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(bad.load(Ordering::SeqCst), 0, "look-ups found a file whose close had returned");
    assert!(found.load(Ordering::Relaxed) > 0 || std::thread::available_parallelism().map_or(true, |n| n.get() < 2), "no look-up found a file");
    t.close_all();
    for x in f.iter() {
        assert_eq!(Arc::strong_count(x), 1);
    }
}
