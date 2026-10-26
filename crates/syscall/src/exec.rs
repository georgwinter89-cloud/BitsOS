//! execve and execveat: the path and the argument strings are copied from the caller with Linux's rules and
//! in its order (linux-6.6/fs/exec.c `do_execveat_common`, `count`, `bprm_stack_limits`, `copy_strings`:
//! the name, then envp and argv, each from its last string to its first), then `proc::execve` runs the file.

use crate::SysCtx;
use alloc::vec::Vec;
use uapi::ab::exec::MAX_ARG_STRLEN;
use uapi::ab::rlimit::RLIMIT_STACK;
use uapi::errno::*;
use vfs::abi::{AT_EMPTY_PATH, AT_FDCWD, PATH_MAX};

/// `_STK_LIM / 4 * 3` and `ARG_MAX` (32 pages) of `bprm_stack_limits`.
#[cfg(test)]
use uapi::ab::exec::STK_LIM_3_4;
#[cfg(test)]
use uapi::ab::exec::ARG_MAX;

/// How many bytes argv + envp + the file name may take (pointers already taken off), or E2BIG (pure,
/// host-tested; `bprm_stack_limits`).
pub(crate) fn arg_space(rlim_stack: u64, argc: u64, envc: u64) -> Result<u64, i64> {
    let limit = uapi::ab::exec::arg_limit(rlim_stack);
    let ptrs = (argc.max(1) + envc) * 8;
    if limit <= ptrs {
        return Err(-E2BIG);
    }
    Ok(limit - ptrs)
}

/// Counts a NULL-terminated pointer array (`count`): EFAULT on a bad pointer, E2BIG past MAX_ARG_STRINGS.
/// The pointers are read again by `strings`, as Linux's `copy_strings` does.
fn count(c: &SysCtx<'_>, arr: u64) -> Result<u64, i64> {
    if arr == 0 {
        return Ok(0);
    }
    let mut n = 0u64;
    loop {
        let at = arr.checked_add(n * 8).ok_or(-EFAULT)?;
        if c.proc.mm.read_u64(at).map_err(|_| -EFAULT)? == 0 {
            return Ok(n);
        }
        if n >= i32::MAX as u64 {
            return Err(-E2BIG);
        }
        n += 1;
        if n % 4096 == 0 {
            if c.proc.has_exited() {
                return Err(-EINTR);
            }
            sched::cond_resched();
        }
    }
}

/// One argument string (`strnlen_user` + `valid_arg_len` + the copy): EFAULT, or E2BIG beyond
/// MAX_ARG_STRLEN with its NUL.
fn arg_string(c: &SysCtx<'_>, at: u64) -> Result<Vec<u8>, i64> {
    let mut out: Vec<u8> = Vec::new();
    let mut a = at;
    while out.len() < MAX_ARG_STRLEN {
        let n = ((4096 - (a & 4095)) as usize).min(MAX_ARG_STRLEN - out.len());
        let old = out.len();
        out.try_reserve(n).map_err(|_| -ENOMEM)?;
        out.resize(old + n, 0);
        c.proc.mm.copy_from_user(&mut out[old..], a).map_err(|_| -EFAULT)?;
        if let Some(i) = out[old..].iter().position(|&b| b == 0) {
            out.truncate(old + i);
            return Ok(out);
        }
        a = a.checked_add(n as u64).ok_or(-EFAULT)?;
    }
    Err(-E2BIG)
}

/// Copies the `n` strings of the pointer array at `arr` (last first, as Linux), charging `space`.
fn strings(c: &SysCtx<'_>, arr: u64, n: u64, space: &mut u64) -> Result<proc::ArgList, i64> {
    let mut v = proc::ArgList::new();
    for i in (0..n).rev() {
        let p = c.proc.mm.read_u64(arr + i * 8).map_err(|_| -EFAULT)?;
        let s = arg_string(c, p)?;
        let len = s.len() as u64 + 1;
        if len > *space {
            return Err(-E2BIG);
        }
        *space -= len;
        v.push(&s).map_err(|e| -e)?;
    }
    v.reversed().map_err(|e| -e)
}

fn exec(c: &mut SysCtx<'_>, dirfd: i32, path: u64, argv: u64, envp: u64, flags: u64) -> i64 {
    let mut name = Vec::new();
    match hal::usercopy::strncpy_from_user(c.proc.mm.root(), path, &mut name, PATH_MAX) {
        Ok(_) => {}
        Err(hal::usercopy::StrError::Fault) => return -EFAULT,
        Err(hal::usercopy::StrError::TooLong) => return -ENAMETOOLONG,
    }
    if name.is_empty() && flags & AT_EMPTY_PATH == 0 {
        return -ENOENT;
    }
    let r = (|| -> Result<proc::ExecRequest, i64> {
        let argc = count(c, argv)?;
        let envc = count(c, envp)?;
        let rlim = c.proc.limits.get(RLIMIT_STACK).map_or(uapi::ab::exec::STACK_RLIMIT, |l| l.cur);
        let mut space = arg_space(rlim, argc, envc)?;
        // bprm->filename as the kernel builds it (/dev/fd/N/... for a relative execveat).
        let fname_len = if name.first() == Some(&b'/') || dirfd == AT_FDCWD {
            name.len()
        } else {
            alloc::format!("/dev/fd/{dirfd}/").len() + name.len() - name.is_empty() as usize
        };
        space = space.checked_sub(fname_len as u64 + 1).ok_or(-E2BIG)?;
        let envs = strings(c, envp, envc, &mut space)?;
        let mut args = strings(c, argv, argc, &mut space)?;
        if args.is_empty() {
            // Linux 6.6: a NULL or empty argv gets "" as argv[0].
            args.push(b"").map_err(|e| -e)?;
        }
        Ok(proc::ExecRequest { dirfd, path: name.clone(), flags, argv: args, envp: envs })
    })();
    let r = match r {
        Ok(r) => r,
        Err(e) => return e,
    };
    match proc::execve(c.proc, c.thread, c.user, &r) {
        Ok(()) => 0,
        Err(e) => {
            klog::event!(Debug, "syscall", "exec.failed", pid = c.proc.pid, path = alloc::string::String::from_utf8_lossy(&r.path).as_ref(), errno = e);
            -e
        }
    }
}

fn sys_execve(c: &mut SysCtx<'_>) -> i64 {
    let [path, argv, envp, ..] = c.args;
    exec(c, AT_FDCWD, path, argv, envp, 0)
}
crate::syscall!(EXECVE, sys_execve);

fn sys_execveat(c: &mut SysCtx<'_>) -> i64 {
    let [dirfd, path, argv, envp, flags, _] = c.args;
    exec(c, dirfd as i32, path, argv, envp, flags as u32 as u64)
}
crate::syscall_nr!(uapi::ab::nr_more::EXECVEAT, "execveat", sys_execveat);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_space_like_bprm_stack_limits() {
        // 8 MiB stack: a quarter (2 MiB) minus the pointers.
        assert_eq!(arg_space(8 << 20, 3, 2), Ok((2 << 20) - 5 * 8));
        // argc 0 counts as 1 (the "" Linux adds).
        assert_eq!(arg_space(8 << 20, 0, 0), Ok((2 << 20) - 8));
        // A huge stack limit: 3/4 of _STK_LIM; a tiny one: ARG_MAX.
        assert_eq!(arg_space(u64::MAX, 1, 0), Ok(STK_LIM_3_4 - 8));
        assert_eq!(arg_space(4096, 1, 0), Ok(ARG_MAX - 8));
        assert_eq!(arg_space(8 << 20, 1 << 20, 0), Err(-E2BIG));
    }
}
