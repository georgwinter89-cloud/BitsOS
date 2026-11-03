//! The service's only unsafe code: no_new_privs and the seccomp filter, PR_SET_DUMPABLE, SO_PEERCRED of a
//! confirmer's socket, and (MC-11) the text console's ioctls and poll (libc wrappers; each block names what it
//! touches).

use bitsinstall::filter::Insn;
use std::os::fd::AsRawFd;

fn err(what: &str) -> String {
    let e = std::io::Error::last_os_error();
    format!("{what}: errno {}", e.raw_os_error().unwrap_or(0))
}

/// Not dumpable: no core file, and /proc/<pid>/mem and friends stay closed to others (Linux `SUID_DUMP_DISABLE`).
pub fn not_dumpable() -> Result<(), String> {
    // SAFETY: prctl with integer arguments only.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(err("prctl(PR_SET_DUMPABLE, 0)"));
    }
    Ok(())
}

/// no_new_privs, then the filter for this thread and every thread it starts (call while it has one thread).
pub fn seccomp(prog: &[Insn]) -> Result<(), String> {
    // SAFETY: prctl with integer arguments only.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(err("prctl(PR_SET_NO_NEW_PRIVS)"));
    }
    let filter: Vec<libc::sock_filter> = prog.iter().map(|i| libc::sock_filter { code: i.code, jt: i.jt, jf: i.jf, k: i.k }).collect();
    let fprog = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_ptr() as *mut libc::sock_filter };
    // SAFETY: seccomp(SET_MODE_FILTER) reads `fprog` and the `filter` array it points to; both outlive the call
    // (the kernel copies the program).
    if unsafe { libc::syscall(libc::SYS_seccomp, 1 as libc::c_long, 0 as libc::c_long, &fprog as *const libc::sock_fprog) } != 0 {
        return Err(err("seccomp(SECCOMP_SET_MODE_FILTER)"));
    }
    Ok(())
}

/// The peer of a connected Unix socket: (pid, uid, gid) as it was at connect (linux-6.6/net/unix/af_unix.c).
pub fn peer_cred(s: &impl AsRawFd) -> Result<(u32, u32, u32), String> {
    let mut c = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `c`, a local that outlives the call, and updates `len`.
    let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut c as *mut libc::ucred as *mut libc::c_void, &mut len) };
    if r != 0 || len as usize != std::mem::size_of::<libc::ucred>() {
        return Err(err("getsockopt(SO_PEERCRED)"));
    }
    Ok((c.pid as u32, c.uid, c.gid))
}

pub fn euid() -> u32 {
    // SAFETY: geteuid takes nothing and cannot fail.
    unsafe { libc::geteuid() }
}

/// For the filter probe: a fork-style clone, which the filter must answer by ending the process.
pub fn raw_fork() -> i64 {
    // SAFETY: clone with flags = SIGCHLD and a null stack is fork: the child leaves at once through _exit.
    let r = unsafe { libc::syscall(libc::SYS_clone, libc::SIGCHLD as libc::c_long, 0 as libc::c_long, 0 as libc::c_long, 0 as libc::c_long, 0 as libc::c_long) };
    if r == 0 {
        // SAFETY: the child ends without running destructors or flushing the parent's buffers.
        unsafe { libc::_exit(0) };
    }
    r
}

/// MC-11: an ioctl that fills `buf` (EVIOCGNAME, EVIOCGPHYS, EVIOCGID, EVIOCGBIT, KDGETMODE) -> its result, or the errno.
pub fn ioctl_read(fd: &impl AsRawFd, req: u64, buf: &mut [u8]) -> Result<i32, i32> {
    // SAFETY: every request passed here writes at most the size encoded in it, and callers encode `buf.len()`
    // (or pass a buffer at least as large as the request's fixed size: input_id 8 bytes, an int 4 bytes).
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), req as _, buf.as_mut_ptr() as *mut libc::c_void) };
    if r < 0 { Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0)) } else { Ok(r) }
}

/// MC-11: an ioctl that takes its argument by value (EVIOCGRAB 1/0).
pub fn ioctl_val(fd: &impl AsRawFd, req: u64, val: u64) -> Result<(), i32> {
    // SAFETY: the request reads no memory; the value is passed as the argument itself.
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), req as _, val as libc::c_ulong) };
    if r < 0 { Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0)) } else { Ok(()) }
}

/// MC-11 (fixer): an ioctl that reads an int argument from memory (EVIOC_BITSOS_PHYSICAL 1).
pub fn ioctl_int(fd: &impl AsRawFd, req: u64, val: i32) -> Result<(), i32> {
    let v = val;
    // SAFETY: the request reads the 4 bytes of `v`, a local that outlives the call, and writes nothing.
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), req as _, &v as *const i32) };
    if r < 0 { Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0)) } else { Ok(()) }
}

/// MC-11: which of `fds` are readable within `ms` (poll(2); EINTR counts as none).
pub fn poll_in(fds: &[i32], ms: i32) -> Vec<bool> {
    let mut p: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    // SAFETY: `p` is a live array of `p.len()` pollfd entries the kernel may write `revents` into.
    let r = unsafe { libc::poll(p.as_mut_ptr(), p.len() as libc::nfds_t, ms) };
    if r <= 0 {
        return vec![false; fds.len()];
    }
    p.iter().map(|x| x.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0).collect()
}
