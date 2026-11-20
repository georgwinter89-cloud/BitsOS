//! L7-SYNC ktests of the Linux 7.3.1 futex additions on a real process address space (the helpers of
//! `futex_ktests`): FUTEX_ROBUST_UNLOCK / FUTEX_ROBUST_LIST32 with a real waiter, the robust exit walk waking
//! the waiter of a pending lock that another thread owns without FUTEX_WAITERS, and futex2's calls with a NUMA
//! word. Each part was red on the 6.6 code (ENOSYS, a waiter left asleep, ENOSYS).

use super::{Fx, PRIV, args, done, queued};

/// One futex call in a thread of the process (the word operations need its address space loaded).
fn call_now(t: &Fx, what: &str, a: [u64; 6]) -> Result<i64, alloc::string::String> {
    let h = t.call("sc-fx7-c", a)?;
    h.result(10 * super::SEC).ok_or_else(|| alloc::format!("{what}: still running after 10 s"))
}
use crate::futex::*;
use ktest::{TestResult, kassert, ktest};
use uapi::errno::*;

ktest!("sc.futex.l7.robust_unlock", Safe, robust_unlock);
fn robust_unlock() -> TestResult {
    let t = Fx::new("sc-fx7-ru")?;
    let base = sched::futex::waiters();
    // A locked word with a waiter, the robust list's pending pointer at 0x40.
    let tid = 77u32;
    t.put32(0, tid | sched::futex::FUTEX_WAITERS);
    t.put64(0x40, t.a(0));
    let w = t.call("sc-fx7-w", args(t.a(0), FUTEX_WAIT as u64 | PRIV, (tid | sched::futex::FUTEX_WAITERS) as u64, 0, 0, 0))?;
    queued(base, 1)?;
    let n = call_now(&t, "UNLOCK_WAKE_LIST64", args(t.a(0), (FUTEX_WAKE | FUTEX_ROBUST_UNLOCK) as u64 | PRIV, 1, 0, t.a(0x40), 0))?;
    kassert!(n == 1, "UNLOCK_WAKE_LIST64 woke {n}");
    done(&w, "waiter of the robust unlock", 0)?;
    kassert!(t.get32(0) == 0 && t.get32(0x40) == 0 && t.get32(0x44) == 0, "word {:#x}, pending {:#x}:{:#x}", t.get32(0), t.get32(0x40), t.get32(0x44));
    // LIST32: four bytes only.
    t.put32(0, tid);
    t.put64(0x40, u64::MAX);
    let n = call_now(&t, "UNLOCK_WAKE_LIST32", args(t.a(0), (FUTEX_WAKE | FUTEX_ROBUST_UNLOCK | FUTEX_ROBUST_LIST32) as u64 | PRIV, 1, 0, t.a(0x40), 0))?;
    kassert!(n == 0 && t.get32(0) == 0 && t.get32(0x40) == 0 && t.get32(0x44) == u32::MAX, "LIST32: {n}, {:#x}:{:#x}", t.get32(0x40), t.get32(0x44));
    // The command mask: ROBUST_LIST32 alone is a plain wake, ROBUST_UNLOCK elsewhere ENOSYS.
    kassert!(t.now(args(t.a(0), (FUTEX_WAKE | FUTEX_ROBUST_LIST32) as u64 | PRIV, 1, 0, 0, 0)) == 0, "WAKE|LIST32");
    let r = t.now(args(t.a(0), (FUTEX_REQUEUE | FUTEX_ROBUST_UNLOCK) as u64 | PRIV, 1, 0, t.a(4), 0));
    kassert!(r == -ENOSYS, "REQUEUE|ROBUST_UNLOCK: {r}");
    // A pending pointer that cannot be written: EFAULT after the word was unlocked.
    t.put32(0, tid);
    let r = call_now(&t, "bad pending pointer", args(t.a(0), (FUTEX_WAKE | FUTEX_ROBUST_UNLOCK) as u64 | PRIV, 1, 0, 0x10, 0))?;
    kassert!(r == -EFAULT && t.get32(0) == 0, "bad pending pointer: {r}, word {:#x}", t.get32(0));
    Ok(())
}

ktest!("sc.futex.l7.exit_wakes_pending", Safe, exit_wakes_pending);
fn exit_wakes_pending() -> TestResult {
    let t = Fx::new("sc-fx7-ex")?;
    let base = sched::futex::waiters();
    let (dier, owner) = (4321u32, 999u32);
    // head: an empty list (next = head), futex_offset 0, list_op_pending = the word at 0x100 that `owner` holds
    // without FUTEX_WAITERS: the dying thread was between taking it and setting the bit, or it was woken.
    t.put64(0, t.a(0));
    t.put64(8, 0);
    t.put64(16, t.a(0x100));
    t.put32(0x100, owner);
    let w = t.call("sc-fx7-exw", args(t.a(0x100), FUTEX_WAIT as u64, owner as u64, 0, 0, 0))?;
    queued(base, 1)?;
    let head = t.a(0);
    let o = t.thread("sc-fx7-exo", move |p, _| {
        exit_release(p, dier, head, 0, false);
        0
    })?;
    done(&o, "the dying thread", 0)?;
    done(&w, "waiter of the pending lock (6.6: left asleep)", 0)?;
    kassert!(t.get32(0x100) == owner, "the other owner's word changed: {:#x}", t.get32(0x100));
    Ok(())
}

ktest!("sc.futex.l7.futex2_numa", Safe, futex2_numa);
fn futex2_numa() -> TestResult {
    let t = Fx::new("sc-fx7-f2")?;
    let base = sched::futex::waiters();
    let flags = (FUTEX2_SIZE_U32 | FUTEX2_NUMA | FUTEX2_PRIVATE) as u64;
    t.put32(0, 0);
    t.put32(4, u32::MAX);
    let w = t.thread("sc-fx7-f2w", move |p, mem| do_futex_wait(p, [mem, 0, u32::MAX as u64, flags, 0, 0]))?;
    queued(base, 1)?;
    kassert!(t.get32(4) == 0, "the node word became {:#x}, not this CPU's node 0", t.get32(4));
    let n = do_futex_wake(&t.p, [t.a(0), u32::MAX as u64, 1, flags, 0, 0]);
    kassert!(n == 1, "futex_wake woke {n}");
    done(&w, "futex_wait", 0)?;
    t.put32(4, 3);
    let r = do_futex_wake(&t.p, [t.a(0), u32::MAX as u64, 1, flags, 0, 0]);
    kassert!(r == -EINVAL, "node 3: {r}");
    let r = do_futex_wake(&t.p, [t.a(4), u32::MAX as u64, 1, flags, 0, 0]);
    kassert!(r == -EINVAL, "a NUMA pair at a 4-aligned address: {r}");
    let r = do_futex_wake(&t.p, [t.a(0), u32::MAX as u64, 0, (FUTEX2_SIZE_U32 | FUTEX2_PRIVATE) as u64, 0, 0]);
    kassert!(r == 0, "nr 0 wakes nobody (FLAGS_STRICT): {r}");
    Ok(())
}
