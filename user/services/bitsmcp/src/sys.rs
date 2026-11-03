//! The service's only unsafe code: giving root up, no_new_privs, the seccomp filter and the attempt at a private
//! mount view (libc wrappers; each block names what it touches). Called while bitsmcp has one thread.

use bitsmcp::filter::Insn;

fn err(what: &str) -> String {
    let e = std::io::Error::last_os_error();
    format!("{what}: errno {}", e.raw_os_error().unwrap_or(0))
}

/// A mount view of its own (Linux `unshare(CLONE_NEWNS)`, then everything private below /). Err while V2 has no
/// mount namespaces (SX-2); the seccomp filter keeps the service off the file system either way.
pub fn private_mounts() -> Result<(), String> {
    // SAFETY: unshare takes a flag word, no memory.
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        return Err(err("unshare(CLONE_NEWNS)"));
    }
    let root = c"/";
    // SAFETY: mount reads the NUL-terminated path above (it outlives the call); the other pointers are null.
    if unsafe { libc::mount(std::ptr::null(), root.as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()) } != 0 {
        return Err(err("mount(/, MS_REC|MS_PRIVATE)"));
    }
    Ok(())
}

/// Gives root up for `(uid, gid)` as chpst does: supplementary groups = [gid], all three gids, all three uids;
/// checked afterwards, and that root cannot come back (linux-6.6/kernel/sys.c `__sys_setresuid`).
pub fn drop_to(uid: u32, gid: u32) -> Result<(), String> {
    let groups = [gid as libc::gid_t];
    // SAFETY: setgroups reads one gid from `groups`, which outlives the call.
    if unsafe { libc::setgroups(1, groups.as_ptr()) } != 0 {
        return Err(err("setgroups"));
    }
    // SAFETY: plain id changes, no memory is passed.
    if unsafe { libc::setresgid(gid, gid, gid) } != 0 {
        return Err(err("setresgid"));
    }
    // SAFETY: as above.
    if unsafe { libc::setresuid(uid, uid, uid) } != 0 {
        return Err(err("setresuid"));
    }
    let (mut r, mut e, mut s) = (0u32, 0u32, 0u32);
    // SAFETY: getresuid writes three u32s into locals that outlive it.
    unsafe { libc::getresuid(&mut r, &mut e, &mut s) };
    if [r, e, s] != [uid; 3] {
        return Err(format!("uids after the drop are {:?}", [r, e, s]));
    }
    // SAFETY: as setresuid; this must fail now (no saved root id).
    if uid != 0 && unsafe { libc::setuid(0) } == 0 {
        return Err("setuid(0) still works after the drop".into());
    }
    Ok(())
}

/// no_new_privs, then the filter for this thread and every thread it starts.
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

/// The effective uid (the filter probe gives root up only when it has it).
pub fn euid() -> u32 {
    // SAFETY: geteuid takes nothing and cannot fail.
    unsafe { libc::geteuid() }
}

/// For the filter probe: clone(2) as fork does (SIGCHLD, no CLONE_THREAD), which the filter must answer by
/// ending the process. Returns what clone returned if the filter let it through.
pub fn raw_fork() -> i64 {
    // SAFETY: clone with flags = SIGCHLD and a null stack is fork: the child runs on a copy of this stack and
    // leaves at once through _exit below, touching nothing.
    let r = unsafe { libc::syscall(libc::SYS_clone, libc::SIGCHLD as libc::c_long, 0 as libc::c_long, 0 as libc::c_long, 0 as libc::c_long, 0 as libc::c_long) };
    if r == 0 {
        // SAFETY: the child ends without running destructors or flushing the parent's buffers.
        unsafe { libc::_exit(0) };
    }
    r
}
