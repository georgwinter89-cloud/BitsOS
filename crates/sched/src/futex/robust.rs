//! Robust futexes and the thread-exit protocol (linux-6.6/kernel/futex/core.c:636 `handle_futex_death`, :756
//! `exit_robust_list`; kernel/fork.c:1616 `mm_release`).
//!
//! At thread exit the kernel walks the thread's robust list (registered with set_robust_list; a userspace list,
//! so every read may fail and the walk then just stops) and, for every lock word whose owner TID is the dying
//! thread, sets FUTEX_OWNER_DIED atomically (keeping FUTEX_WAITERS) and wakes one waiter (shared key, as Linux:
//! glibc's robust mutexes use shared futex operations for that reason). `list_op_pending` (a lock being
//! taken or released when the thread died) is handled last; an unowned pending word only wakes. The walk stops
//! after ROBUST_LIST_LIMIT entries (Linux's bound against circular lists, an input bound, not a count limit).
//! Bit 0 of a list pointer marks a PI futex: its word gets OWNER_DIED but no wake here: the owner's exit hands
//! each PI futex it holds to its top waiter (`pi::owner_exits`, SC-7b), as Linux's `exit_pi_state_list`.
//!
//! CLONE_CHILD_CLEARTID: when the thread's address space has other users, the kernel writes 0 to the
//! registered tid word and wakes one waiter (FUTEX_WAKE, shared): how pthread_join learns the end.
//!
//! Both run in the exiting thread (its address space loaded), before its address space reference goes.

use super::{FUTEX_OWNER_DIED, FUTEX_TID_MASK, FUTEX_WAITERS, Space, wake};
use ksync::user_word::{WordError, WordOp};

/// Linux's ROBUST_LIST_LIMIT.
pub const ROBUST_LIST_LIMIT: u32 = 2048;
/// sizeof(struct robust_list_head) on x86-64 (next, futex_offset, list_op_pending).
pub const HEAD_LEN: u64 = 24;

klog::counter!(DEATHS, "futex.owner_died");

/// One list pointer: the address with bit 0 cleared and whether bit 0 (PI) was set.
fn fetch(sp: &dyn Space, at: u64) -> Result<(u64, bool), ()> {
    let v = sp.read_u64(at).map_err(|_| ())?;
    Ok((v & !1, v & 1 != 0))
}

/// `handle_futex_death`: Err stops the walk.
pub fn handle_death(sp: &dyn Space, uaddr: u64, tid: u32, pi: bool, pending: bool) -> Result<(), ()> {
    if uaddr % 4 != 0 {
        return Err(());
    }
    let (mut faults, mut races) = (0, 0);
    loop {
        let uval = sp.read_u32(uaddr).map_err(|_| ())?;
        let owner = uval & FUTEX_TID_MASK;
        if pending && !pi && owner == 0 {
            let _ = wake(sp, uaddr, true, 1, super::BITSET_MATCH_ANY);
            return Ok(());
        }
        if owner != tid {
            return Ok(());
        }
        let mval = (uval & FUTEX_WAITERS) | FUTEX_OWNER_DIED;
        match sp.word(uaddr, WordOp::CmpXchg { old: uval, new: mval }) {
            // The word changed between the read and the cmpxchg (Linux: `goto retry`). Bounded like the fault
            // retries: a sibling that keeps changing the word must not hold the exiting thread here (B4).
            Ok(cur) if cur != uval => {
                races += 1;
                if races > super::FAULT_RETRY_LIMIT {
                    klog::event!(Warn, "futex", "robust.contended", tid = tid, races = races; "a robust futex word kept changing while its owner died: left without OWNER_DIED, the walk stops");
                    return Err(());
                }
                crate::cond_resched();
                continue;
            }
            Ok(_) => {}
            Err(WordError::NotLoaded) => {
                super::not_loaded();
                return Err(());
            }
            Err(_) => {
                sp.fault_in(uaddr, true).map_err(|_| ())?;
                super::retry_fault(&mut faults).map_err(|_| ())?;
                continue;
            }
        }
        DEATHS.inc();
        if !pi && uval & FUTEX_WAITERS != 0 {
            let _ = wake(sp, uaddr, true, 1, super::BITSET_MATCH_ANY);
        }
        return Ok(());
    }
}

/// `exit_robust_list` for the list at `head` of the dying thread `tid` (0: no list).
pub fn exit_robust_list(sp: &dyn Space, head: u64, tid: u32) {
    if head == 0 {
        return;
    }
    let Ok((mut entry, mut pi)) = fetch(sp, head) else { return };
    let Ok(off) = sp.read_u64(head + 8) else { return };
    let Ok((pending, pip)) = fetch(sp, head + 16) else { return };
    let mut limit = ROBUST_LIST_LIMIT;
    let mut walked = 0u32;
    while entry != head {
        let next = fetch(sp, entry);
        if entry != pending && handle_death(sp, entry.wrapping_add(off), tid, pi, false).is_err() {
            return;
        }
        let Ok((n, npi)) = next else { return };
        entry = n;
        pi = npi;
        walked += 1;
        limit -= 1;
        if limit == 0 {
            klog::event!(Info, "futex", "robust.limit", tid = tid, entries = walked; "robust list longer than ROBUST_LIST_LIMIT (or circular): the walk stops as Linux's does");
            break;
        }
        crate::cond_resched();
    }
    if pending != 0 {
        let _ = handle_death(sp, pending.wrapping_add(off), tid, pip, true);
    }
}

/// `mm_release`'s CLONE_CHILD_CLEARTID part: `put_user(0)` (its error ignored, as Linux) and a shared FUTEX_WAKE
/// of one waiter. `addr` 0: nothing registered.
pub fn clear_child_tid(sp: &dyn Space, addr: u64) {
    if addr == 0 {
        return;
    }
    let _ = sp.write_u32(addr, 0);
    let _ = wake(sp, addr, true, 1, super::BITSET_MATCH_ANY);
}

/// Everything the futex code does when a thread ends (Linux `exit_mm_release`): the robust list, then the tid
/// word when other threads share the address space (`mm_users > 1`).
pub fn thread_exit(sp: &dyn Space, tid: u32, robust_head: u64, clear_tid: u64, mm_shared: bool) {
    exit_robust_list(sp, robust_head, tid);
    if mm_shared {
        clear_child_tid(sp, clear_tid);
    }
}
