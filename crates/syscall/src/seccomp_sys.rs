//! SX-1 glue (granted to SX in crates/syscall): seccomp(2), prctl's PR_SET_SECCOMP, and the dispatcher's
//! check (linux-6.6/kernel/seccomp.c `do_seccomp`, `seccomp_set_mode_filter`, `prctl_set_seccomp`,
//! `__seccomp_filter`). The filters and their rules are in `crates/seccomp`.

use crate::SysCtx;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use hal::entry::UserContext;
use proc::{Process, Thread};
use seccomp::consts::*;
use seccomp::{AttachError, Data, Verdict};
use uapi::ab::signal::{SIGKILL, SIGSYS};
use uapi::errno::*;

klog::counter!(ERRNOS, "seccomp.errno");
klog::counter!(TRAPS, "seccomp.trap");
klog::counter!(KILLS, "seccomp.kill");
klog::counter!(LOGGED, "seccomp.log");
klog::counter!(ATTACHED, "seccomp.attach");

/// A bound against a hostile thread group: TSYNC walks the thread list again while threads that inherited
/// the old stack keep appearing (a clone racing the walk); a walk that changes nothing ends it.
const TSYNC_PASSES: u32 = 64;

fn sys_seccomp(c: &mut SysCtx<'_>) -> i64 {
    // `unsigned int op, unsigned int flags, void __user *uargs`.
    do_seccomp(c, c.args[0] as u32, c.args[1] as u32, c.args[2])
}
crate::syscall_nr!(NR_SECCOMP, "seccomp", sys_seccomp);

/// `prctl_set_seccomp`: strict mode ignores the third argument; prctl has no flags.
pub(crate) fn prctl_set(c: &mut SysCtx<'_>, mode: u64, filter: u64) -> i64 {
    match mode {
        1 => do_seccomp(c, SET_MODE_STRICT, 0, 0),
        2 => do_seccomp(c, SET_MODE_FILTER, 0, filter),
        _ => -EINVAL,
    }
}

fn do_seccomp(c: &mut SysCtx<'_>, op: u32, flags: u32, uargs: u64) -> i64 {
    match op {
        SET_MODE_STRICT => {
            if flags != 0 || uargs != 0 {
                return -EINVAL;
            }
            let g = seccomp::change();
            match c.thread.prctl().seccomp().set_strict(&g) {
                Ok(()) => {
                    klog::event!(Info, "seccomp", "mode.strict", pid = c.proc.pid, tid = c.thread.tid());
                    0
                }
                Err(_) => -EINVAL,
            }
        }
        SET_MODE_FILTER => set_filter(c, flags, uargs),
        GET_ACTION_AVAIL => {
            if flags != 0 {
                return -EINVAL;
            }
            let mut b = [0u8; 4];
            if c.proc.mm.copy_from_user(&mut b, uargs).is_err() {
                return -EFAULT;
            }
            if seccomp::action_available(u32::from_le_bytes(b)) { 0 } else { -EOPNOTSUPP }
        }
        GET_NOTIF_SIZES => {
            if flags != 0 {
                return -EINVAL;
            }
            let mut b = [0u8; 6];
            for (i, v) in NOTIF_SIZES.iter().enumerate() {
                b[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
            }
            c.proc.mm.copy_to_user(uargs, &b).map(|_| 0).unwrap_or(-EFAULT)
        }
        _ => -EINVAL,
    }
}

static LISTENER_LOGGED: AtomicBool = AtomicBool::new(false);

fn set_filter(c: &mut SysCtx<'_>, flags: u32, uargs: u64) -> i64 {
    if flags & !FLAGS_BUILT != 0 {
        if flags & (FLAG_NEW_LISTENER | FLAG_WAIT_KILLABLE_RECV) != 0 && !LISTENER_LOGGED.swap(true, Ordering::Relaxed) {
            klog::event!(Info, "seccomp", "listener.not_built", flags = klog::Hex(flags as u64), program = c.proc.name.as_str(); "EINVAL as a kernel without user notification (logged once)");
        }
        return -EINVAL;
    }
    // `seccomp_prepare_user_filter` / `seccomp_prepare_filter` / `bpf_prog_create_from_user`, in their order.
    let mut fp = [0u8; FPROG_SIZE];
    if c.proc.mm.copy_from_user(&mut fp, uargs).is_err() {
        return -EFAULT;
    }
    let len = u16::from_le_bytes([fp[0], fp[1]]) as usize;
    let ptr = u64::from_le_bytes(fp[8..16].try_into().unwrap_or([0; 8]));
    if len == 0 || len > MAX_INSNS {
        return -EINVAL;
    }
    let pr = c.thread.prctl();
    // `ns_capable_noaudit(current_user_ns(), CAP_SYS_ADMIN)`: also root of a user namespace (SX-2).
    if !pr.no_new_privs() && !proc::cred::own_capable(&c.proc.cred(), proc::cred::Cap::SysAdmin) {
        return -EACCES;
    }
    if ptr == 0 {
        return -EINVAL;
    }
    let mut bytes = Vec::new();
    if bytes.try_reserve_exact(len * INSN_SIZE).is_err() {
        return -ENOMEM;
    }
    bytes.resize(len * INSN_SIZE, 0);
    if c.proc.mm.copy_from_user(&mut bytes, ptr).is_err() {
        return -EFAULT;
    }
    let prog = match seccomp::load(&bytes) {
        Ok(p) => p,
        Err(r) => {
            klog::event!(Info, "seccomp", "filter.refused", pid = c.proc.pid, len = len, reason = r.what(), at = r.at().map_or(-1, |p| p as i64), program = c.proc.name.as_str());
            return -EINVAL;
        }
    };
    let tsync = flags & FLAG_TSYNC != 0;
    let me = pr.seccomp();
    let g = seccomp::change();
    match me.can_attach(&g, &prog) {
        Ok(()) => {}
        Err(AttachError::Mode) => return -EINVAL,
        Err(AttachError::TooLong) => return -ENOMEM,
    }
    if tsync {
        // `seccomp_can_sync_threads`: the first sibling that cannot take the stack is the answer.
        for t in c.thread.group().threads() {
            if !Arc::ptr_eq(&t, c.thread) && !t.prctl().seccomp().can_sync_from(me) {
                klog::event!(Info, "seccomp", "tsync.refused", pid = c.proc.pid, tid = c.thread.tid(), sibling = t.tid());
                return if flags & FLAG_TSYNC_ESRCH != 0 { -ESRCH } else { t.tid() as i64 };
            }
        }
    }
    let depth = match me.attach(&g, prog, flags & FLAG_LOG != 0) {
        Ok(d) => d,
        Err(AttachError::Mode) => return -EINVAL,
        Err(AttachError::TooLong) => return -ENOMEM,
    };
    ATTACHED.inc();
    let synced = if tsync { sync_threads(c, &g) } else { 0 };
    klog::event!(Info, "seccomp", "filter.attach", pid = c.proc.pid, tid = c.thread.tid(), len = len, depth = depth, flags = klog::Hex(flags as u64), synced = synced, program = c.proc.name.as_str());
    0
}

/// `seccomp_sync_threads`: every sibling takes the caller's stack and its no_new_privs.
fn sync_threads(c: &SysCtx<'_>, g: &seccomp::ChangeGuard) -> u32 {
    let me = c.thread.prctl().seccomp();
    let nnp = c.thread.prctl().no_new_privs();
    let mut synced = 0;
    for _ in 0..TSYNC_PASSES {
        let mut changed = false;
        for t in c.thread.group().threads() {
            if Arc::ptr_eq(&t, c.thread) {
                continue;
            }
            if nnp {
                t.prctl().set_no_new_privs();
            }
            let s = t.prctl().seccomp();
            if !s.synced_with(me) {
                s.sync_from(g, me);
                synced += 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    synced
}

/// The dispatcher's check, reached only when the thread has a mode: true when the call must not run (its
/// answer is set and the way out done, or the thread or process is ending).
#[inline(never)]
pub(crate) fn entry(p: &Arc<Process>, t: &Arc<Thread>, user: &mut UserContext, nr: usize, args: &[u64; 6]) -> bool {
    // `populate_seccomp_data`: the number as the low 32 bits (orig_ax as int), the native arch (V2 runs no
    // 32-bit code), the address after the `syscall` instruction.
    let d = Data { nr: user.regs.rax as u32, arch: AUDIT_ARCH_X86_64, ip: user.regs.rip, args: *args };
    let s = t.prctl().seccomp();
    match s.check(&d) {
        Verdict::Allow { log } => {
            if log {
                LOGGED.inc();
                klog::event!(Info, "seccomp", "log", pid = p.pid, tid = t.tid(), nr = d.nr, program = p.name.as_str());
            }
            false
        }
        Verdict::Errno { errno, action, log } => {
            ERRNOS.inc();
            if log {
                klog::event!(Info, "seccomp", "errno", pid = p.pid, tid = t.tid(), nr = d.nr, errno = errno, action = klog::Hex(action as u64));
            }
            let ret = -(errno as i64);
            user.set_syscall_return(ret);
            proc::deliver::syscall_exit(t, user, nr, ret);
            true
        }
        Verdict::Trap { data, log } => {
            TRAPS.inc();
            if log {
                klog::event!(Info, "seccomp", "trap", pid = p.pid, tid = t.tid(), nr = d.nr, data = data);
            }
            // `syscall_rollback`: the handler sees the call's registers, rax still the number.
            proc::deliver::force_seccomp_trap(t, data, d.ip, d.nr, d.arch);
            let rax = user.regs.rax as i64;
            proc::deliver::syscall_exit(t, user, nr, rax);
            true
        }
        Verdict::KillThread { data } => {
            kill(p, t, &d, if t.group().thread_count() > 1 { Kill::Thread(SIGSYS) } else { Kill::Process(SIGSYS) }, "kill_thread", data);
            true
        }
        Verdict::KillProcess { data } => {
            kill(p, t, &d, Kill::Process(SIGSYS), "kill_process", data);
            true
        }
        Verdict::StrictKill => {
            // `__secure_computing_strict`: do_exit(SIGKILL) of this thread.
            kill(p, t, &d, if t.group().thread_count() > 1 { Kill::Thread(SIGKILL) } else { Kill::Process(SIGKILL) }, "strict", 0);
            true
        }
    }
}

enum Kill {
    Thread(u32),
    Process(u32),
}

/// The kill actions: the thread runs no further call (MODE_DEAD); the process ends as by the signal (V2
/// writes no core dump, so the parent sees what Linux shows for a non-dumpable process), or the thread alone
/// ends (`do_exit(SIGSYS)`) while siblings live.
fn kill(p: &Process, t: &Thread, d: &Data, how: Kill, action: &'static str, data: u16) {
    KILLS.inc();
    t.prctl().seccomp().set_dead();
    let (sig, whole) = match how {
        Kill::Thread(s) => (s, false),
        Kill::Process(s) => (s, true),
    };
    klog::event!(Warn, "seccomp", "kill", pid = p.pid, tid = t.tid(), nr = d.nr, action = action, data = data, process = whole, program = p.name.as_str());
    if whole {
        t.group().kill(sig);
    } else {
        t.exit_thread(sig);
    }
}
