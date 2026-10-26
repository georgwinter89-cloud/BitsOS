//! SC-5 ktests of the futex core on kernel memory (`KSpace`: words in a kernel buffer, keys by buffer and
//! address), so the queue, wake, requeue and timeout logic is checked with many kernel threads without any
//! user address space. The syscall-level tests (user memory, keys, robust lists, programs) are in
//! `syscall::futex_ktests`. All `Safe` and rerunnable.

use super::{Deadline, Key, KeyKind, KeyRef, Space};
use crate::Class;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use ksync::user_word::{WordError, WordOp, apply};
use ktest::{TestResult, kassert, ktest};
use uapi::errno::*;

pub(super) const SEC: u64 = 1_000_000_000;
pub(super) const MS: u64 = 1_000_000;
const BASE: u64 = 0x10_0000;

pub(super) struct KSpace {
    words: Box<[AtomicU32]>,
    pub(super) stop: AtomicBool,
}

impl KSpace {
    pub(super) fn new(n: usize) -> Arc<KSpace> {
        Arc::new(KSpace { words: (0..n).map(|_| AtomicU32::new(0)).collect(), stop: AtomicBool::new(false) })
    }
    pub(super) fn a(i: usize) -> u64 {
        BASE + i as u64 * 4
    }
    fn idx(&self, addr: u64) -> Option<usize> {
        let i = (addr.checked_sub(BASE)? / 4) as usize;
        (i < self.words.len()).then_some(i)
    }
    pub(super) fn w(&self, i: usize) -> &AtomicU32 {
        &self.words[i]
    }
}

impl Space for KSpace {
    fn key(&self, addr: u64, _shared: bool, _write: bool) -> Result<KeyRef, i64> {
        if addr % 4 != 0 {
            return Err(EINVAL);
        }
        self.idx(addr).ok_or(EFAULT)?;
        let object = self as *const KSpace as u64;
        Ok(KeyRef::plain(Key { kind: KeyKind::Private, object, page: addr & !0xfff, offset: (addr & 0xfff) as u32 }))
    }
    fn word(&self, addr: u64, op: WordOp) -> Result<u32, WordError> {
        let i = self.idx(addr).ok_or(WordError::Fault)?;
        Ok(apply(&self.words[i], op))
    }
    fn fault_in(&self, addr: u64, _write: bool) -> Result<(), i64> {
        self.idx(addr).map(|_| ()).ok_or(EFAULT)
    }
    fn read_u32(&self, addr: u64) -> Result<u32, i64> {
        self.idx(addr).map(|i| self.words[i].load(Ordering::SeqCst)).ok_or(EFAULT)
    }
    fn read_u64(&self, _addr: u64) -> Result<u64, i64> {
        Err(EFAULT)
    }
    fn write_u32(&self, addr: u64, v: u32) -> Result<(), i64> {
        self.idx(addr).map(|i| self.words[i].store(v, Ordering::SeqCst)).ok_or(EFAULT)
    }
    fn interrupted(&self) -> Option<i64> {
        self.stop.load(Ordering::Acquire).then_some(EINTR)
    }
    /// Kernel threads have no TIDs: the scheduler id stands in (they are small numbers).
    fn tid(&self) -> Option<u32> {
        Some(crate::current_id() as u32 & super::FUTEX_TID_MASK)
    }
    fn thread_of(&self, tid: u32) -> Option<u64> {
        crate::table::get(tid as u64).map(|t| t.id)
    }
}

type Running = (crate::JoinHandle, Arc<AtomicU64>);

fn spawn_r(name: &str, f: impl FnOnce() -> i64 + Send + 'static) -> Result<Running, String> {
    let r = Arc::new(AtomicU64::new(u64::MAX));
    let r2 = r.clone();
    let h = crate::spawn(name, Class::Normal, move || r2.store(f() as u64, Ordering::Release)).map_err(|e| format!("spawn: {e:?}"))?;
    Ok((h, r))
}

fn result(h: &Running, ns: u64) -> Option<i64> {
    h.0.join_timeout(ns).then(|| h.1.load(Ordering::Acquire) as i64)
}

fn queued(base: usize, n: usize) -> TestResult {
    let end = time::now_ns() + 10 * SEC;
    while super::waiters() < base + n {
        kassert!(time::now_ns() < end, "only {} of {n} waiters queued", super::waiters().saturating_sub(base));
        crate::sleep_ns(MS);
    }
    Ok(())
}

ktest!("sc.futex.core.basics", Safe, basics);
fn basics() -> TestResult {
    let sp = KSpace::new(16);
    let base = super::waiters();
    sp.w(0).store(3, Ordering::SeqCst);
    kassert!(super::wait(&*sp, KSpace::a(0), false, 4, !0, None) == Err(EAGAIN), "mismatch");
    kassert!(super::wait(&*sp, KSpace::a(0) + 2, false, 3, !0, None) == Err(EINVAL), "misaligned");
    kassert!(super::wait(&*sp, KSpace::a(0), false, 3, 0, None) == Err(EINVAL), "empty bitset");
    kassert!(super::wake(&*sp, KSpace::a(0), false, 1, 0) == Err(EINVAL), "wake with an empty bitset");
    kassert!(super::wait(&*sp, KSpace::a(99), false, 0, !0, None) == Err(EFAULT), "outside");
    let s = time::now_ns();
    kassert!(super::wait(&*sp, KSpace::a(0), false, 3, !0, Some(Deadline::Monotonic(s + 15 * MS))) == Err(ETIMEDOUT), "timeout");
    kassert!(time::now_ns() >= s + 15 * MS, "woke before the deadline");
    kassert!(super::wait(&*sp, KSpace::a(0), false, 3, !0, Some(Deadline::Monotonic(0))) == Err(ETIMEDOUT), "deadline in the past");
    // A waiter interrupted (its `Space` says so) after a kick.
    let sp2 = sp.clone();
    let w = spawn_r("sc-fxc-int", move || match super::wait(&*sp2, KSpace::a(1), false, 0, !0, None) {
        Ok(()) => 0,
        Err(e) => -e,
    })?;
    queued(base, 1)?;
    sp.stop.store(true, Ordering::Release);
    crate::kick_thread(w.0.id());
    let got = result(&w, 10 * SEC);
    kassert!(got == Some(-EINTR), "interrupted wait: {got:?}");
    sp.stop.store(false, Ordering::Release);
    kassert!(super::waiters() == base, "waiters left behind");
    Ok(())
}

// 64 threads, a condition variable in the shape of glibc's old one: the broadcaster bumps the sequence word,
// FUTEX_CMP_REQUEUEs every waiter onto the "mutex" word and wakes that. 200 rounds; each thread must see every
// round. A lost wake-up or a requeue that strands a waiter hangs a round (the limit fails it).
ktest!("sc.futex.core.requeue_storm", Safe, requeue_storm);
fn requeue_storm() -> TestResult {
    const T: u64 = 64;
    const ROUNDS: u32 = 200;
    const SEQ: usize = 0;
    const MUTEX: usize = 1;
    let sp = KSpace::new(4);
    let passed = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for i in 0..T {
        let (sp, passed) = (sp.clone(), passed.clone());
        hs.push(spawn_r(&format!("sc-fxc-s{i}"), move || {
            for r in 1..=ROUNDS {
                loop {
                    let s = sp.w(SEQ).load(Ordering::SeqCst);
                    if s >= r {
                        break;
                    }
                    match super::wait(&*sp, KSpace::a(SEQ), false, s, !0, Some(Deadline::Monotonic(time::now_ns() + 30 * SEC))) {
                        Ok(()) | Err(EAGAIN) => {}
                        Err(e) => return -e,
                    }
                }
                passed.fetch_add(1, Ordering::SeqCst);
            }
            0
        })?);
    }
    let end = time::now_ns() + 120 * SEC;
    let mut moved = 0i64;
    for r in 1..=ROUNDS {
        sp.w(SEQ).store(r, Ordering::SeqCst);
        moved += super::requeue(&*sp, KSpace::a(SEQ), KSpace::a(MUTEX), false, 1, i32::MAX, Some(r)).unwrap_or(0);
        loop {
            let _ = super::wake(&*sp, KSpace::a(MUTEX), false, i32::MAX, !0);
            if passed.load(Ordering::SeqCst) >= T * r as u64 {
                break;
            }
            kassert!(time::now_ns() < end, "round {r}: only {} of {T} threads passed", passed.load(Ordering::SeqCst) - T * (r as u64 - 1));
            crate::sleep_ns(MS / 4);
        }
    }
    for h in &hs {
        let got = result(h, 30 * SEC);
        kassert!(got == Some(0), "a thread ended with {got:?}");
    }
    klog::event!(Info, "sc", "futex.requeue_storm", threads = T, rounds = ROUNDS, moved = moved);
    Ok(())
}

// futex_waitv on the core: the highest woken index, a mismatch, a timeout.
ktest!("sc.futex.core.waitv", Safe, waitv);
fn waitv() -> TestResult {
    let sp = KSpace::new(8);
    let base = super::waiters();
    let items: Vec<super::WaitvItem> = (0..4).map(|i| super::WaitvItem { addr: KSpace::a(i), val: 0, shared: false }).collect();
    let (sp2, it2) = (sp.clone(), items.clone());
    let w = spawn_r("sc-fxc-wv", move || super::wait_multiple(&*sp2, &it2, None).unwrap_or_else(|e| -e))?;
    queued(base, 4)?;
    kassert!(super::wake(&*sp, KSpace::a(3), false, 1, !0) == Ok(1), "wake 3");
    let got = result(&w, 10 * SEC);
    kassert!(got == Some(3), "waitv answered {got:?}");
    sp.w(2).store(1, Ordering::SeqCst);
    kassert!(super::wait_multiple(&*sp, &items, None) == Err(EAGAIN), "mismatch");
    kassert!(super::wait_multiple(&*sp, &items[..2], Some(Deadline::Monotonic(time::now_ns() + 5 * MS))) == Err(ETIMEDOUT), "timeout");
    kassert!(super::waiters() == base, "waiters left behind");
    Ok(())
}
