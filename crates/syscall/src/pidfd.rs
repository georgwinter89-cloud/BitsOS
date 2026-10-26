//! AB-12: process handles. `pidfd_open` (linux-6.6/kernel/pid.c:627), `pidfd_send_signal` (kernel/signal.c:3882)
//! and `pidfd_getfd` (kernel/pid.c:725); the pidfd file itself is `proc::pidfd`, CLONE_PIDFD is in
//! `proc::clone`, waitid(P_PIDFD) in `process.rs`, the fdinfo lines in `procfs/fd.rs`.

use crate::SysCtx;
use alloc::sync::Arc;
use proc::{Group, Process};
use proc::signal::{SigInfo, To};
use uapi::ab::pidfd::PIDFD_NONBLOCK;
use uapi::ab::siginfo::{SI_TKILL, SI_USER};
use uapi::errno::*;

/// Linux `pidfd_get_pid` / `pidfd_pid`: the process an fd is a pidfd of, and the fd's F_GETFL. EBADF for a
/// closed fd and for every file that is not a pidfd.
pub(crate) fn get(c: &SysCtx<'_>, fd: i32) -> Result<(Arc<Group>, u64), i64> {
    let f = (fd >= 0).then(|| c.proc.fds.get(fd as usize).ok()).flatten().ok_or(-EBADF)?;
    let g = proc::pidfd::group_of(&f).ok_or(-EBADF)?;
    Ok((g, f.status_flags()))
}

/// `pidfd_open(pid, flags)`: a pidfd for a thread-group leader (live or a zombie); a thread's id is EINVAL
/// (`pidfd_prepare`: no PIDTYPE_TGID task), an unknown id ESRCH.
fn sys_pidfd_open(c: &mut SysCtx<'_>) -> i64 {
    let (pid, flags) = (c.args[0] as i32, c.args[1] as u32 as u64);
    if flags & !PIDFD_NONBLOCK != 0 || pid <= 0 {
        return -EINVAL;
    }
    let target = match proc::find(pid as u32) {
        Some(p) => p,
        None if proc::find_thread(pid as u32).is_some() => return -EINVAL,
        None => return -ESRCH,
    };
    let f = proc::pidfd::new_file(target.group(), flags & PIDFD_NONBLOCK != 0);
    match c.proc.fds.install_flags(f, 0, true) {
        Ok(fd) => fd as i64,
        Err(e) => -e,
    }
}
crate::syscall_nr!(uapi::ab::nr_more::PIDFD_OPEN, "pidfd_open", sys_pidfd_open);

/// `pidfd_send_signal(pidfd, sig, info, flags)`: kill(2) to the process behind the pidfd with kill's rules
/// (`send`). A program's own siginfo must carry the same signal (EINVAL) and may pretend to be the kernel or
/// tkill only towards the calling thread's own pid (EPERM). A reaped process is ESRCH; a zombie takes the
/// signal and drops it (0).
fn sys_pidfd_send_signal(c: &mut SysCtx<'_>) -> i64 {
    let (pidfd, sig, uinfo, flags) = (c.args[0] as i32, c.args[1] as i32, c.args[2], c.args[3] as u32);
    if flags != 0 {
        return -EINVAL;
    }
    let g = match get(c, pidfd) {
        Ok((g, _)) => g,
        Err(e) => return e,
    };
    let info = if uinfo != 0 {
        match pidfd_info(c, sig, uinfo, &g) {
            Ok(i) => i,
            Err(e) => return e,
        }
    } else {
        crate::signal::kill_info(c, sig, SI_USER)
    };
    let Some(target) = proc::pidfd::process_of(&g) else { return -ESRCH };
    crate::signal::send(c, &target, sig, info, To::Group).err().unwrap_or(0)
}
crate::syscall_nr!(uapi::ab::nr_more::PIDFD_SEND_SIGNAL, "pidfd_send_signal", sys_pidfd_send_signal);

/// `copy_siginfo_from_user_any` and pidfd_send_signal's two checks, in Linux's order (EFAULT, E2BIG, EINVAL,
/// EPERM). `task_pid(current) != pid`: only the calling thread's own pid (the leader of its own process) may
/// send a forged si_code.
fn pidfd_info(c: &SysCtx<'_>, sig: i32, uinfo: u64, g: &Arc<Group>) -> Result<SigInfo, i64> {
    let mut b = [0u8; SigInfo::USER_SIZE];
    if c.proc.mm.copy_from_user(&mut b, uinfo).is_err() {
        return Err(-EFAULT);
    }
    let signo = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let info = SigInfo::from_user(signo as u32, &b).map_err(|e| -e)?;
    if signo != sig {
        return Err(-EINVAL);
    }
    let to_self = Arc::ptr_eq(c.proc.group(), g) && c.thread.tid() == g.tgid;
    if (info.code() >= 0 || info.code() == SI_TKILL) && !to_self {
        klog::event!(Info, "syscall", "pidfd.signal_refused", pid = c.proc.pid, target = g.tgid, code = info.code());
        return Err(-EPERM);
    }
    Ok(info)
}

/// `ptrace_may_access(task, PTRACE_MODE_ATTACH_REALCREDS)` without an LSM (`Process::may_ptrace`, AB-13).
pub(crate) fn may_attach(me: &Process, target: &Process) -> bool {
    me.may_ptrace(target, false)
}

/// `pidfd_getfd(pidfd, targetfd, flags)`: a duplicate of another process's fd (the same open file), installed
/// close-on-exec in the caller's table. ESRCH once the process is reaped, EPERM without ptrace rights, EBADF
/// when the fd is not open there or the process's leader no longer has an fd table (it ended).
fn sys_pidfd_getfd(c: &mut SysCtx<'_>) -> i64 {
    let (pidfd, fd, flags) = (c.args[0] as i32, c.args[1] as i32, c.args[2] as u32);
    if flags != 0 {
        return -EINVAL;
    }
    let g = match get(c, pidfd) {
        Ok((g, _)) => g,
        Err(e) => return e,
    };
    let Some(target) = proc::pidfd::process_of(&g) else { return -ESRCH };
    if !may_attach(c.proc, &target) {
        klog::event!(Info, "syscall", "pidfd.getfd_refused", pid = c.proc.pid, uid = c.proc.cred().uid, target = target.pid, target_uid = target.cred().uid);
        return -EPERM;
    }
    // `fget_task`: the leader's `files`, which `exit_files` cleared when it ended.
    let leader_runs = proc::find_thread(g.tgid).is_some_and(|t| Arc::ptr_eq(t.group(), &g));
    if !leader_runs || target.is_finished() || fd < 0 {
        return -EBADF;
    }
    let f = match target.fds.get(fd as usize) {
        Ok(f) => f,
        Err(_) => return -EBADF,
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
