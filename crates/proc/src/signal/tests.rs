//! Host tests of the pure signal state (`Signals`): masks, sending and the round robin, default actions,
//! queueing and RLIMIT_SIGPENDING, synchronous-first dequeue, SA_RESETHAND/SA_NODEFER, job control and
//! siginfo layouts. No kernel needed.

use super::*;
use uapi::ab::siginfo::*;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64};

const SIGUSR1: u32 = 10;
const SIGUSR2: u32 = 12;

/// A group with `n` threads keyed 1..=n, kthread ids 1000+key, masks 0.
fn group(n: u64) -> Signals {
    let mut s = Signals::default();
    for k in 1..=n {
        s.add_thread(k, k as u32, 0, Arc::new(AtomicBool::new(false)), Arc::new(AtomicU64::new(1000 + k)));
    }
    s
}

fn flag(s: &Signals, key: u64) -> bool {
    s.thread(key).map(|t| t.flag_is_set()).unwrap_or(false)
}

#[test]
fn mask_rules_and_view() {
    let mut s = group(1);
    assert_eq!(s.sigprocmask(SIG_BLOCK, Some(u64::MAX)), Ok(0));
    assert_eq!(s.blocked_of(1), !UNBLOCKABLE, "SIGKILL/SIGSTOP never blocked");
    assert_eq!(s.sigprocmask(SIG_UNBLOCK, Some(bit(SIGUSR1))), Ok(!UNBLOCKABLE));
    assert_eq!(s.blocked_of(1) & bit(SIGUSR1), 0);
    assert_eq!(s.sigprocmask(7, Some(0)), Err(EINVAL));
    assert_eq!(s.sigprocmask(7, None), Ok(s.blocked_of(1)), "how is not looked at without a set");
}

#[test]
fn sigaction_flag_and_mask_cleanup() {
    let mut s = group(1);
    let h = SigAction { handler: 0x4000, flags: SA_RESTORER | SA_SIGINFO | (1 << 40), restorer: 0x1234, mask: u64::MAX };
    assert_eq!(s.sigaction(9, Some(h)), Err(EINVAL), "SIGKILL cannot change");
    assert_eq!(s.sigaction(SIGUSR1, Some(h)), Ok(SigAction::default()));
    let o = s.sigaction(SIGUSR1, None).unwrap();
    assert_eq!(o.flags, SA_RESTORER | SA_SIGINFO, "unknown flag bits cleared");
    assert_eq!(o.mask, !UNBLOCKABLE);
    s.send(SigInfo::kernel(SIGUSR2), To::Group, false, u64::MAX).unwrap();
    assert!(s.shared.has(SIGUSR2));
    s.sigaction(SIGUSR2, Some(SigAction { handler: SIG_IGN, ..Default::default() })).unwrap();
    assert!(!s.shared.has(SIGUSR2), "SIG_IGN flushed the pending signal");
}

#[test]
fn default_actions_and_fatal() {
    let mut s = group(1);
    assert_eq!(s.send(SigInfo::kernel(17), To::Group, false, u64::MAX).unwrap().sent, Sent::Dropped);
    assert_eq!(s.send(SigInfo::kernel(15), To::Group, false, u64::MAX).unwrap().sent, Sent::Fatal);
    assert_eq!(s.send(SigInfo::kernel(9), To::Group, false, u64::MAX).unwrap().sent, Sent::Fatal);
    let mut s = group(1);
    let out = s.send(SigInfo::kernel(SIGSEGV), To::Group, false, u64::MAX).unwrap();
    assert_eq!(out.sent, Sent::Pending, "a core signal is taken by the thread, not ended by the sender");
    assert!(s.shared.has(SIGSEGV));
    assert_eq!(s.next_for(1), Some(Next::Fatal(SigInfo::kernel(SIGSEGV))));
}

#[test]
fn blocked_signal_waits_then_fatal_on_unblock() {
    let mut s = group(1);
    s.sigprocmask(SIG_BLOCK, Some(bit(15))).unwrap();
    assert_eq!(s.send(SigInfo::kernel(15), To::Group, false, u64::MAX).unwrap().sent, Sent::Pending);
    assert_eq!(s.take_fatal(), None, "still blocked");
    s.sigprocmask(SIG_UNBLOCK, Some(bit(15))).unwrap();
    assert_eq!(s.take_fatal(), Some(15));
}

#[test]
fn round_robin_and_blocking_threads() {
    let mut s = group(3);
    // A real-time signal (queues each send) so the round robin shows across two sends.
    s.set_blocked(1, bit(SIGRTMIN));
    let a = SigAction { handler: 0x1000, flags: SA_RESTORER, restorer: 1, mask: 0 };
    s.sigaction(SIGRTMIN, Some(a)).unwrap();
    let out = s.send(SigInfo::user(SIGRTMIN, SI_QUEUE, 1, 1), To::Group, false, u64::MAX).unwrap();
    assert_eq!(out.sent, Sent::Pending);
    assert_eq!(out.wake.0, alloc::vec![1002], "woke thread 2's kthread");
    assert!(!flag(&s, 1) && flag(&s, 2));
    let out = s.send(SigInfo::user(SIGRTMIN, SI_QUEUE, 1, 2), To::Group, false, u64::MAX).unwrap();
    assert_eq!(out.wake.0, alloc::vec![1003]);
}

#[test]
fn thread_directed_and_retarget_on_exit() {
    let mut s = group(2);
    let a = SigAction { handler: 0x1000, flags: SA_RESTORER, restorer: 1, mask: 0 };
    s.sigaction(SIGUSR1, Some(a)).unwrap();
    s.send(SigInfo::kernel(SIGUSR1), To::Group, false, u64::MAX).unwrap();
    assert!(flag(&s, 1) && !flag(&s, 2));
    let (wake, _) = s.remove_thread(1);
    assert!(s.shared.has(SIGUSR1), "the shared signal stayed");
    assert!(flag(&s, 2), "thread 2 now carries it");
    assert_eq!(wake.0, alloc::vec![1002]);
}

#[test]
fn queueing_and_rlimit() {
    let mut s = group(1);
    s.sigprocmask(SIG_BLOCK, Some(bit(SIGRTMIN) | bit(SIGUSR1))).unwrap();
    for _ in 0..3 {
        s.send(SigInfo::user(SIGUSR1, SI_QUEUE, 1, 0), To::Group, false, u64::MAX).unwrap();
    }
    assert_eq!(s.queued(), 1, "a standard signal coalesces");
    for v in 0..2 {
        assert_eq!(s.send(SigInfo::user(SIGRTMIN, SI_QUEUE, 1, v), To::Group, false, 3).unwrap().sent, Sent::Pending);
    }
    assert_eq!(s.queued(), 3);
    assert_eq!(s.send(SigInfo::user(SIGRTMIN, SI_QUEUE, 1, 9), To::Group, false, 3).err(), Some(EAGAIN));
    assert_eq!(s.send(SigInfo::user(SIGRTMIN, SI_USER, 1, 0), To::Group, false, 3).unwrap().sent, Sent::Pending);
    assert!(s.shared.has(SIGRTMIN));
}

#[test]
fn synchronous_signals_dequeue_first() {
    let mut s = group(1);
    let a = SigAction { handler: 0x1000, flags: SA_RESTORER, restorer: 1, mask: 0 };
    for sig in [SIGUSR1, SIGSEGV, SIGUSR2] {
        s.sigaction(sig, Some(a)).unwrap();
    }
    s.send(SigInfo::user(SIGUSR1, SI_USER, 1, 0), To::Thread(1), false, u64::MAX).unwrap();
    s.send(SigInfo::fault(SIGSEGV, SEGV_MAPERR, 0x1234), To::Thread(1), false, u64::MAX).unwrap();
    s.send(SigInfo::user(SIGUSR2, SI_USER, 1, 0), To::Thread(1), false, u64::MAX).unwrap();
    assert_eq!(s.next_for(1), Some(Next::Handler(SigInfo::fault(SIGSEGV, SEGV_MAPERR, 0x1234), a)));
    assert!(matches!(s.next_for(1), Some(Next::Handler(i, _)) if i.signo() == SIGUSR1), "then the lowest number");
}

#[test]
fn resethand_and_nodefer_and_delivered_mask() {
    let mut s = group(1);
    let a = SigAction { handler: 0x1000, flags: SA_RESTORER | SA_RESETHAND, restorer: 1, mask: bit(SIGUSR2) };
    s.sigaction(SIGUSR1, Some(a)).unwrap();
    s.send(SigInfo::kernel(SIGUSR1), To::Thread(1), false, u64::MAX).unwrap();
    assert!(matches!(s.next_for(1), Some(Next::Handler(..))));
    assert_eq!(s.action(SIGUSR1).handler, SIG_DFL, "SA_RESETHAND put it back to default");
    let w = s.delivered(1, SIGUSR1, &a);
    assert!(w.is_empty());
    assert_eq!(s.blocked_of(1), bit(SIGUSR1) | bit(SIGUSR2));
    let mut s = group(1);
    let a = SigAction { handler: 0x1000, flags: SA_RESTORER | SA_NODEFER, restorer: 1, mask: 0 };
    s.delivered(1, SIGUSR1, &a);
    assert_eq!(s.blocked_of(1), 0);
}

#[test]
fn saved_mask_round_trip() {
    let mut s = group(1);
    let (old, _) = s.set_temporary(1, bit(SIGUSR1)).unwrap();
    assert_eq!(old, 0);
    assert_eq!(s.blocked_of(1), bit(SIGUSR1));
    assert_eq!(s.mask_to_save(1), 0, "the frame carries the saved (old) mask");
    s.restore_saved(1);
    assert_eq!(s.blocked_of(1), 0);
    s.set_temporary(1, bit(SIGUSR1)).unwrap();
    s.set_saved(1, 0);
    let a = SigAction { handler: 1, flags: SA_RESTORER, restorer: 1, mask: 0 };
    s.delivered(1, SIGUSR1, &a);
    assert_eq!(s.saved_of(1), None, "delivered cleared the saved mask");
}

#[test]
fn unkillable_init() {
    let mut s = group(1);
    s.unkillable = true;
    assert_eq!(s.send(SigInfo::user(15, SI_USER, 2, 1000), To::Group, false, u64::MAX).unwrap().sent, Sent::Dropped);
    // A SIGKILL from a program (force=false) is dropped too; only the kernel's forced one reaches it.
    assert_eq!(s.send(SigInfo::user(9, SI_USER, 2, 1000), To::Group, false, u64::MAX).unwrap().sent, Sent::Dropped);
    assert_eq!(s.send(SigInfo::kernel(9), To::Group, true, u64::MAX).unwrap().sent, Sent::Fatal, "a forced SIGKILL (kernel) is not ignored");
    let a = SigAction { handler: 0x1000, flags: SA_RESTORER, restorer: 1, mask: 0 };
    s.sigaction(15, Some(a)).unwrap();
    assert_eq!(s.send(SigInfo::kernel(15), To::Group, false, u64::MAX).unwrap().sent, Sent::Pending);
}

#[test]
fn group_stop_and_continue() {
    let mut s = group(2);
    assert_eq!(s.next_for(1), None);
    s.send(SigInfo::kernel(SIGTSTP), To::Group, false, u64::MAX).unwrap();
    assert!(matches!(s.next_for(1), Some(Next::Stop(_))));
    let w = s.begin_group_stop(1, SIGTSTP);
    assert_eq!(w.0, alloc::vec![1002], "the other thread is told to stop too");
    assert_eq!(s.participate(1), Some(false), "not the last to stop");
    assert_eq!(s.participate(2), Some(true), "last: the parent gets CLD_STOPPED");
    assert!(s.group_stopped());
    let out = s.send(SigInfo::kernel(SIGCONT), To::Group, false, u64::MAX).unwrap();
    assert!(out.continued);
    assert!(!s.group_stopped() && !s.is_stopped(1) && !s.is_stopped(2));
    assert_eq!(s.take_cld(), Some(Cld::Continued));
    s.send(SigInfo::kernel(SIGTSTP), To::Group, false, u64::MAX).unwrap();
    assert!(s.shared.has(SIGTSTP));
    s.send(SigInfo::kernel(SIGCONT), To::Group, false, u64::MAX).unwrap();
    assert!(!s.shared.has(SIGTSTP), "SIGCONT flushed the pending stop");
}

#[test]
fn siginfo_from_user_rejects_unknown_layout_with_tail() {
    let mut b = [0u8; 128];
    b[8..12].copy_from_slice(&(SI_QUEUE as u32).to_le_bytes());
    b[64] = 1;
    assert!(SigInfo::from_user(SIGUSR1, &b).is_ok());
    let mut b = [0u8; 128];
    b[8..12].copy_from_slice(&0x1234u32.to_le_bytes());
    b[64] = 1;
    assert_eq!(SigInfo::from_user(SIGUSR1, &b), Err(E2BIG));
}

#[test]
fn signalfd_record_layouts() {
    let i = SigInfo::chld(17, CLD_EXITED, 42, 1000, 3, 7, 9);
    let r = signalfd_record(&i);
    assert_eq!(u32::from_le_bytes(r[0..4].try_into().unwrap()), 17);
    assert_eq!(u32::from_le_bytes(r[12..16].try_into().unwrap()), 42, "ssi_pid");
    assert_eq!(u32::from_le_bytes(r[16..20].try_into().unwrap()), 1000, "ssi_uid");
    assert_eq!(u32::from_le_bytes(r[40..44].try_into().unwrap()), 3, "ssi_status");
    let i = SigInfo::fault(SIGSEGV, SEGV_MAPERR, 0xdead_beef);
    let r = signalfd_record(&i);
    assert_eq!(u64::from_le_bytes(r[72..80].try_into().unwrap()), 0xdead_beef, "ssi_addr");
}

#[test]
fn altstack_on_rules() {
    let mut a = AltStack::DISABLED;
    assert_eq!(a.ss_flags(0x7fff_0000), SS_DISABLE);
    a.change(Some(AltStack { sp: 0x10000, size: 8192, flags: 0 }), 0).unwrap();
    assert_eq!(a.ss_flags(0x10000), 0, "the base itself is not on the stack");
    assert_eq!(a.ss_flags(0x10004), SS_ONSTACK);
    assert_eq!(a.ss_flags(0x12000), SS_ONSTACK, "the top is on it");
    a.change(Some(AltStack { sp: 0x10000, size: 8192, flags: SS_AUTODISARM }), 0).unwrap();
    assert!(!a.on(0x10004));
    assert_eq!(a.ss_flags(0x10004), 0);
    let mut a = AltStack { sp: 0x10000, size: 8192, flags: 0 };
    assert_eq!(a.change(Some(AltStack::DISABLED), 0x10004), Err(EPERM));
}

#[test]
fn exit_signals_not_delivered() {
    let mut s = group(1);
    s.begin_exit();
    assert!(s.exiting);
    assert_eq!(s.send(SigInfo::kernel(SIGUSR1), To::Group, false, u64::MAX).unwrap().sent, Sent::Dropped);
    assert!(flag(&s, 1), "a dying thread always has its flag set");
}
