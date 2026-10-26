//! Signal calls (linux-6.6/kernel/signal.c): rt_sigaction (:4446), rt_sigprocmask (:3177), rt_sigpending
//! (:3249), sigaltstack (:4263), kill (:3807, `kill_something_info` :1604), tkill (:4004), tgkill (:3988,
//! `do_send_specific` :3934); AB-6: rt_sigreturn (arch/x86/kernel/signal_64.c:241), rt_sigsuspend (:4628),
//! pause (:4588), rt_sigtimedwait (:3683), rt_sigqueueinfo (:4058), rt_tgsigqueueinfo (:4104). The state and
//! its rules live in `proc::signal`, delivery in `proc::deliver`, the thread's part in `proc::sigapi`.

use proc::cred::{Cap, Cred, capable};
use crate::SysCtx;
use alloc::sync::Arc;
use alloc::vec::Vec;
use proc::Process;
use proc::signal::{AltStack, SigAction, SigInfo, To};
use uapi::ab::sigabi::SIGSET_SIZE;
use uapi::ab::siginfo::*;
use uapi::errno::*;

fn read_u64(c: &SysCtx<'_>, addr: u64) -> Result<u64, i64> {
    c.proc.mm.read_u64(addr).map_err(|_| -EFAULT)
}

fn sys_rt_sigaction(c: &mut SysCtx<'_>) -> i64 {
    let [sig, act, oact, size, ..] = c.args;
    if size != SIGSET_SIZE {
        return -EINVAL;
    }
    let new = if act != 0 {
        let mut b = [0u8; SigAction::SIZE];
        if c.proc.mm.copy_from_user(&mut b, act).is_err() {
            return -EFAULT;
        }
        Some(SigAction::from_bytes(&b))
    } else {
        None
    };
    // An int: anything outside 1..=64 is EINVAL (0 included).
    let sig = (sig as i32).clamp(0, 65) as u32;
    let old = match c.proc.signals.raw(|s| s.sigaction(sig, new)) {
        Ok(o) => o,
        Err(e) => return -e,
    };
    if oact != 0 && c.proc.mm.copy_to_user(oact, &old.to_bytes()).is_err() {
        return -EFAULT;
    }
    0
}
crate::syscall!(RT_SIGACTION, sys_rt_sigaction);

fn sys_rt_sigprocmask(c: &mut SysCtx<'_>) -> i64 {
    let [how, nset, oset, size, ..] = c.args;
    if size != SIGSET_SIZE {
        return -EINVAL;
    }
    let set = if nset != 0 {
        match read_u64(c, nset) {
            Ok(v) => Some(v),
            Err(e) => return e,
        }
    } else {
        None
    };
    let old = match c.thread.sigprocmask(how as u32 as u64, set) {
        Ok(o) => o,
        Err(e) => return -e,
    };
    if oset != 0 && c.proc.mm.write_u64(oset, old).is_err() {
        return -EFAULT;
    }
    0
}
crate::syscall!(RT_SIGPROCMASK, sys_rt_sigprocmask);

fn sys_rt_sigpending(c: &mut SysCtx<'_>) -> i64 {
    let [uset, size, ..] = c.args;
    if size > SIGSET_SIZE {
        return -EINVAL;
    }
    let set = c.thread.pending_blocked().to_le_bytes();
    // Linux copies `sigsetsize` bytes, so a shorter size gives the low bytes only.
    if c.proc.mm.copy_to_user(uset, &set[..size as usize]).is_err() {
        return -EFAULT;
    }
    0
}
crate::syscall_nr!(uapi::ab::nr_more::RT_SIGPENDING, "rt_sigpending", sys_rt_sigpending);

fn sys_sigaltstack(c: &mut SysCtx<'_>) -> i64 {
    let [uss, uoss, ..] = c.args;
    let new = if uss != 0 {
        let mut b = [0u8; AltStack::SIZE];
        if c.proc.mm.copy_from_user(&mut b, uss).is_err() {
            return -EFAULT;
        }
        Some(AltStack::from_bytes(&b))
    } else {
        None
    };
    let sp = c.user.regs.rsp;
    let old = match c.thread.sigaltstack(new, sp) {
        Ok(o) => o,
        Err(e) => return -e,
    };
    if uoss != 0 && c.proc.mm.copy_to_user(uoss, &old.to_bytes()).is_err() {
        return -EFAULT;
    }
    0
}
crate::syscall!(SIGALTSTACK, sys_sigaltstack);

/// `check_kill_permission` + B3 + the send (sig 0 probes only). From a program (si_code <= 0) another
/// process needs its uid or root (CAP_KILL); SIGCONT may also go within the session. B3 (`policy::check`,
/// Access::Signal) asks about signals to other programs.
pub(crate) fn send(c: &SysCtx<'_>, target: &Arc<Process>, sig: i32, info: SigInfo, to: To) -> Result<(), i64> {
    if !(0..=64).contains(&sig) {
        return Err(-EINVAL);
    }
    let from_user = info.code() <= 0;
    let same_session = sig == uapi::ab::signal::SIGCONT as i32 && target.sid() == c.proc.sid();
    if from_user && !may_signal(c.proc.pid, &c.proc.cred(), target.pid, &target.cred()) && !same_session {
        klog::event!(Info, "syscall", "signal.refused", pid = c.proc.pid, target = target.pid, signal = sig, reason = "uid");
        return Err(-EPERM);
    }
    if target.pid != c.proc.pid
        && let Err(d) = policy::check(&c.proc.subject(), &policy::Access::Signal { target_pid: target.pid, signal: sig as u32 })
    {
        klog::event!(Info, "syscall", "signal.refused", pid = c.proc.pid, target = target.pid, signal = sig, reason = d.reason.as_str());
        return Err(-EPERM);
    }
    if sig != 0 && !target.is_finished() {
        let how = target.send_info(info, to, false).map_err(|e| -e)?;
        if target.pid == 1 && how == proc::signal::Sent::Dropped {
            klog::event!(Info, "syscall", "signal.ignored_by_init", from = c.proc.pid, signal = uapi::ab::signal_names::name(sig as u32));
        } else if target.pid != c.proc.pid {
            klog::event!(Info, "syscall", "signal.sent", from = c.proc.pid, to = target.pid, signal = uapi::ab::signal_names::name(sig as u32), outcome = alloc::format!("{how:?}").as_str());
        }
    }
    Ok(())
}

/// Whether process `pid` with credentials `me` may signal process `target_pid` with `t` (pure, host-tested):
/// the same process, or linux-6.6/kernel/signal.c:779 `kill_ok_by_cred` (the sender's real or effective uid
/// is the target's real or saved uid, or CAP_KILL).
pub(crate) fn may_signal(pid: u32, me: &Cred, target_pid: u32, t: &Cred) -> bool {
    pid == target_pid || me.euid == t.suid || me.euid == t.uid || me.uid == t.suid || me.uid == t.uid || capable(me, Cap::Kill)
}

/// kill(2)'s siginfo (`prepare_kill_siginfo`): SI_USER (SI_TKILL) with the caller's pid and uid.
pub(crate) fn kill_info(c: &SysCtx<'_>, sig: i32, code: i32) -> SigInfo {
    SigInfo::user(sig.max(0) as u32, code, c.proc.pid, c.proc.cred().uid)
}

/// The processes of group `pgid` (live and zombies, as Linux's pid lists).
fn group(pgid: u32) -> Vec<Arc<Process>> {
    proc::processes().into_iter().filter(|p| p.pgid() == pgid).collect()
}

/// `__kill_pgrp_info`: 0 when one send succeeded, else the last error; ESRCH for an empty group.
fn kill_group(c: &SysCtx<'_>, members: &[Arc<Process>], sig: i32) -> i64 {
    let mut ret = -ESRCH;
    let mut success = false;
    for p in members {
        match send(c, p, sig, kill_info(c, sig, SI_USER), To::Group) {
            Ok(()) => success = true,
            Err(e) => ret = e,
        }
    }
    if success { 0 } else { ret }
}

/// The process a pid names (a thread's id names its whole process: `kill_pid_info` sends to the group).
fn process_of(pid: u32) -> Option<Arc<Process>> {
    proc::find(pid).or_else(|| proc::find_thread(pid).and_then(|t| t.group().image()))
}

fn sys_kill(c: &mut SysCtx<'_>) -> i64 {
    let pid = c.args[0] as i32;
    let sig = c.args[1] as i32;
    if pid > 0 {
        let Some(t) = process_of(pid as u32) else { return -ESRCH };
        return send(c, &t, sig, kill_info(c, sig, SI_USER), To::Group).err().unwrap_or(0);
    }
    if pid == i32::MIN {
        return -ESRCH;
    }
    if pid != -1 {
        let pgid = if pid == 0 { c.proc.pgid() } else { (-pid) as u32 };
        return kill_group(c, &group(pgid), sig);
    }
    // -1: every process except pid 1 and the caller; EPERM answers do not count as the result.
    let mut count = 0;
    let mut ret = 0;
    for p in proc::processes().iter().filter(|p| p.pid > 1 && p.pid != c.proc.pid) {
        count += 1;
        if let Err(e) = send(c, p, sig, kill_info(c, sig, SI_USER), To::Group)
            && e != -EPERM
        {
            ret = e;
        }
    }
    if count > 0 { ret } else { -ESRCH }
}
crate::syscall!(KILL, sys_kill);

/// `do_send_specific`: thread `tid`, in process `tgid` when given; the signal goes to that thread
/// (PIDTYPE_PID). A thread that is gone is ESRCH.
fn send_specific(c: &SysCtx<'_>, tgid: i32, tid: i32, sig: i32, info: SigInfo) -> i64 {
    let Some(t) = proc::find_thread(tid as u32) else { return -ESRCH };
    if tgid > 0 && t.group().tgid != tgid as u32 {
        return -ESRCH;
    }
    let Some(p) = t.group().image() else { return -ESRCH };
    match send(c, &p, sig, info, To::Thread(t.sig_key())) {
        // The thread ended between the look and the send: as Linux, that is not an error.
        Err(e) if e == -ESRCH => 0,
        r => r.err().unwrap_or(0),
    }
}

fn sys_tkill(c: &mut SysCtx<'_>) -> i64 {
    let tid = c.args[0] as i32;
    if tid <= 0 {
        return -EINVAL;
    }
    let sig = c.args[1] as i32;
    send_specific(c, 0, tid, sig, kill_info(c, sig, SI_TKILL))
}
crate::syscall!(TKILL, sys_tkill);

fn sys_tgkill(c: &mut SysCtx<'_>) -> i64 {
    let (tgid, tid) = (c.args[0] as i32, c.args[1] as i32);
    if tid <= 0 || tgid <= 0 {
        return -EINVAL;
    }
    let sig = c.args[2] as i32;
    send_specific(c, tgid, tid, sig, kill_info(c, sig, SI_TKILL))
}
crate::syscall!(TGKILL, sys_tgkill);

/// A siginfo from a program (`copy_siginfo_from_user` + `do_rt_sigqueueinfo`'s rule: nobody pretends to be
/// the kernel or kill/tkill when signalling another process).
pub(crate) fn user_info(c: &SysCtx<'_>, sig: i32, uinfo: u64, target_is_self: bool) -> Result<SigInfo, i64> {
    let mut b = [0u8; SigInfo::USER_SIZE];
    if c.proc.mm.copy_from_user(&mut b, uinfo).is_err() {
        return Err(-EFAULT);
    }
    let info = SigInfo::from_user(sig.max(0) as u32, &b).map_err(|e| -e)?;
    if (info.code() >= 0 || info.code() == SI_TKILL) && !target_is_self {
        return Err(-EPERM);
    }
    Ok(info)
}

/// `rt_sigqueueinfo(pid, sig, uinfo)`: like kill to one process, with the program's siginfo.
fn sys_rt_sigqueueinfo(c: &mut SysCtx<'_>) -> i64 {
    let (pid, sig) = (c.args[0] as i32, c.args[1] as i32);
    // `task_pid_vnr(current) != pid` (kernel/signal.c do_rt_sigqueueinfo): the calling thread's id, so a
    // non-leader thread naming its own process gets EPERM for a forged si_code.
    let info = match user_info(c, sig, c.args[2], pid > 0 && pid as u32 == c.thread.tid()) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let Some(t) = (pid > 0).then(|| process_of(pid as u32)).flatten() else { return -ESRCH };
    send(c, &t, sig, info, To::Group).err().unwrap_or(0)
}
crate::syscall_nr!(uapi::ab::nr_more::RT_SIGQUEUEINFO, "rt_sigqueueinfo", sys_rt_sigqueueinfo);

/// `rt_tgsigqueueinfo(tgid, tid, sig, uinfo)`.
fn sys_rt_tgsigqueueinfo(c: &mut SysCtx<'_>) -> i64 {
    let (tgid, tid, sig) = (c.args[0] as i32, c.args[1] as i32, c.args[2] as i32);
    if tid <= 0 || tgid <= 0 {
        return -EINVAL;
    }
    // `task_pid_vnr(current) != pid`: the calling thread's own id.
    let info = match user_info(c, sig, c.args[3], tid as u32 == c.thread.tid()) {
        Ok(i) => i,
        Err(e) => return e,
    };
    send_specific(c, tgid, tid, sig, info)
}
crate::syscall_nr!(uapi::ab::nr_more::RT_TGSIGQUEUEINFO, "rt_tgsigqueueinfo", sys_rt_tgsigqueueinfo);

/// `rt_sigreturn()`: the frame at rsp - 8 (`proc::deliver::sigreturn` checks it); the restored rax is the
/// answer the dispatcher writes back.
fn sys_rt_sigreturn(c: &mut SysCtx<'_>) -> i64 {
    proc::deliver::sigreturn(c.thread, c.user)
}
crate::syscall!(RT_SIGRETURN, sys_rt_sigreturn);

/// `rt_sigsuspend(mask, sigsetsize)`.
fn sys_rt_sigsuspend(c: &mut SysCtx<'_>) -> i64 {
    let [umask, size, ..] = c.args;
    if size != SIGSET_SIZE {
        return -EINVAL;
    }
    match read_u64(c, umask) {
        Ok(m) => c.thread.sigsuspend(m),
        Err(e) => e,
    }
}
crate::syscall_nr!(uapi::ab::nr_more::RT_SIGSUSPEND, "rt_sigsuspend", sys_rt_sigsuspend);

fn sys_pause(c: &mut SysCtx<'_>) -> i64 {
    c.thread.pause()
}
crate::syscall_nr!(uapi::ab::nr_more::PAUSE, "pause", sys_pause);

/// `rt_sigtimedwait(uthese, uinfo, uts, sigsetsize)`: the signal number, or EAGAIN / EINTR.
fn sys_rt_sigtimedwait(c: &mut SysCtx<'_>) -> i64 {
    let [uthese, uinfo, uts, size, ..] = c.args;
    if size != SIGSET_SIZE {
        return -EINVAL;
    }
    let these = match read_u64(c, uthese) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let timeout = if uts != 0 {
        let mut b = [0u8; 16];
        if c.proc.mm.copy_from_user(&mut b, uts).is_err() {
            return -EFAULT;
        }
        let sec = i64::from_le_bytes(b[..8].try_into().unwrap_or([0; 8]));
        let nsec = i64::from_le_bytes(b[8..].try_into().unwrap_or([0; 8]));
        // `timespec64_valid`.
        if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
            return -EINVAL;
        }
        Some((sec as u64).saturating_mul(1_000_000_000).saturating_add(nsec as u64))
    } else {
        None
    };
    match c.thread.sigtimedwait(these, timeout) {
        Ok(info) => {
            if uinfo != 0 && c.proc.mm.copy_to_user(uinfo, &info.to_user()).is_err() {
                return -EFAULT;
            }
            info.signo() as i64
        }
        Err(e) => -e,
    }
}
crate::syscall_nr!(uapi::ab::nr_more::RT_SIGTIMEDWAIT, "rt_sigtimedwait", sys_rt_sigtimedwait);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_same_uid_or_root_may_signal() {
        let k = |u| Cred::new(u, u);
        assert!(may_signal(7, &k(1000), 7, &k(1000)), "itself");
        assert!(may_signal(7, &k(1000), 8, &k(1000)), "same uid");
        assert!(!may_signal(7, &k(1000), 1, &k(0)), "uid 1000 against init (uid 0)");
        assert!(may_signal(1, &k(0), 7, &k(1000)), "root against anyone");
        assert!(!may_signal(7, &k(1000), 9, &k(1001)));
        // kill_ok_by_cred: the sender's euid against the target's saved uid; not the target's euid.
        assert!(may_signal(7, &Cred { euid: 1001, ..k(1000) }, 9, &Cred { suid: 1001, ..k(1002) }));
        assert!(!may_signal(7, &k(1000), 9, &Cred { euid: 1000, ..k(1002) }), "the target's euid does not count");
    }
}
