//! L7-PROC ktests (Safe, rerunnable): Linux 7.3.1's thread pidfds (CLONE_PIDFD with CLONE_THREAD, pidfd_open
//! with PIDFD_THREAD), the pidfs inode numbers, PIDFD_GET_INFO, the pidfd_send_signal scopes and pseudo fds, and
//! clone3's CLONE_AUTOREAP, CLONE_NNP and CLONE_PIDFD_AUTOKILL. Held processes whose syscalls the test thread
//! plays (`sx2_ktests::T`); clone children run a sleeping loop.

use crate::sx2_ktests::{T, expect};
use ktest::{TestResult, kassert, ktest};
use uapi::ab::clone::*;
use uapi::ab::nr_more::{PIDFD_OPEN, PIDFD_SEND_SIGNAL};
use uapi::errno::*;
use uapi::nr;

const SEC: u64 = 1_000_000_000;
const THREAD: u64 = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD;
const F_GETFL: u64 = 3;
const O_RDWR: i64 = 2;
const O_EXCL: i64 = 0o200;
const PIDFD_THREAD: u64 = 0o200;
/// `_IOWR(0xFF, 11, struct pidfd_info)` with the 88-byte struct.
const PIDFD_GET_INFO: u64 = 0xc058_ff0b;

fn until(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = time::now_ns() + secs * SEC;
    while time::now_ns() < end {
        if f() {
            return true;
        }
        sched::sleep_ms(5);
    }
    f()
}

fn w32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn ino(t: &mut T, fd: i64, at: u64) -> u64 {
    let r = t.call(nr::FSTAT, &[fd as u64, t.a(at)]);
    if r != 0 {
        return 0;
    }
    u64::from_le_bytes(t.get(at + 8, 8).try_into().unwrap_or([0; 8]))
}

ktest!("ab.l7.pidfd_thread", Safe, pidfd_thread);
/// linux-7.3.1/kernel/fork.c:2356 and kernel/pid.c:697: a thread pidfd shows PIDFD_THREAD (O_EXCL) in F_GETFL, its
/// own pidfs inode, and its thread's numbers in PIDFD_GET_INFO; a non-leader thread without PIDFD_THREAD is ENOENT.
fn pidfd_thread() -> TestResult {
    let mut t = T::new("l7-pidfd-thread")?;
    t.put(0x300, &[0xff; 4]);
    let pidfd_at = t.a(0x300);
    let tid = t.clone3_with(THREAD | CLONE_PIDFD, 0, pidfd_at, true);
    kassert!(tid > 0, "clone3(CLONE_THREAD | CLONE_PIDFD) = {tid}");
    let tfd = w32(&t.get(0x300, 4), 0) as i64;
    expect("F_GETFL of the thread pidfd", t.call(nr::FCNTL, &[tfd as u64, F_GETFL]), O_RDWR | O_EXCL)?;
    expect("pidfd_open(thread, 0)", t.call(PIDFD_OPEN, &[tid as u64, 0]), -ENOENT)?;
    let t2 = t.call(PIDFD_OPEN, &[tid as u64, PIDFD_THREAD]);
    let p1 = t.call(PIDFD_OPEN, &[t.p.pid as u64, 0]);
    let p2 = t.call(PIDFD_OPEN, &[t.p.pid as u64, 0]);
    let lt = t.call(PIDFD_OPEN, &[t.p.pid as u64, PIDFD_THREAD]);
    kassert!(t2 >= 0 && p1 >= 0 && p2 >= 0 && lt >= 0, "pidfds {t2} {p1} {p2} {lt}");
    let (it, it2, ip1, ip2, ilt) = (ino(&mut t, tfd, 0x400), ino(&mut t, t2, 0x400), ino(&mut t, p1, 0x400), ino(&mut t, p2, 0x400), ino(&mut t, lt, 0x400));
    kassert!(it != 0 && it == it2 && ip1 == ip2 && ip1 == ilt && it != ip1, "inodes: thread {it} {it2}, process {ip1} {ip2}, leader thread {ilt}");
    expect("F_GETFL of a process pidfd", t.call(nr::FCNTL, &[p1 as u64, F_GETFL]), O_RDWR)?;
    // PIDFD_GET_INFO: the thread's pid, its process as tgid, credentials and pid always.
    let mut info = [0u8; 88];
    info[..8].copy_from_slice(&0x23u64.to_le_bytes());
    t.put(0x500, &info);
    expect("PIDFD_GET_INFO(thread)", t.call(nr::IOCTL, &[tfd as u64, PIDFD_GET_INFO, t.a(0x500)]), 0)?;
    let got = t.get(0x500, 88);
    let mask = u64::from_le_bytes(got[..8].try_into().unwrap_or([0; 8]));
    kassert!(mask == 0x23, "mask {mask:#x} (PID | CREDS | SUPPORTED_MASK)");
    let euid = t.p.cred().euid;
    kassert!(w32(&got, 16) == tid as u32 && w32(&got, 20) == t.p.pid && w32(&got, 36) == euid, "pid {} tgid {} euid {}", w32(&got, 16), w32(&got, 20), w32(&got, 36));
    kassert!(u64::from_le_bytes(got[80..88].try_into().unwrap_or([0; 8])) == 0xff, "supported mask");
    // The scopes: a thread pidfd signals its thread; two scopes at once are EINVAL; the pseudo fds name the caller.
    expect("send(thread pidfd, 0)", t.call(PIDFD_SEND_SIGNAL, &[tfd as u64, 0, 0, 0]), 0)?;
    expect("send(flags 3)", t.call(PIDFD_SEND_SIGNAL, &[p1 as u64, 0, 0, 3]), -EINVAL)?;
    expect("send(PIDFD_SELF_THREAD)", t.call(PIDFD_SEND_SIGNAL, &[(-10000i64) as u64, 0, 0, 0]), 0)?;
    expect("send(PIDFD_SELF_THREAD_GROUP)", t.call(PIDFD_SEND_SIGNAL, &[(-10001i64) as u64, 0, 0, 0]), 0)?;
    expect("poll of a live thread's pidfd", proc_events(&t, tfd), 0)?;
    Ok(())
}

/// The pidfd's poll answer as `File::events` gives it.
fn proc_events(t: &T, fd: i64) -> i64 {
    t.p.fds.get(fd as usize).map_or(-1, |f| f.file.events().0 as i64)
}

ktest!("ab.l7.autoreap", Safe, autoreap);
/// clone3 CLONE_AUTOREAP (linux-7.3.1/kernel/fork.c:2089, kernel/signal.c:2327): no exit signal allowed, no
/// CLONE_PARENT; the child is reaped at its end without SIGCHLD, so wait4 finds no child and its pidfd reads HUP.
/// CLONE_NNP gives the child no_new_privs; CLONE_PIDFD_AUTOKILL needs CLONE_PIDFD and CLONE_AUTOREAP and kills the
/// child when the pidfd is closed.
fn autoreap() -> TestResult {
    let mut t = T::new("l7-autoreap")?;
    expect("AUTOREAP with an exit signal", t.clone3_with(CLONE_AUTOREAP, 17, 0, true), -EINVAL)?;
    expect("AUTOREAP | PARENT", t.clone3_with(CLONE_AUTOREAP | CLONE_PARENT, 0, 0, true), -EINVAL)?;
    expect("NNP | THREAD", t.clone3_with(THREAD | CLONE_NNP, 0, 0, true), -EINVAL)?;
    expect("AUTOKILL without PIDFD", t.clone3_with(CLONE_PIDFD_AUTOKILL | CLONE_AUTOREAP, 0, 0, true), -EINVAL)?;
    let at = t.a(0x300);
    expect("AUTOKILL without AUTOREAP", t.clone3_with(CLONE_PIDFD_AUTOKILL | CLONE_PIDFD, 17, at, true), -EINVAL)?;
    expect("AUTOKILL without NNP, unprivileged", t.clone3_with(CLONE_PIDFD_AUTOKILL | CLONE_PIDFD | CLONE_AUTOREAP, 0, at, true), -EPERM)?;
    // AUTOREAP | NNP: a sleeping child; no_new_privs is set on it.
    let kid = t.clone3_with(CLONE_AUTOREAP | CLONE_NNP | CLONE_PIDFD, 0, at, true);
    kassert!(kid > 0, "clone3(AUTOREAP | NNP | PIDFD) = {kid}");
    let pfd = w32(&t.get(0x300, 4), 0) as i64;
    let th = proc::find_thread(kid as u32).ok_or("no child thread")?;
    kassert!(th.prctl().no_new_privs(), "CLONE_NNP did not set no_new_privs");
    drop(th);
    expect("kill(child, SIGKILL)", t.call(nr::KILL, &[kid as u64, 9]), 0)?;
    kassert!(until(10, || proc::find(kid as u32).is_none()), "the autoreap child stayed a zombie");
    expect("wait4 after the autoreap child ended", t.call(nr::WAIT4, &[u64::MAX, 0, 1, 0]), -ECHILD)?;
    expect("pidfd of the reaped child", proc_events(&t, pfd), 0x11 | 0x40)?;
    // AUTOKILL with NNP: closing the pidfd kills the child.
    let kid = t.clone3_with(CLONE_AUTOREAP | CLONE_NNP | CLONE_PIDFD | CLONE_PIDFD_AUTOKILL, 0, at, true);
    kassert!(kid > 0, "clone3(AUTOKILL) = {kid}");
    let kfd = w32(&t.get(0x300, 4), 0) as i64;
    expect("F_GETFL of an autokill pidfd", t.call(nr::FCNTL, &[kfd as u64, F_GETFL]), O_RDWR | 0o1000)?;
    expect("close(autokill pidfd)", t.call(nr::CLOSE, &[kfd as u64]), 0)?;
    kassert!(until(10, || proc::find(kid as u32).is_none()), "closing the pidfd did not end the child");
    Ok(())
}

ktest!("ab.l7.thread_exit_code", Safe, thread_exit_code);
/// linux-7.3.1/fs/pidfs.c:788 (`pidfs_exit` keeps the task's own `exit_code` at its release): a thread that
/// ended by exit(5) reports 0x500 through its pidfd, also after the process ended by its leader's exit(3).
fn thread_exit_code() -> TestResult {
    let mut t = T::new("l7-thread-exit")?;
    t.put(0x300, &[0xff; 4]);
    let at = t.a(0x300);
    let tid = t.clone3_with(THREAD | CLONE_PIDFD, 0, at, true);
    kassert!(tid > 0, "clone3(CLONE_THREAD | CLONE_PIDFD) = {tid}");
    let tfd = w32(&t.get(0x300, 4), 0) as usize;
    let f = t.p.fds.get(tfd).map_err(|e| alloc::format!("the thread pidfd: {e:?}"))?;
    let target = proc::pidfd::target_of(&f).ok_or("not a pidfd")?;
    // Both threads end alone as soon as the held process goes: the thread with 5, the leader with 3.
    proc::find_thread(tid as u32).ok_or("no thread")?.exit_thread(5);
    proc::find_thread(t.p.pid).ok_or("no leader thread")?.exit_thread(3);
    t.p.group().release();
    kassert!(t.p.wait(Some(10 * SEC)).is_some(), "the process did not end");
    kassert!(until(10, || proc::find_thread(tid as u32).is_none()), "the thread did not end");
    let group = t.p.group().status().map(|s| s.wait_status());
    kassert!(group == Some(0x300), "the process's wait status {group:?}");
    kassert!(target.exit_code() == Some(0x500), "the thread pidfd's exit code {:?}", target.exit_code());
    Ok(())
}

ktest!("ab.l7.parent_tid_after_pidfd", Safe, parent_tid_after_pidfd);
/// linux-7.3.1/kernel/fork.c:2366 and :2799: the pidfd's number is written inside `copy_process`, parent_tid
/// only after it succeeded, so a clone that fails on the pidfd pointer leaves parent_tid alone (thread and process).
fn parent_tid_after_pidfd() -> TestResult {
    let mut t = T::new("l7-settid")?;
    let ptid = t.a(0x300);
    for (what, flags, sig) in [("thread", THREAD, 0), ("process", 0, 17)] {
        t.put(0x300, &0x5a5a_5a5au32.to_le_bytes());
        let r = t.clone3_args(flags | CLONE_PIDFD | CLONE_PARENT_SETTID, sig, 0x10, ptid, true);
        expect(&alloc::format!("clone3({what}, pidfd at an unmapped address)"), r, -EFAULT)?;
        let v = w32(&t.get(0x300, 4), 0);
        kassert!(v == 0x5a5a_5a5a, "{what}: a failed clone wrote parent_tid ({v:#x})");
    }
    Ok(())
}

ktest!("ab.l7.zombie_leader_signal", Safe, zombie_leader_signal);
/// linux-7.3.1/kernel/signal.c:1458 `group_send_sig_info`: PIDFD_SIGNAL_THREAD towards a leader that ended before
/// its other thread still checks the sender's permission on that zombie thread: EPERM for another uid, 0 for
/// the same uid (the signal is then dropped).
fn zombie_leader_signal() -> TestResult {
    use uapi::ab::pidfd::PIDFD_SIGNAL_THREAD;
    let mut t = T::new("l7-zombie-leader")?;
    let tid = t.clone3_with(THREAD, 0, 0, true);
    kassert!(tid > 0, "clone3(CLONE_THREAD) = {tid}");
    let pid = t.p.pid;
    let leader = proc::find_thread(pid).ok_or("no leader thread")?;
    leader.exit_thread(0);
    drop(leader);
    t.p.group().release();
    kassert!(until(10, || proc::find_thread(pid).is_none()), "the leader thread did not end");
    kassert!(!t.p.has_exited(), "the process ended with its leader");
    let mut other = crate::ab6_ktests::T::with("l7-zombie-other", 2000, false)?;
    let mut same = crate::ab6_ktests::T::new("l7-zombie-same")?;
    let ofd = other.call(PIDFD_OPEN, &[pid as u64, 0]);
    let sfd = same.call(PIDFD_OPEN, &[pid as u64, 0]);
    kassert!(ofd >= 0 && sfd >= 0, "pidfd_open of the process: {ofd} {sfd}");
    let scope = PIDFD_SIGNAL_THREAD as u64;
    expect("SIGNAL_THREAD to the zombie leader from uid 2000", other.call(PIDFD_SEND_SIGNAL, &[ofd as u64, 10, 0, scope]), -EPERM)?;
    expect("SIGNAL_THREAD to the zombie leader from its uid", same.call(PIDFD_SEND_SIGNAL, &[sfd as u64, 0, 0, scope]), 0)?;
    expect("the process scope from uid 2000", other.call(PIDFD_SEND_SIGNAL, &[ofd as u64, 0, 0, 0]), -EPERM)?;
    kassert!(!t.p.has_exited(), "a refused signal ended the process");
    Ok(())
}
