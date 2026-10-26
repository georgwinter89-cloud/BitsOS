//! AB-12 ktests (Safe, rerunnable): pidfds. Each test plays held processes (`ab6_ktests::T`) and makes real
//! children (a sleeper spawned with the held parent, or a clone of the held parent that runs exit_group(0)).

use crate::ab6_ktests::T;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use proc::{ExitStatus, Process};
use uapi::ab::nr_more::{PIDFD_GETFD, PIDFD_OPEN, PIDFD_SEND_SIGNAL, WAITID};
use uapi::errno::*;
use uapi::nr;

const SEC: u64 = 1_000_000_000;
const KERNEL_ADDR: u64 = 0xffff_8880_0000_0000;
/// Above every pid_max: no process has it.
const NO_PID: u64 = 0x3fff_fff0;
const O_RDWR: i64 = 2;
const O_NONBLOCK: u64 = 0o4000;
const F_GETFD: u64 = 1;
const F_GETFL: u64 = 3;
const P_PID: u64 = 1;
const P_PIDFD: u64 = 3;
const WNOHANG: u64 = 1;
const WEXITED: u64 = 4;
const CLONE_PIDFD: u64 = 0x1000;
const CLONE_THREAD: u64 = 0x10000;
const CLONE_PARENT_SETTID: u64 = 0x10_0000;
const CLONE_PARENT: u64 = 0x8000;
const SIGCHLD: u64 = 17;
const POLLIN: u16 = 1;
// nanosleep({100, 0}); jmp .
const SLEEPER: [u8; 18] = [0x6a, 0x00, 0x6a, 0x64, 0x48, 0x89, 0xe7, 0x31, 0xf6, 0xb8, 0x23, 0, 0, 0, 0x0f, 0x05, 0xeb, 0xfe];

/// A sleeping child (killed at the end of the test if still there).
struct Child(Arc<Process>);
impl Drop for Child {
    fn drop(&mut self) {
        if !self.0.has_exited() {
            self.0.kill(9);
        }
        let _ = self.0.wait(Some(5 * SEC));
    }
}

fn sleeper(name: &str, parent: &Arc<Process>) -> Result<Child, String> {
    let image = crate::ktests::elf(&SLEEPER);
    let argv = [name];
    let mut o = proc::SpawnOptions::new(&argv, &[], policy::ProgramId(1));
    o.parent = Some(parent);
    o.uid = parent.cred().uid;
    proc::spawn_image_with(name, &image, &o).map(Child).map_err(|e| format!("spawn {name}: {e}"))
}

fn until(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = time::now_ns() + secs * SEC;
    while time::now_ns() < end {
        if f() {
            return true;
        }
        sched::sleep_ns(5_000_000);
    }
    f()
}

fn w32(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// poll(fd, POLLIN, 0): the revents.
fn revents(t: &mut T, fd: i64) -> u16 {
    let mut pfd = [0u8; 8];
    pfd[..4].copy_from_slice(&(fd as i32).to_le_bytes());
    pfd[4..6].copy_from_slice(&POLLIN.to_le_bytes());
    t.put(3000, &pfd);
    if t.call(nr::POLL, &[t.a(3000), 1, 0]) < 0 {
        return 0xffff;
    }
    let b = t.get(3006, 2);
    u16::from_le_bytes([b[0], b[1]])
}

/// `/proc/<pid>/fdinfo/<fd>` of the held process, read through its own syscalls.
fn fdinfo(t: &mut T, fd: i64) -> String {
    let path = format!("/proc/{}/fdinfo/{fd}\0", t.p.pid);
    t.put(3200, path.as_bytes());
    let f = t.call(nr::OPENAT, &[(-100i64) as u64, t.a(3200), 0, 0]);
    if f < 0 {
        return format!("open: {f}");
    }
    let n = t.call(nr::READ, &[f as u64, t.a(3400), 512]);
    t.call(nr::CLOSE, &[f as u64]);
    if n < 0 {
        return format!("read: {n}");
    }
    String::from_utf8_lossy(&t.get(3400, n as usize)).into_owned()
}

/// waitid(which, id, &si, options, NULL): the result and the siginfo (filled with 0xaa before).
fn waitid(t: &mut T, which: u64, id: i64, options: u64) -> (i64, Vec<u8>) {
    t.put(3600, &[0xaa; 128]);
    let r = t.call(WAITID, &[which, id as u64, t.a(3600), options, 0]);
    (r, t.get(3600, 128))
}

fn close(t: &mut T, fds: &[i64]) {
    for &fd in fds {
        t.call(nr::CLOSE, &[fd as u64]);
    }
}

ktest!("ab.pidfd.open", Safe, open);
fn open() -> TestResult {
    let mut t = T::new("ab12-open")?;
    let c = sleeper("ab12-open-child", &t.p)?;
    let cpid = c.0.pid as u64;
    kassert!(t.call(PIDFD_OPEN, &[cpid, 1]) == -EINVAL, "unknown flag");
    kassert!(t.call(PIDFD_OPEN, &[0, 0]) == -EINVAL && t.call(PIDFD_OPEN, &[u32::MAX as u64, 0]) == -EINVAL, "pid <= 0");
    kassert!(t.call(PIDFD_OPEN, &[NO_PID, 0]) == -ESRCH, "unknown pid");
    let fd = t.call(PIDFD_OPEN, &[cpid, 0]);
    kassert!(fd >= 0, "pidfd_open = {fd}");
    let nb = t.call(PIDFD_OPEN, &[cpid, O_NONBLOCK]);
    kassert!(nb >= 0, "pidfd_open(PIDFD_NONBLOCK) = {nb}");
    kassert!(t.call(nr::FCNTL, &[fd as u64, F_GETFD]) == 1, "close-on-exec always");
    kassert!(t.call(nr::FCNTL, &[fd as u64, F_GETFL]) == O_RDWR, "F_GETFL O_RDWR");
    kassert!(t.call(nr::FCNTL, &[nb as u64, F_GETFL]) == O_RDWR | O_NONBLOCK as i64, "F_GETFL O_RDWR|O_NONBLOCK");
    kassert!(t.call(nr::READ, &[fd as u64, t.a(100), 8]) == -EINVAL, "read");
    kassert!(t.call(nr::WRITE, &[fd as u64, t.a(100), 8]) == -EINVAL, "write");
    kassert!(t.call(nr::LSEEK, &[fd as u64, 0, 0]) == -ESPIPE, "lseek");
    kassert!(revents(&mut t, fd) == 0, "not readable while it runs");
    let info = fdinfo(&mut t, fd);
    kassert!(info.ends_with(&format!("Pid:\t{cpid}\nNSpid:\t{cpid}\n")), "fdinfo {info:?}");
    kassert!(info.starts_with("pos:\t0\nflags:\t02000002\n"), "fdinfo head {info:?}");
    // The nonblocking pidfd: waitid without WNOHANG answers EAGAIN; with WNOHANG 0 and a zeroed siginfo.
    let (r, _) = waitid(&mut t, P_PIDFD, nb, WEXITED);
    kassert!(r == -EAGAIN, "nonblocking waitid = {r}");
    let (r, si) = waitid(&mut t, P_PIDFD, nb, WEXITED | WNOHANG);
    kassert!(r == 0 && w32(&si, 16) == 0 && w32(&si, 0) == 0, "WNOHANG waitid = {r}, si_pid {}", w32(&si, 16));
    // The end: readable, then waitid through the pidfd collects it.
    kassert!(t.call(PIDFD_SEND_SIGNAL, &[fd as u64, 9, 0, 0]) == 0, "SIGKILL through the pidfd");
    kassert!(until(5, || c.0.is_finished()), "the child did not end");
    kassert!(revents(&mut t, fd) == POLLIN, "readable at the end");
    kassert!(fdinfo(&mut t, fd).ends_with(&format!("Pid:\t{cpid}\nNSpid:\t{cpid}\n")), "a zombie keeps its pid");
    kassert!(t.call(PIDFD_SEND_SIGNAL, &[fd as u64, 0, 0, 0]) == 0, "a zombie takes signals");
    let (r, si) = waitid(&mut t, P_PIDFD, fd, WEXITED);
    kassert!(r == 0, "waitid = {r}");
    kassert!(w32(&si, 0) == SIGCHLD as i32 && w32(&si, 8) == 2 && w32(&si, 16) as u64 == cpid && w32(&si, 24) == 9, "siginfo {:?}", &si[..28]);
    kassert!(c.0.status() == Some(ExitStatus::Signaled(9)));
    // Reaped: gone for every call, still readable.
    kassert!(t.call(PIDFD_SEND_SIGNAL, &[fd as u64, 0, 0, 0]) == -ESRCH, "signal after the reap");
    kassert!(waitid(&mut t, P_PIDFD, fd, WEXITED | WNOHANG).0 == -ECHILD, "waitid after the reap");
    kassert!(revents(&mut t, fd) == POLLIN, "still readable");
    let info = fdinfo(&mut t, fd);
    kassert!(info.ends_with("Pid:\t-1\nNSpid:\t-1\n"), "fdinfo after the reap {info:?}");
    kassert!(t.call(PIDFD_OPEN, &[cpid, 0]) == -ESRCH, "pidfd_open of a reaped pid");
    close(&mut t, &[fd, nb]);
    Ok(())
}

ktest!("ab.pidfd.signal_rules", Safe, signal_rules);
fn signal_rules() -> TestResult {
    let mut t = T::new("ab12-sender")?;
    let peer = T::with("ab12-peer", 1000, false)?;
    let other = T::with("ab12-other-uid", 1001, false)?;
    let root = T::with("ab12-root", 0, false)?;
    let own = t.call(PIDFD_OPEN, &[t.p.pid as u64, 0]);
    let pp = t.call(PIDFD_OPEN, &[peer.p.pid as u64, 0]);
    let op = t.call(PIDFD_OPEN, &[other.p.pid as u64, 0]);
    let rp = t.call(PIDFD_OPEN, &[root.p.pid as u64, 0]);
    kassert!(own >= 0 && pp >= 0 && op >= 0 && rp >= 0, "pidfds {own} {pp} {op} {rp}");
    kassert!(t.call(PIDFD_SEND_SIGNAL, &[pp as u64, 0, 0, 1]) == -EINVAL, "flags");
    kassert!(t.call(PIDFD_SEND_SIGNAL, &[999_999, 0, 0, 0]) == -EBADF, "closed fd");
    let efd = t.call(nr::EVENTFD2, &[0, 0]);
    kassert!(t.call(PIDFD_SEND_SIGNAL, &[efd as u64, 0, 0, 0]) == -EBADF, "not a pidfd");
    kassert!(waitid(&mut t, P_PIDFD, efd, WEXITED).0 == -EBADF, "waitid on an eventfd");
    kassert!(waitid(&mut t, P_PIDFD, -1, WEXITED).0 == -EINVAL, "waitid on -1");
    kassert!(waitid(&mut t, P_PIDFD, pp, WEXITED).0 == -ECHILD, "waitid on a non-child");
    kassert!(waitid(&mut t, P_PIDFD, own, WEXITED | WNOHANG).0 == -ECHILD, "waitid on oneself");
    kassert!(t.call(PIDFD_SEND_SIGNAL, &[pp as u64, 0, 0, 0]) == 0, "same uid");
    kassert!(t.call(PIDFD_SEND_SIGNAL, &[op as u64, 0, 0, 0]) == -EPERM, "other uid");
    kassert!(t.call(PIDFD_SEND_SIGNAL, &[rp as u64, 0, 0, 0]) == -EPERM, "root's process");
    kassert!(t.call(PIDFD_SEND_SIGNAL, &[pp as u64, 65, 0, 0]) == -EINVAL, "signal 65");
    // A program's own siginfo: the same signal, and no SI_USER/SI_TKILL/kernel code towards others.
    let mut si = [0u8; 128];
    si[8..12].copy_from_slice(&(-1i32).to_le_bytes()); // SI_QUEUE
    t.put(1000, &si);
    kassert!(t.call(PIDFD_SEND_SIGNAL, &[pp as u64, 0, t.a(1000), 0]) == 0, "SI_QUEUE to a peer");
    si[0..4].copy_from_slice(&10i32.to_le_bytes());
    t.put(1000, &si);
    kassert!(t.call(PIDFD_SEND_SIGNAL, &[pp as u64, 0, t.a(1000), 0]) == -EINVAL, "si_signo differs");
    si[0..4].copy_from_slice(&0i32.to_le_bytes());
    si[8..12].copy_from_slice(&0i32.to_le_bytes()); // SI_USER
    t.put(1000, &si);
    kassert!(t.call(PIDFD_SEND_SIGNAL, &[pp as u64, 0, t.a(1000), 0]) == -EPERM, "SI_USER to a peer");
    kassert!(t.call(PIDFD_SEND_SIGNAL, &[own as u64, 0, t.a(1000), 0]) == 0, "SI_USER to oneself");
    si[8..12].copy_from_slice(&(-6i32).to_le_bytes()); // SI_TKILL
    t.put(1000, &si);
    kassert!(t.call(PIDFD_SEND_SIGNAL, &[pp as u64, 0, t.a(1000), 0]) == -EPERM, "SI_TKILL to a peer");
    kassert!(t.call(PIDFD_SEND_SIGNAL, &[pp as u64, 0, KERNEL_ADDR, 0]) == -EFAULT, "bad siginfo");
    // A real signal through the pidfd ends the peer.
    kassert!(t.call(PIDFD_SEND_SIGNAL, &[pp as u64, 15, 0, 0]) == 0, "SIGTERM");
    kassert!(until(5, || peer.p.is_finished()), "the peer did not end");
    kassert!(peer.p.status() == Some(ExitStatus::Signaled(15)), "peer {:?}", peer.p.status());
    close(&mut t, &[own, pp, op, rp, efd]);
    Ok(())
}

/// clone3 with a VER0 `struct clone_args` (64 bytes) at `at`: SIGCHLD as the exit signal (none with CLONE_THREAD
/// or CLONE_PARENT, which clone3 refuses with one: linux-6.6/kernel/fork.c clone3_args_valid).
fn clone3(t: &mut T, at: u64, flags: u64, pidfd: u64) -> i64 {
    let mut b = [0u8; 64];
    b[0..8].copy_from_slice(&flags.to_le_bytes());
    b[8..16].copy_from_slice(&pidfd.to_le_bytes());
    if flags & (CLONE_THREAD | CLONE_PARENT) == 0 {
        b[32..40].copy_from_slice(&SIGCHLD.to_le_bytes());
    }
    t.put(at, &b);
    t.call(nr::CLONE3, &[t.a(at), 64])
}

ktest!("ab.pidfd.clone", Safe, clone);
fn clone() -> TestResult {
    let mut t = T::new("ab12-clone")?;
    // EFAULT on the pidfd pointer fails the clone: no child, no fd left behind.
    let probe = t.call(nr::EVENTFD2, &[0, 0]);
    t.call(nr::CLOSE, &[probe as u64]);
    kassert!(clone3(&mut t, 1200, CLONE_PIDFD, KERNEL_ADDR) == -EFAULT, "bad pidfd pointer");
    kassert!(t.call(nr::WAIT4, &[u64::MAX, 0, WNOHANG, 0]) == -ECHILD, "a child was left");
    let again = t.call(nr::EVENTFD2, &[0, 0]);
    kassert!(again == probe, "an fd leaked: {again} != {probe}");
    t.call(nr::CLOSE, &[again as u64]);
    let thread = CLONE_PIDFD | CLONE_THREAD | 0x800 | 0x100 | 0x200 | 0x400;
    let a1300 = t.a(1300);
    kassert!(clone3(&mut t, 1200, thread, a1300) == -EINVAL, "CLONE_PIDFD | CLONE_THREAD");
    // clone3: the child (the held image's exit_group(0)) and its pidfd.
    t.put(1300, &[0xff; 4]);
    let pid = clone3(&mut t, 1200, CLONE_PIDFD, a1300);
    kassert!(pid > 0, "clone3 = {pid}");
    let fd = w32(&t.get(1300, 4), 0) as i64;
    kassert!(fd >= 0 && t.call(nr::FCNTL, &[fd as u64, F_GETFD]) == 1, "pidfd {fd} close-on-exec");
    let child = proc::find(pid as u32);
    kassert!(until(5, || child.as_ref().is_none_or(|c| c.is_finished())), "the child did not end");
    drop(child);
    kassert!(fdinfo(&mut t, fd).contains(&format!("Pid:\t{pid}\n")), "fdinfo names the zombie");
    kassert!(revents(&mut t, fd) == POLLIN, "readable at the end");
    let (r, si) = waitid(&mut t, P_PIDFD, fd, WEXITED);
    kassert!(r == 0 && w32(&si, 16) as i64 == pid && w32(&si, 8) == 1 && w32(&si, 24) == 0, "waitid {r} {:?}", &si[..28]);
    t.call(nr::CLOSE, &[fd as u64]);
    // Legacy clone: the pidfd goes to parent_tid; with CLONE_PARENT_SETTID the same address is EINVAL.
    let flags = CLONE_PIDFD | SIGCHLD;
    kassert!(t.call(nr::CLONE, &[flags | CLONE_PARENT_SETTID, 0, t.a(1300), 0, 0]) == -EINVAL, "PIDFD + PARENT_SETTID");
    let pid = t.call(nr::CLONE, &[flags, 0, t.a(1300), 0, 0]);
    kassert!(pid > 0, "clone = {pid}");
    let fd = w32(&t.get(1300, 4), 0) as i64;
    kassert!(fd >= 0, "legacy pidfd {fd}");
    // P_PID collects it; the pidfd then sees a reaped process.
    kassert!(until(5, || waitid(&mut t, P_PID, pid, WEXITED | WNOHANG).1[16..20] == (pid as i32).to_le_bytes()), "P_PID collect");
    kassert!(waitid(&mut t, P_PIDFD, fd, WEXITED | WNOHANG).0 == -ECHILD, "reaped through P_PID");
    kassert!(fdinfo(&mut t, fd).ends_with("Pid:\t-1\nNSpid:\t-1\n"), "fdinfo after the reap");
    // A later child (a zombie now) is never what the old pidfd means: it stays ECHILD and keeps Pid: -1.
    let next = clone3(&mut t, 1200, 0, 0);
    kassert!(next > 0, "next clone3 = {next}");
    let nc = proc::find(next as u32);
    kassert!(until(5, || nc.as_ref().is_none_or(|c| c.is_finished())), "the next child did not end");
    drop(nc);
    kassert!(waitid(&mut t, P_PIDFD, fd, WEXITED | WNOHANG).0 == -ECHILD, "old pidfd took the next child");
    kassert!(fdinfo(&mut t, fd).ends_with("Pid:\t-1\nNSpid:\t-1\n"), "old fdinfo after the next child");
    kassert!(waitid(&mut t, P_PID, next, WEXITED | WNOHANG).1[16..20] == (next as i32).to_le_bytes(), "P_PID collects the next child");
    t.call(nr::CLOSE, &[fd as u64]);
    Ok(())
}

ktest!("ab.pidfd.clone_parent", Safe, clone_parent);
/// CLONE_PIDFD | CLONE_PARENT: the child is the caller's sibling, so waitid through the caller's pidfd is ECHILD
/// (linux-6.6/kernel/exit.c `eligible_child` sees only the caller's own children), while poll still works and
/// the caller's parent collects it.
fn clone_parent() -> TestResult {
    let mut outer = T::new("ab12-cparent-outer")?;
    let mut inner = T::child_of("ab12-cparent-inner", &outer.p)?;
    let a1300 = inner.a(1300);
    inner.put(1300, &[0xff; 4]);
    let pid = clone3(&mut inner, 1200, CLONE_PIDFD | CLONE_PARENT, a1300);
    kassert!(pid > 0, "clone3 = {pid}");
    let fd = w32(&inner.get(1300, 4), 0) as i64;
    kassert!(fd >= 0, "pidfd {fd}");
    let child = proc::find(pid as u32);
    kassert!(child.as_ref().is_none_or(|c| c.ppid() == outer.p.pid), "the child's parent is not the caller's parent");
    kassert!(until(5, || child.as_ref().is_none_or(|c| c.is_finished())), "the child did not end");
    drop(child);
    kassert!(revents(&mut inner, fd) == POLLIN, "readable at the end");
    kassert!(waitid(&mut inner, P_PIDFD, fd, WEXITED | WNOHANG).0 == -ECHILD, "P_PIDFD of a sibling");
    kassert!(waitid(&mut inner, P_PIDFD, fd, WEXITED).0 == -ECHILD, "blocking P_PIDFD of a sibling");
    let (r, si) = waitid(&mut outer, P_PID, pid, WEXITED | WNOHANG);
    kassert!(r == 0 && w32(&si, 16) as i64 == pid, "the caller's parent collects it: {r}");
    kassert!(fdinfo(&mut inner, fd).ends_with("Pid:\t-1\nNSpid:\t-1\n"), "fdinfo after the reap");
    inner.call(nr::CLOSE, &[fd as u64]);
    drop(inner);
    Ok(())
}

ktest!("ab.pidfd.getfd", Safe, getfd);
fn getfd() -> TestResult {
    let mut t = T::new("ab12-getter")?;
    let mut target = T::with("ab12-getfd-target", 1000, false)?;
    let mut stranger = T::with("ab12-getfd-stranger", 1001, false)?;
    let mut root = T::with("ab12-getfd-root", 0, false)?;
    let tfd = target.call(nr::EVENTFD2, &[0, 0]);
    let sfd = stranger.call(nr::EVENTFD2, &[0, 0]);
    kassert!(tfd >= 0 && sfd >= 0, "eventfds {tfd} {sfd}");
    let pt = t.call(PIDFD_OPEN, &[target.p.pid as u64, 0]);
    let ps = t.call(PIDFD_OPEN, &[stranger.p.pid as u64, 0]);
    kassert!(t.call(PIDFD_GETFD, &[pt as u64, tfd as u64, 1]) == -EINVAL, "flags");
    kassert!(t.call(PIDFD_GETFD, &[999_999, tfd as u64, 0]) == -EBADF, "closed pidfd");
    let efd = t.call(nr::EVENTFD2, &[0, 0]);
    kassert!(t.call(PIDFD_GETFD, &[efd as u64, 0, 0]) == -EBADF, "not a pidfd");
    kassert!(t.call(PIDFD_GETFD, &[pt as u64, 999_999, 0]) == -EBADF, "no such fd there");
    kassert!(t.call(PIDFD_GETFD, &[ps as u64, sfd as u64, 0]) == -EPERM, "another uid");
    let n = t.call(PIDFD_GETFD, &[pt as u64, tfd as u64, 0]);
    kassert!(n >= 0, "pidfd_getfd = {n}");
    kassert!(t.call(nr::FCNTL, &[n as u64, F_GETFD]) == 1, "close-on-exec");
    let same = match (t.p.fds.get(n as usize), target.p.fds.get(tfd as usize)) {
        (Ok(a), Ok(b)) => Arc::ptr_eq(&a, &b),
        _ => false,
    };
    kassert!(same, "not the same open file");
    // Not dumpable: only root (and the process itself) may.
    kassert!(target.call(nr::PRCTL, &[4, 0]) == 0, "PR_SET_DUMPABLE 0");
    kassert!(t.call(PIDFD_GETFD, &[pt as u64, tfd as u64, 0]) == -EPERM, "undumpable target");
    let rt = root.call(PIDFD_OPEN, &[target.p.pid as u64, 0]);
    kassert!(root.call(PIDFD_GETFD, &[rt as u64, tfd as u64, 0]) >= 0, "root may");
    let own = target.call(PIDFD_OPEN, &[target.p.pid as u64, 0]);
    kassert!(target.call(PIDFD_GETFD, &[own as u64, tfd as u64, 0]) >= 0, "its own process");
    // Gone (no parent: reaped at once): ESRCH.
    drop(stranger);
    kassert!(until(5, || t.call(PIDFD_GETFD, &[ps as u64, 0, 0]) == -ESRCH), "after the end");
    close(&mut t, &[pt, ps, n, efd]);
    Ok(())
}

ktest!("ab.pidfd.epoll", Safe, epoll);
fn epoll() -> TestResult {
    let mut t = T::new("ab12-epoll")?;
    let c = sleeper("ab12-epoll-child", &t.p)?;
    let fd = t.call(PIDFD_OPEN, &[c.0.pid as u64, 0]);
    let ep = t.call(nr::EPOLL_CREATE1, &[0]);
    kassert!(fd >= 0 && ep >= 0, "fds {fd} {ep}");
    let mut ev = [0u8; 12];
    ev[..4].copy_from_slice(&1u32.to_le_bytes());
    ev[4..12].copy_from_slice(&0x55u64.to_le_bytes());
    t.put(2000, &ev);
    kassert!(t.call(nr::EPOLL_CTL, &[ep as u64, 1, fd as u64, t.a(2000)]) == 0, "EPOLL_CTL_ADD");
    kassert!(t.call(nr::EPOLL_WAIT, &[ep as u64, t.a(2100), 4, 0]) == 0, "not ready while it runs");
    c.0.kill(9);
    kassert!(until(5, || c.0.is_finished()), "the child did not end");
    // Level-triggered: ready on every look.
    for round in 0..2 {
        let n = t.call(nr::EPOLL_WAIT, &[ep as u64, t.a(2100), 4, 0]);
        let out = t.get(2100, 12);
        kassert!(n == 1 && w32(&out, 0) == 1 && out[4] == 0x55, "round {round}: {n} {:?}", out);
    }
    kassert!(waitid(&mut t, P_PIDFD, fd, WEXITED).0 == 0, "collect");
    kassert!(t.call(nr::EPOLL_WAIT, &[ep as u64, t.a(2100), 4, 0]) == 1, "readable after the reap");
    close(&mut t, &[ep, fd]);
    Ok(())
}
