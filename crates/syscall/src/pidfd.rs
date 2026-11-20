//! AB-12, L7-PROC: process handles. `pidfd_open` (linux-7.3.1/kernel/pid.c:697, `pidfd_prepare` kernel/fork.c:1918),
//! `pidfd_send_signal` (kernel/signal.c:4144 with the PIDFD_SIGNAL_* scopes and the PIDFD_SELF_* pseudo fds),
//! `pidfd_getfd` (kernel/pid.c:927, `__pidfd_fget` :850) and the pidfs ioctls (fs/pidfs.c:530 `pidfd_ioctl`, :365
//! `pidfd_info`); the pidfd file itself is `proc::pidfd`, CLONE_PIDFD is in `proc::clone`, waitid(P_PIDFD) in
//! `process.rs`, the fdinfo lines in `procfs/fd.rs`.

use crate::SysCtx;
use alloc::sync::Arc;
use proc::pidfd::Target;
use proc::signal::{SigInfo, To};
use proc::Process;
use uapi::ab::pidfd::*;
use uapi::ab::siginfo::{SI_TKILL, SI_USER};
use uapi::errno::*;

/// Linux `pidfd_get_pid` / `pidfd_pid`: what an fd is a pidfd of, and the fd's F_GETFL. EBADF for a closed fd
/// and for every file that is not a pidfd.
pub(crate) fn get_target(c: &SysCtx<'_>, fd: i32) -> Result<(Target, u64), i64> {
    let f = (fd >= 0).then(|| c.proc.fds.get(fd as usize).ok()).flatten().ok_or(-EBADF)?;
    let t = proc::pidfd::target_of(&f).ok_or(-EBADF)?;
    Ok((t, f.status_flags()))
}

/// Whether the caller's pid namespace is the pidfd's or one above it (linux-7.3.1/kernel/signal.c:4042
/// `access_pidfd_pidns`, fs/pidfs.c:330 `pid_in_current_pidns`). Decided by the namespaces, not by the numbers
/// table: a `struct pid` keeps its numbers after the reap, the table gives them back.
pub(crate) fn visible(c: &SysCtx<'_>, t: &Target) -> bool {
    c.proc.pid_ns.is_ancestor_of(&t.group.pid_ns)
}

/// `pidfd_open(pid, flags)`: PIDFD_NONBLOCK and PIDFD_THREAD; an unknown id (or one already reaped) is ESRCH; a
/// thread that is not a group leader is ENOENT without PIDFD_THREAD (`pidfd_prepare`).
fn sys_pidfd_open(c: &mut SysCtx<'_>) -> i64 {
    let (pid, flags) = (c.args[0] as i32, c.args[1] as u32 as u64);
    if flags & !(PIDFD_NONBLOCK | PIDFD_THREAD) != 0 || pid <= 0 {
        return -EINVAL;
    }
    let leader = crate::ns_sys::find(c, pid as u32);
    let thread = crate::ns_sys::find_thread(c, pid as u32);
    let (group, target) = match (&leader, &thread) {
        (Some(p), _) => (p.group().clone(), (p.pid, None)),
        (None, Some(t)) if flags & PIDFD_THREAD != 0 => (t.group().clone(), (t.tid(), Some(t.clone()))),
        (None, Some(_)) => return -ENOENT,
        (None, None) => return -ESRCH,
    };
    let thread = (flags & PIDFD_THREAD != 0).then_some(target);
    let f = proc::pidfd::new_file(&group, thread, flags & PIDFD_NONBLOCK != 0, false);
    match c.proc.fds.install_flags(f, 0, true) {
        Ok(fd) => fd as i64,
        Err(e) => -e,
    }
}
crate::syscall_nr!(uapi::ab::nr_more::PIDFD_OPEN, "pidfd_open", sys_pidfd_open);

/// Where a pidfd_send_signal goes (Linux's `enum pid_type` of the call).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Scope {
    Thread,
    Group,
    ProcessGroup,
}

/// The scope flags of pidfd_send_signal (pure, host-tested): unknown bits and more than one scope are EINVAL;
/// no flag keeps the pidfd's own scope.
pub(crate) fn scope_of(flags: u32, own: Scope) -> Result<Scope, i64> {
    let all = PIDFD_SIGNAL_THREAD | PIDFD_SIGNAL_THREAD_GROUP | PIDFD_SIGNAL_PROCESS_GROUP;
    if flags & !all != 0 || (flags & all).count_ones() > 1 {
        return Err(-EINVAL);
    }
    Ok(match flags {
        PIDFD_SIGNAL_THREAD => Scope::Thread,
        PIDFD_SIGNAL_THREAD_GROUP => Scope::Group,
        PIDFD_SIGNAL_PROCESS_GROUP => Scope::ProcessGroup,
        _ => own,
    })
}

/// `pidfd_send_signal(pidfd, sig, info, flags)`: kill(2)'s rules (`send`) towards the pidfd's thread, process or
/// process group. A program's own siginfo must carry the same signal (EINVAL) and may pretend to be the kernel or
/// tkill only towards the calling thread itself, never to a process group (EPERM). A pid without its task is
/// ESRCH; a zombie takes the signal and drops it (0).
fn sys_pidfd_send_signal(c: &mut SysCtx<'_>) -> i64 {
    let (pidfd, sig, uinfo, flags) = (c.args[0] as i32, c.args[1] as i32, c.args[2], c.args[3] as u32);
    if let Err(e) = scope_of(flags, Scope::Group) {
        return e;
    }
    let (t, own) = match pidfd {
        PIDFD_SELF_THREAD => (Target { group: c.proc.group().clone(), thread: Some((c.thread.tid(), Some(c.thread.clone()))) }, Scope::Thread),
        PIDFD_SELF_THREAD_GROUP => (Target { group: c.proc.group().clone(), thread: None }, Scope::Group),
        fd => match get_target(c, fd) {
            // `access_pidfd_pidns`: the pid must be visible from the caller's pid namespace.
            Ok((t, _)) if !visible(c, &t) => return -EINVAL,
            Ok((t, _)) => {
                // linux-7.3.1/kernel/signal.c:4182: the file's PIDFD_THREAD decides, not the pid: a non-leader's
                // pid opened by handle without O_EXCL signals that thread's whole process (PIDTYPE_TGID).
                let thread = c.proc.fds.get(fd as usize).ok().and_then(|f| proc::pidfd::shown_flags(&f)).is_some_and(|s| s & PIDFD_THREAD != 0);
                (t, if thread { Scope::Thread } else { Scope::Group })
            }
            Err(e) => return e,
        },
    };
    let scope = scope_of(flags, own).unwrap_or(own);
    // `task_pid(current) != pid`: the calling thread's own pid (for the group scope, only its leader's).
    let me = c.thread.tid() == t.id() && Arc::ptr_eq(c.proc.group(), &t.group);
    let info = if uinfo != 0 {
        match user_info(c, sig, uinfo, me && scope != Scope::ProcessGroup, t.group.tgid) {
            Ok(i) => i,
            Err(e) => return e,
        }
    } else {
        crate::signal::kill_info(c, sig, if scope == Scope::Thread { SI_TKILL } else { SI_USER })
    };
    match scope {
        Scope::ProcessGroup => {
            // `kill_pgrp_info`: the group the pid names as a PGID, also after its leader was reaped (the
            // members keep the `struct pid`). A pid number is not given out while it is anyone's pgid
            // (`proc::id_in_use`), so a reaped pid whose number another task holds now has no group of its own.
            let pgid = t.id();
            if !t.attached() && (proc::find(pgid).is_some() || proc::find_thread(pgid).is_some()) {
                return -ESRCH;
            }
            let members = crate::signal::group(pgid);
            let mut ret = -ESRCH;
            let mut ok = false;
            for p in &members {
                match crate::signal::send(c, p, sig, info, To::Group) {
                    Ok(()) => ok = true,
                    Err(e) => ret = e,
                }
            }
            if ok { 0 } else { ret }
        }
        Scope::Thread => {
            let Some(p) = proc::pidfd::process_of(&t.group).filter(|_| t.attached()) else { return -ESRCH };
            let th = if t.is_thread() { t.live_thread() } else { proc::find_thread(t.id()).filter(|x| Arc::ptr_eq(x.group(), &t.group)) };
            // A leader that ended before its other threads still holds its pid: Linux checks the permission on
            // that zombie thread (`group_send_sig_info`) and then drops the signal.
            let Some(th) = th else { return crate::signal::may_send(c, &p, sig, &info).err().unwrap_or(0) };
            crate::signal::send(c, &p, sig, info, To::Thread(th.sig_key())).err().unwrap_or(0)
        }
        Scope::Group => {
            if !t.attached() {
                return -ESRCH;
            }
            let Some(p) = proc::pidfd::process_of(&t.group) else { return -ESRCH };
            crate::signal::send(c, &p, sig, info, To::Group).err().unwrap_or(0)
        }
    }
}
crate::syscall_nr!(uapi::ab::nr_more::PIDFD_SEND_SIGNAL, "pidfd_send_signal", sys_pidfd_send_signal);

/// `copy_siginfo_from_user_any` and pidfd_send_signal's two checks, in Linux's order (EFAULT, E2BIG, EINVAL,
/// EPERM): a forged si_code (the kernel's or tkill's) only towards the caller itself (`to_self`).
fn user_info(c: &SysCtx<'_>, sig: i32, uinfo: u64, to_self: bool, target: u32) -> Result<SigInfo, i64> {
    let mut b = [0u8; SigInfo::USER_SIZE];
    if c.proc.mm.copy_from_user(&mut b, uinfo).is_err() {
        return Err(-EFAULT);
    }
    let signo = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let info = SigInfo::from_user(signo as u32, &b).map_err(|e| -e)?;
    if signo != sig {
        return Err(-EINVAL);
    }
    if (info.code() >= 0 || info.code() == SI_TKILL) && !to_self {
        klog::event!(Info, "syscall", "pidfd.signal_refused", pid = c.proc.pid, target = target, code = info.code());
        return Err(-EPERM);
    }
    Ok(info)
}

/// `ptrace_may_access(task, PTRACE_MODE_ATTACH_REALCREDS)` without an LSM (`Process::may_ptrace`, AB-13).
pub(crate) fn may_attach(me: &Process, target: &Process) -> bool {
    me.may_ptrace(target, false)
}

/// `pidfd_getfd(pidfd, targetfd, flags)`: a duplicate of another process's fd (the same open file), installed
/// close-on-exec in the caller's table. ESRCH once the pid has no task, EPERM without ptrace rights, ESRCH while
/// the task is exiting (`PF_EXITING`), EBADF when the fd is not open there.
fn sys_pidfd_getfd(c: &mut SysCtx<'_>) -> i64 {
    let (pidfd, fd, flags) = (c.args[0] as i32, c.args[1] as i32, c.args[2] as u32);
    if flags != 0 {
        return -EINVAL;
    }
    let t = match get_target(c, pidfd) {
        Ok((t, _)) => t,
        Err(e) => return e,
    };
    // `get_pid_task(pid, PIDTYPE_PID)`: the leader until the reap, a thread until it ended.
    let Some(target) = proc::pidfd::process_of(&t.group).filter(|_| t.attached()) else { return -ESRCH };
    if !may_attach(c.proc, &target) {
        klog::event!(Info, "syscall", "pidfd.getfd_refused", pid = c.proc.pid, uid = c.proc.cred().uid, target = target.pid, target_uid = target.cred().uid);
        return -EPERM;
    }
    // PF_EXITING: the task (the leader for a process pidfd) began to end.
    let exiting = match t.live_thread() {
        Some(th) => target.has_exited() || th.exit_requested(),
        None => target.has_exited() || !proc::find_thread(t.group.tgid).is_some_and(|x| Arc::ptr_eq(x.group(), &t.group)),
    };
    if exiting {
        return -ESRCH;
    }
    let f = match (fd >= 0).then(|| target.fds.get(fd as usize).ok()).flatten() {
        Some(f) => f,
        None => return -EBADF,
    };
    match c.proc.fds.install_flags(f, 0, true) {
        Ok(n) => {
            klog::event!(Debug, "syscall", "pidfd.getfd", pid = c.proc.pid, target = target.pid, fd = fd, new_fd = n);
            n as i64
        }
        Err(e) => -e,
    }
}
crate::syscall_nr!(uapi::ab::nr_more::PIDFD_GETFD, "pidfd_getfd", sys_pidfd_getfd);

#[path = "pidfd_ioctl.rs"]
mod ioctl;
pub(crate) use ioctl::pidfd_ioctl;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_signal_scopes_like_linux() {
        assert_eq!(scope_of(0, Scope::Group), Ok(Scope::Group));
        assert_eq!(scope_of(0, Scope::Thread), Ok(Scope::Thread), "a thread pidfd's own scope");
        assert_eq!(scope_of(PIDFD_SIGNAL_THREAD, Scope::Group), Ok(Scope::Thread));
        assert_eq!(scope_of(PIDFD_SIGNAL_THREAD_GROUP, Scope::Thread), Ok(Scope::Group));
        assert_eq!(scope_of(PIDFD_SIGNAL_PROCESS_GROUP, Scope::Group), Ok(Scope::ProcessGroup));
        assert_eq!(scope_of(3, Scope::Group), Err(-EINVAL), "two scopes");
        assert_eq!(scope_of(8, Scope::Group), Err(-EINVAL), "an unknown bit");
    }
}
