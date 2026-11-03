//! PF-3 ktests: close and dup2 on one fd number from kernel threads on several CPUs (`-- -smp 16`). Every
//! reference dup2 put at the number comes back exactly once (a close took it, a later dup2 replaced it, or it
//! is still there at the end) and a look-up only ever finds one of the two files: the table step that changes
//! the number is the one that hands the file out (`FdTable::close_get`, `FdTable::dup_to`), and a look-up that
//! starts after such a change returned, on any CPU, never finds the old file. On the Mini-PC the Safe tests
//! run pinned to CPU 0: every wait yields.

use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ktest::{TestResult, kassert, ktest};
use vfs::fd::{FdTable, OpenFile};

const SEC: u64 = 1_000_000_000;
const NUM: usize = 9;
const ROUNDS: u64 = 20_000;

ktest!("fs.pf3.close_dup_race", Safe, close_dup_race);
ktest!("fs.pf3.lookup_cache", Safe, lookup_cache);
ktest!("fs.pf3.no_stale_lookup", Safe, no_stale_lookup);
ktest!("fs.pf3.close_reopen", Safe, close_reopen);

/// One fd per kernel thread (PF-1's fd-per-thread layout), threads on every CPU: every look-up runs without
/// the table lock (PF-3b's view), each finds its own file, and a close in between is seen at once.
fn lookup_cache() -> TestResult {
    const THREADS: usize = 8;
    const LOOKUPS: u64 = 10_000;
    let t = Arc::new(FdTable::new());
    let mut files = Vec::new();
    for _ in 0..THREADS {
        let (r, _w) = vfs::pipe::new_pipe(vfs::inode::Cred::ROOT, 0).map_err(|e| format!("pipe: {e}"))?;
        t.install(r.clone()).map_err(|e| format!("install: {e}"))?;
        files.push(r);
    }
    let wrong = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for (fd, f) in files.iter().enumerate() {
        let (t, f, wrong) = (t.clone(), f.clone(), wrong.clone());
        hs.push(
            sched::spawn("fs-ktest-pf3c", sched::Class::Normal, move || {
                for _ in 0..LOOKUPS {
                    if !t.get(fd).is_ok_and(|g| Arc::ptr_eq(&g, &f)) {
                        wrong.fetch_add(1, Ordering::SeqCst);
                    }
                }
            })
            .map_err(|e| format!("spawn: {e:?}"))?,
        );
    }
    let mut joined = true;
    for h in hs {
        joined &= h.join_timeout(10 * SEC);
    }
    kassert!(joined, "a helper thread did not finish in 10 s");
    kassert!(wrong.load(Ordering::SeqCst) == 0, "{} look-ups found another file or none", wrong.load(Ordering::SeqCst));
    // PF-3b: a published number is always found without the table lock, on every CPU.
    let s = t.lookup_stats();
    kassert!(s.locked == 0, "{} of {} look-ups took the table lock", s.locked, THREADS as u64 * LOOKUPS);
    // A close is seen by the next look-up on any CPU.
    let gone = t.close_get(0).map_err(|e| format!("close: {e}"))?;
    kassert!(t.get(0).err() == Some(uapi::errno::EBADF), "fd 0 still found after its close");
    drop(gone);
    t.close_all();
    for f in &files {
        kassert!(Arc::strong_count(f) == 1, "a reference left after close_all");
    }
    Ok(())
}

fn close_dup_race() -> TestResult {
    let (r0, _w0) = vfs::pipe::new_pipe(vfs::inode::Cred::ROOT, 0).map_err(|e| format!("pipe: {e}"))?;
    let (r1, _w1) = vfs::pipe::new_pipe(vfs::inode::Cred::ROOT, 0).map_err(|e| format!("pipe: {e}"))?;
    let files: Arc<[Arc<OpenFile>; 2]> = Arc::new([r0, r1]);
    let t = Arc::new(FdTable::new());
    for f in files.iter() {
        t.install(f.clone()).map_err(|e| format!("install: {e}"))?;
    }
    let which = |x: &Arc<OpenFile>, f: &[Arc<OpenFile>; 2]| f.iter().position(|y| Arc::ptr_eq(x, y));
    // [installs of 0, of 1, back of 0, back of 1, odd answers]
    let n: Arc<[AtomicU64; 5]> = Arc::new(Default::default());
    let ready = Arc::new(AtomicU64::new(0));
    let go = Arc::new(AtomicBool::new(false));
    let start = |ready: &AtomicU64, go: &AtomicBool| {
        ready.fetch_add(1, Ordering::SeqCst);
        let deadline = time::now_ns() + 2 * SEC;
        while !go.load(Ordering::Acquire) && time::now_ns() < deadline {
            sched::yield_now();
        }
    };
    let mut hs = Vec::new();
    for role in 0..4usize {
        let (t, files, n, ready, go) = (t.clone(), files.clone(), n.clone(), ready.clone(), go.clone());
        let h = sched::spawn("fs-ktest-pf3", sched::Class::Normal, move || {
            start(&ready, &go);
            for _ in 0..ROUNDS {
                match role {
                    0 | 1 => match t.dup_to(role, NUM, false) {
                        Ok(replaced) => {
                            n[role].fetch_add(1, Ordering::SeqCst);
                            if let Some(r) = replaced {
                                match which(&r, &files) {
                                    Some(i) => n[2 + i].fetch_add(1, Ordering::SeqCst),
                                    None => n[4].fetch_add(1, Ordering::SeqCst),
                                };
                            }
                        }
                        Err(_) => {
                            n[4].fetch_add(1, Ordering::SeqCst);
                        }
                    },
                    2 => {
                        if let Ok(r) = t.close_get(NUM) {
                            match which(&r, &files) {
                                Some(i) => n[2 + i].fetch_add(1, Ordering::SeqCst),
                                None => n[4].fetch_add(1, Ordering::SeqCst),
                            };
                        }
                    }
                    _ => match t.get(NUM) {
                        Ok(r) if which(&r, &files).is_none() => {
                            n[4].fetch_add(1, Ordering::SeqCst);
                        }
                        Err(e) if e != uapi::errno::EBADF => {
                            n[4].fetch_add(1, Ordering::SeqCst);
                        }
                        _ => {}
                    },
                }
            }
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
        hs.push(h);
    }
    let deadline = time::now_ns() + 2 * SEC;
    while ready.load(Ordering::SeqCst) < 4 && time::now_ns() < deadline {
        sched::yield_now();
    }
    go.store(true, Ordering::Release);
    let mut joined = true;
    for h in hs {
        joined &= h.join_timeout(10 * SEC);
    }
    kassert!(joined, "a helper thread did not finish in 10 s");
    if let Ok(r) = t.close_get(NUM) {
        match which(&r, &files) {
            Some(i) => n[2 + i].fetch_add(1, Ordering::SeqCst),
            None => n[4].fetch_add(1, Ordering::SeqCst),
        };
    }
    let v: Vec<u64> = n.iter().map(|a| a.load(Ordering::SeqCst)).collect();
    kassert!(v[4] == 0, "odd answers: {}", v[4]);
    kassert!(v[0] == v[2] && v[1] == v[3], "references put at fd {} vs handed back: {:?}", NUM, v);
    t.close_all();
    for (i, f) in files.iter().enumerate() {
        kassert!(Arc::strong_count(f) == 1, "file {}: {} references left after close_all", i, Arc::strong_count(f));
    }
    Ok(())
}

/// Round r puts file 0 at NUM, then closes NUM (even r) or dups file 1 over it (odd r); the phase is odd while
/// NUM changes. Readers on other CPUs (looking file 0 up without the lock) that read the same even phase before and
/// after a look-up must find nothing (after a close) or file 1 (after a dup2), never file 0.
fn no_stale_lookup() -> TestResult {
    const READERS: usize = 6;
    const ROUNDS: u64 = 2000;
    let (r0, _w0) = vfs::pipe::new_pipe(vfs::inode::Cred::ROOT, 0).map_err(|e| format!("pipe: {e}"))?;
    let (r1, _w1) = vfs::pipe::new_pipe(vfs::inode::Cred::ROOT, 0).map_err(|e| format!("pipe: {e}"))?;
    let files: Arc<[Arc<OpenFile>; 2]> = Arc::new([r0, r1]);
    let t = Arc::new(FdTable::new());
    for f in files.iter() {
        t.install(f.clone()).map_err(|e| format!("install: {e}"))?;
    }
    let phase = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    // [stale answers, checks]
    let n: Arc<[AtomicU64; 2]> = Arc::new(Default::default());
    let mut hs = Vec::new();
    for _ in 0..READERS {
        let (t, files, phase, stop, n) = (t.clone(), files.clone(), phase.clone(), stop.clone(), n.clone());
        let h = sched::spawn("fs-ktest-pf3s", sched::Class::Normal, move || {
            let deadline = time::now_ns() + 10 * SEC;
            let mut i = 0u32;
            while !stop.load(Ordering::SeqCst) && time::now_ns() < deadline {
                let p = phase.load(Ordering::SeqCst);
                let got = t.get(NUM);
                if p != 0 && p % 2 == 0 && phase.load(Ordering::SeqCst) == p {
                    n[1].fetch_add(1, Ordering::Relaxed);
                    let ok = if (p / 2 - 1) % 2 == 0 {
                        got.err() == Some(uapi::errno::EBADF)
                    } else {
                        got.is_ok_and(|g| Arc::ptr_eq(&g, &files[1]))
                    };
                    if !ok {
                        n[0].fetch_add(1, Ordering::SeqCst);
                    }
                }
                i = i.wrapping_add(1);
                if i % 64 == 0 {
                    sched::yield_now();
                }
            }
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
        hs.push(h);
    }
    let mut failed = None;
    for r in 0..ROUNDS {
        phase.store(2 * r + 1, Ordering::SeqCst);
        if let Err(e) = t.dup_to(0, NUM, false) {
            failed = Some(format!("dup2 of file 0: {e}"));
            break;
        }
        // Readers look file 0 up meanwhile.
        settle(20_000);
        let changed = if r % 2 == 0 { t.close_get(NUM).map(drop) } else { t.dup_to(1, NUM, false).map(drop) };
        if let Err(e) = changed {
            failed = Some(format!("round {r}: {e}"));
            break;
        }
        phase.store(2 * r + 2, Ordering::SeqCst);
        // Until a reader checked this phase (bounded: 1 ms).
        let (seen, until) = (n[1].load(Ordering::Relaxed), time::now_ns() + 1_000_000);
        while n[1].load(Ordering::Relaxed) == seen && time::now_ns() < until {
            sched::yield_now();
        }
    }
    stop.store(true, Ordering::SeqCst);
    let mut joined = true;
    for h in hs {
        joined &= h.join_timeout(15 * SEC);
    }
    kassert!(joined, "a reader did not finish in 15 s");
    if let Some(e) = failed {
        return Err(e);
    }
    let (stale, checks) = (n[0].load(Ordering::SeqCst), n[1].load(Ordering::Relaxed));
    kassert!(stale == 0, "{} of {} look-ups after the change returned found the old state", stale, checks);
    kassert!(checks > 0, "no look-up ran in a settled phase");
    klog::event!(Info, "vfs", "pf3.no_stale", rounds = ROUNDS, checks = checks; "fd look-ups checked after a change returned");
    t.close_all();
    for (i, f) in files.iter().enumerate() {
        kassert!(Arc::strong_count(f) == 1, "file {}: {} references left after close_all", i, Arc::strong_count(f));
    }
    Ok(())
}

/// PF-6 fixer: one thread closes the number and opens the next of many files there (close + install, as
/// close(2) then open(2) taking the freed number), round after round, while readers on other CPUs look it up.
/// A reader first reads how many rounds' closes have returned, then looks up: it must find nothing (EBADF) or
/// a file installed after that close, never one whose close returned (a stale lockless entry). Every file stays
/// alive meanwhile (held here), so a view entry a close left behind would still upgrade and be caught.
/// Rerunnable: its own table and pipes.
fn close_reopen() -> TestResult {
    const K: usize = 16;
    const READERS: usize = 6;
    const ROUNDS: u64 = 3000;
    let mut v = Vec::new();
    for _ in 0..K {
        let (r, _w) = vfs::pipe::new_pipe(vfs::inode::Cred::ROOT, 0).map_err(|e| format!("pipe: {e}"))?;
        v.push(r);
    }
    let files: Arc<Vec<Arc<OpenFile>>> = Arc::new(v);
    let t = Arc::new(FdTable::new());
    // gen_of[k]: the round that last installed file k (stored before the install).
    let gen_of: Arc<Vec<AtomicU64>> = Arc::new((0..K).map(|_| AtomicU64::new(0)).collect());
    let closed = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    // [stale answers, odd answers, files found]
    let n: Arc<[AtomicU64; 3]> = Arc::new(Default::default());
    let mut hs = Vec::new();
    for _ in 0..READERS {
        let (t, files, gen_of, closed, stop, n) = (t.clone(), files.clone(), gen_of.clone(), closed.clone(), stop.clone(), n.clone());
        let h = sched::spawn("fs-ktest-pf3r", sched::Class::Normal, move || {
            let deadline = time::now_ns() + 10 * SEC;
            let mut i = 0u32;
            while !stop.load(Ordering::SeqCst) && time::now_ns() < deadline {
                let d = closed.load(Ordering::SeqCst);
                match t.get(NUM) {
                    Err(e) if e == uapi::errno::EBADF => {}
                    Err(_) => {
                        n[1].fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(g) => {
                        n[2].fetch_add(1, Ordering::Relaxed);
                        match files.iter().position(|x| Arc::ptr_eq(x, &g)) {
                            Some(k) if gen_of[k].load(Ordering::SeqCst) > d => {}
                            Some(_) => {
                                n[0].fetch_add(1, Ordering::SeqCst);
                            }
                            None => {
                                n[1].fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                }
                i = i.wrapping_add(1);
                if i % 64 == 0 {
                    sched::yield_now();
                }
            }
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
        hs.push(h);
    }
    let mut failed = None;
    for r in 1..=ROUNDS {
        let k = r as usize % K;
        gen_of[k].store(r, Ordering::SeqCst);
        if let Err(e) = t.install_at(NUM, files[k].clone()) {
            failed = Some(format!("round {r}: install: {e}"));
            break;
        }
        if r % 8 == 0 {
            settle(10_000);
        }
        if let Err(e) = t.close_get(NUM).map(drop) {
            failed = Some(format!("round {r}: close: {e}"));
            break;
        }
        closed.store(r, Ordering::SeqCst);
    }
    stop.store(true, Ordering::SeqCst);
    let mut joined = true;
    for h in hs {
        joined &= h.join_timeout(15 * SEC);
    }
    kassert!(joined, "a reader did not finish in 15 s");
    if let Some(e) = failed {
        return Err(e);
    }
    let (stale, odd, found) = (n[0].load(Ordering::SeqCst), n[1].load(Ordering::SeqCst), n[2].load(Ordering::Relaxed));
    kassert!(stale == 0, "{} look-ups found a file whose close had returned ({} found a file)", stale, found);
    kassert!(odd == 0, "{} look-ups answered another errno or an unknown file", odd);
    klog::event!(Info, "vfs", "pf3.close_reopen", rounds = ROUNDS, found = found; "fd look-ups checked against closes that returned");
    t.close_all();
    for (i, f) in files.iter().enumerate() {
        kassert!(Arc::strong_count(f) == 1, "file {}: {} references left after close_all", i, Arc::strong_count(f));
    }
    Ok(())
}

/// Gives way for about `ns` nanoseconds (other threads on this CPU run meanwhile).
fn settle(ns: u64) {
    let until = time::now_ns() + ns;
    while time::now_ns() < until {
        sched::yield_now();
    }
}
