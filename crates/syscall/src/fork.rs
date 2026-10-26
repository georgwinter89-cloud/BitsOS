//! clone, clone3, fork and vfork (linux-6.6/kernel/fork.c: `SYSCALL_DEFINE5(clone)` :3036 in the x86-64
//! argument order, `copy_clone_args_from_user` :3058, `clone3_args_valid` :3155); the work is `proc::clone`.

use crate::SysCtx;
use proc::CloneArgs;
use uapi::ab::clone::*;
use uapi::errno::*;

fn run(c: &mut SysCtx<'_>, a: &CloneArgs) -> i64 {
    proc::clone(c.proc, c.thread, c.user, a).map(|t| t as i64).unwrap_or_else(|e| -e)
}

/// `clone(flags, newsp, parent_tid, child_tid, tls)`: the low byte of the flags is the exit signal, the upper 32
/// bits are ignored (`lower_32_bits`). For CLONE_PIDFD the pidfd goes to `parent_tid`.
fn sys_clone(c: &mut SysCtx<'_>) -> i64 {
    let [flags, newsp, ptid, ctid, tls, _] = c.args;
    let low = flags & 0xffff_ffff;
    let a = CloneArgs { flags: low & !CSIGNAL, exit_signal: (low & CSIGNAL) as u32, stack: newsp, parent_tid: ptid, child_tid: ctid, tls, pidfd: ptid, ..Default::default() };
    run(c, &a)
}
crate::syscall!(CLONE, sys_clone);

fn sys_fork(c: &mut SysCtx<'_>) -> i64 {
    run(c, &CloneArgs { exit_signal: uapi::ab::signal::SIGCHLD, ..Default::default() })
}
crate::syscall!(FORK, sys_fork);

fn sys_vfork(c: &mut SysCtx<'_>) -> i64 {
    run(c, &CloneArgs { flags: CLONE_VFORK | CLONE_VM, exit_signal: uapi::ab::signal::SIGCHLD, ..Default::default() })
}
crate::syscall!(VFORK, sys_vfork);

/// `struct clone_args` (88 bytes in VER2): flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size,
/// tls, set_tid, set_tid_size, cgroup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Clone3 {
    pub flags: u64,
    pub pidfd: u64,
    pub child_tid: u64,
    pub parent_tid: u64,
    pub exit_signal: u64,
    pub stack: u64,
    pub stack_size: u64,
    pub tls: u64,
    pub set_tid: u64,
    pub set_tid_size: u64,
    pub cgroup: u64,
}

impl Clone3 {
    fn from_words(w: &[u64; 11]) -> Self {
        Clone3 { flags: w[0], pidfd: w[1], child_tid: w[2], parent_tid: w[3], exit_signal: w[4], stack: w[5], stack_size: w[6], tls: w[7], set_tid: w[8], set_tid_size: w[9], cgroup: w[10] }
    }
}

/// `copy_clone_args_from_user`'s checks after the copy (pure, host-tested). `size` is the caller's struct size;
/// `tail_zero` whether every byte beyond our 88 is zero (`copy_struct_from_user`: E2BIG otherwise).
pub(crate) fn clone3_copy_checks(k: &Clone3, size: u64, tail_zero: bool) -> Result<(), i64> {
    if size > 4096 {
        return Err(-E2BIG);
    }
    if size < CLONE_ARGS_SIZE_VER0 {
        return Err(-EINVAL);
    }
    if !tail_zero {
        return Err(-E2BIG);
    }
    if k.set_tid_size > MAX_PID_NS_LEVEL || (k.set_tid == 0 && k.set_tid_size > 0) || (k.set_tid != 0 && k.set_tid_size == 0) {
        return Err(-EINVAL);
    }
    if k.exit_signal & !CSIGNAL != 0 || k.exit_signal > 64 {
        return Err(-EINVAL);
    }
    if k.flags & CLONE_INTO_CGROUP != 0 && (k.cgroup > i32::MAX as u64 || size < CLONE_ARGS_SIZE_VER2) {
        return Err(-EINVAL);
    }
    Ok(())
}

/// `clone3_args_valid` (pure, host-tested): the args as `CloneArgs`, or EINVAL. `set_tid`: the first set_tid
/// entry when one was given (proc checks it where Linux's `alloc_pid` does).
pub(crate) fn clone3_args(k: &Clone3, set_tid: Option<i32>) -> Result<CloneArgs, i64> {
    if k.flags & !(CLONE_LEGACY_FLAGS | CLONE_CLEAR_SIGHAND | CLONE_INTO_CGROUP) != 0 {
        return Err(-EINVAL);
    }
    if k.flags & (CLONE_DETACHED | (CSIGNAL & !CLONE_NEWTIME)) != 0 {
        return Err(-EINVAL);
    }
    if k.flags & (CLONE_SIGHAND | CLONE_CLEAR_SIGHAND) == CLONE_SIGHAND | CLONE_CLEAR_SIGHAND {
        return Err(-EINVAL);
    }
    if k.flags & (CLONE_THREAD | CLONE_PARENT) != 0 && k.exit_signal != 0 {
        return Err(-EINVAL);
    }
    let stack = match (k.stack, k.stack_size) {
        (0, 0) => 0,
        (0, _) | (_, 0) => return Err(-EINVAL),
        // access_ok of the stack range; the stack grows down from its end.
        (s, n) => s.checked_add(n).filter(|&e| e <= hal::USER_TOP).ok_or(-EINVAL)?,
    };
    Ok(CloneArgs {
        flags: k.flags,
        exit_signal: k.exit_signal as u32,
        stack,
        parent_tid: k.parent_tid,
        child_tid: k.child_tid,
        tls: k.tls,
        pidfd: k.pidfd,
        set_tid,
        set_tid_count: k.set_tid_size,
    })
}

/// `clone3(uargs, size)`: glibc 2.34+ tries it first for threads and posix_spawn (and falls back to clone on
/// ENOSYS); V2 implements it with the same rules as clone.
fn sys_clone3(c: &mut SysCtx<'_>) -> i64 {
    let [uargs, size, ..] = c.args;
    if size > 4096 {
        return -E2BIG;
    }
    if size < CLONE_ARGS_SIZE_VER0 {
        return -EINVAL;
    }
    let mut b = [0u8; 4096];
    let n = size as usize;
    if c.proc.mm.copy_from_user(&mut b[..n], uargs).is_err() {
        return -EFAULT;
    }
    let mut w = [0u64; 11];
    for (i, x) in w.iter_mut().enumerate() {
        let o = i * 8;
        if o + 8 <= n {
            *x = u64::from_le_bytes(b[o..o + 8].try_into().unwrap_or_default());
        }
    }
    let k = Clone3::from_words(&w);
    let tail_zero = n <= 88 || b[88..n].iter().all(|&x| x == 0);
    if let Err(e) = clone3_copy_checks(&k, size, tail_zero) {
        return e;
    }
    let mut set_tid = None;
    if k.set_tid != 0 {
        let mut t = [0u8; 4];
        if c.proc.mm.copy_from_user(&mut t, k.set_tid).is_err() {
            return -EFAULT;
        }
        set_tid = Some(i32::from_le_bytes(t));
    }
    match clone3_args(&k, set_tid) {
        Ok(a) => run(c, &a),
        Err(e) => e,
    }
}
crate::syscall!(CLONE3, sys_clone3);

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(flags: u64) -> Clone3 {
        Clone3 { flags, ..Default::default() }
    }

    #[test]
    fn clone3_rules_like_linux() {
        let cc = |k: &Clone3, size: u64, tail: bool| clone3_copy_checks(k, size, tail);
        assert_eq!(cc(&ok(0), 63, true), Err(-EINVAL), "below VER0");
        assert_eq!(cc(&ok(0), 4097, true), Err(-E2BIG));
        assert_eq!(cc(&ok(0), 96, false), Err(-E2BIG), "unknown non-zero tail");
        assert!(cc(&ok(0), 96, true).is_ok(), "a zero tail is fine");
        assert_eq!(cc(&Clone3 { exit_signal: 65, ..Default::default() }, 88, true), Err(-EINVAL));
        assert_eq!(cc(&Clone3 { exit_signal: 1 << 32, ..Default::default() }, 88, true), Err(-EINVAL));
        assert_eq!(cc(&Clone3 { set_tid_size: 1, ..Default::default() }, 88, true), Err(-EINVAL));
        assert_eq!(cc(&Clone3 { set_tid: 0x1000, ..Default::default() }, 88, true), Err(-EINVAL));
        assert_eq!(cc(&Clone3 { set_tid: 0x1000, set_tid_size: 33, ..Default::default() }, 88, true), Err(-EINVAL));
        assert_eq!(cc(&Clone3 { flags: CLONE_INTO_CGROUP, ..Default::default() }, 80, true), Err(-EINVAL), "cgroup needs VER2");
        assert_eq!(clone3_args(&ok(CLONE_DETACHED), None), Err(-EINVAL));
        assert_eq!(clone3_args(&ok(17), None), Err(-EINVAL), "CSIGNAL bits in flags");
        assert_eq!(clone3_args(&ok(1 << 40), None), Err(-EINVAL), "unknown flag");
        assert_eq!(clone3_args(&ok(CLONE_SIGHAND | CLONE_VM | CLONE_CLEAR_SIGHAND), None), Err(-EINVAL));
        let thread_sig = Clone3 { flags: CLONE_THREAD | CLONE_SIGHAND | CLONE_VM, exit_signal: 17, ..Default::default() };
        assert_eq!(clone3_args(&thread_sig, None), Err(-EINVAL), "threads have no exit signal");
        assert_eq!(clone3_args(&Clone3 { stack: 0x1000, ..Default::default() }, None), Err(-EINVAL), "stack without size");
        assert_eq!(clone3_args(&Clone3 { stack_size: 0x1000, ..Default::default() }, None), Err(-EINVAL));
        let a = clone3_args(&Clone3 { stack: 0x10_0000, stack_size: 0x8000, exit_signal: 17, ..Default::default() }, None).unwrap();
        assert_eq!((a.stack, a.exit_signal), (0x10_8000, 17), "the stack pointer is the end");
        // AB-12: CLONE_PIDFD passes clone3's checks and carries its pointer (copy_process decides the rest).
        let a = clone3_args(&Clone3 { flags: CLONE_PIDFD, pidfd: 0x7000, exit_signal: 17, ..Default::default() }, None).unwrap();
        assert_eq!((a.flags, a.pidfd), (CLONE_PIDFD, 0x7000));
    }
}
