//! Who-am-I and limits of wave A: getpid/gettid/getuid family (linux-6.6/kernel/sys.c), uname (sys.c
//! `newuname`), set_tid_address (kernel/fork.c), set_robust_list/get_robust_list (kernel/futex/syscalls.c),
//! getrlimit/setrlimit/prlimit64 (sys.c :1518, :1731, :1684; rules in `proc::rlimit`), getrandom
//! (drivers/char/random.c :1364), sched_yield (kernel/sched/core.c; the affinity calls are SC-6's `sched_sys`).
//!
//! AB-13: the credential calls (getuid family, set*id, setfs*id, get/setgroups) on `proc::cred`.

use crate::SysCtx;
use alloc::sync::Arc;
use proc::Process;
use proc::cred::{self, Cap, Cred};
use proc::rlimit::Rlimit;
use uapi::ab::random::*;
use uapi::ab::rlimit::RLIMIT_NOFILE;
use uapi::errno::*;

fn sys_getpid(c: &mut SysCtx<'_>) -> i64 {
    c.proc.pid as i64
}
crate::syscall!(GETPID, sys_getpid);

/// The calling thread's id (the main thread's is the pid).
fn sys_gettid(c: &mut SysCtx<'_>) -> i64 {
    c.thread.tid() as i64
}
crate::syscall!(GETTID, sys_gettid);

fn sys_getuid(c: &mut SysCtx<'_>) -> i64 {
    c.proc.cred().uid as i64
}
crate::syscall!(GETUID, sys_getuid);

fn sys_geteuid(c: &mut SysCtx<'_>) -> i64 {
    c.proc.cred().euid as i64
}
crate::syscall!(GETEUID, sys_geteuid);

fn sys_getgid(c: &mut SysCtx<'_>) -> i64 {
    c.proc.cred().gid as i64
}
crate::syscall!(GETGID, sys_getgid);

fn sys_getegid(c: &mut SysCtx<'_>) -> i64 {
    c.proc.cred().egid as i64
}
crate::syscall!(GETEGID, sys_getegid);

/// The 32-bit id argument `i` (`uid_t`; -1 = unchanged / invalid).
fn id(c: &SysCtx<'_>, i: usize) -> u32 {
    c.args[i] as u32
}

/// Runs one set*id rule of `proc::cred` and commits its result: 0 or the negated errno.
fn set(c: &SysCtx<'_>, what: &'static str, f: impl FnOnce(&Cred) -> cred::Change) -> i64 {
    match c.proc.change_cred(c.thread, f) {
        Ok(_) => 0,
        Err(e) => {
            klog::event!(Debug, "syscall", "setid.refused", pid = c.proc.pid, call = what, errno = e);
            -e
        }
    }
}

/// `setuid` and its family: linux-6.6/kernel/sys.c :610 `__sys_setuid`, :438 `__sys_setgid`, :529
/// `__sys_setreuid`, :372 `__sys_setregid`, :667 `__sys_setresuid`, :769 `__sys_setresgid` (the rules are in
/// `proc::cred`, host-tested there).
fn sys_setuid(c: &mut SysCtx<'_>) -> i64 {
    let u = id(c, 0);
    set(c, "setuid", |o| cred::set_uid(o, u))
}
crate::syscall!(SETUID, sys_setuid);

fn sys_setgid(c: &mut SysCtx<'_>) -> i64 {
    let g = id(c, 0);
    set(c, "setgid", |o| cred::set_gid(o, g))
}
crate::syscall!(SETGID, sys_setgid);

fn sys_setreuid(c: &mut SysCtx<'_>) -> i64 {
    let (r, e) = (id(c, 0), id(c, 1));
    set(c, "setreuid", |o| cred::set_reuid(o, r, e))
}
crate::syscall_nr!(uapi::ab::nr_more::SETREUID, "setreuid", sys_setreuid);

fn sys_setregid(c: &mut SysCtx<'_>) -> i64 {
    let (r, e) = (id(c, 0), id(c, 1));
    set(c, "setregid", |o| cred::set_regid(o, r, e))
}
crate::syscall_nr!(uapi::ab::nr_more::SETREGID, "setregid", sys_setregid);

fn sys_setresuid(c: &mut SysCtx<'_>) -> i64 {
    let (r, e, s) = (id(c, 0), id(c, 1), id(c, 2));
    set(c, "setresuid", |o| cred::set_resuid(o, r, e, s))
}
crate::syscall_nr!(uapi::ab::nr_more::SETRESUID, "setresuid", sys_setresuid);

fn sys_setresgid(c: &mut SysCtx<'_>) -> i64 {
    let (r, e, s) = (id(c, 0), id(c, 1), id(c, 2));
    set(c, "setresgid", |o| cred::set_resgid(o, r, e, s))
}
crate::syscall_nr!(uapi::ab::nr_more::SETRESGID, "setresgid", sys_setresgid);

/// `setfsuid`/`setfsgid` (sys.c :863, :907): the previous id, whether or not it changed.
fn sys_setfsuid(c: &mut SysCtx<'_>) -> i64 {
    let u = id(c, 0);
    c.proc.change_cred(c.thread, |o| Ok(cred::set_fsuid(o, u))).map_or_else(|_| c.proc.cred().fsuid, |old| old.fsuid) as i64
}
crate::syscall_nr!(uapi::ab::nr_more::SETFSUID, "setfsuid", sys_setfsuid);

fn sys_setfsgid(c: &mut SysCtx<'_>) -> i64 {
    let g = id(c, 0);
    c.proc.change_cred(c.thread, |o| Ok(cred::set_fsgid(o, g))).map_or_else(|_| c.proc.cred().fsgid, |old| old.fsgid) as i64
}
crate::syscall_nr!(uapi::ab::nr_more::SETFSGID, "setfsgid", sys_setfsgid);

/// `getresuid(ruid*, euid*, suid*)`: written one after the other, the first fault ends it (as `put_user`).
fn put3(c: &SysCtx<'_>, ptrs: [u64; 3], v: [u32; 3]) -> i64 {
    for (p, v) in ptrs.into_iter().zip(v) {
        if c.proc.mm.copy_to_user(p, &v.to_le_bytes()).is_err() {
            return -EFAULT;
        }
    }
    0
}

fn sys_getresuid(c: &mut SysCtx<'_>) -> i64 {
    let k = c.proc.cred();
    put3(c, [c.args[0], c.args[1], c.args[2]], [k.uid, k.euid, k.suid])
}
crate::syscall_nr!(uapi::ab::nr_more::GETRESUID, "getresuid", sys_getresuid);

fn sys_getresgid(c: &mut SysCtx<'_>) -> i64 {
    let k = c.proc.cred();
    put3(c, [c.args[0], c.args[1], c.args[2]], [k.gid, k.egid, k.sgid])
}
crate::syscall_nr!(uapi::ab::nr_more::GETRESGID, "getresgid", sys_getresgid);

/// `getgroups(size, list)` (linux-6.6/kernel/groups.c:161): size 0 asks for the count; a smaller size than the
/// count is EINVAL; the list comes sorted.
fn sys_getgroups(c: &mut SysCtx<'_>) -> i64 {
    let size = c.args[0] as i32;
    if size < 0 {
        return -EINVAL;
    }
    let k = c.proc.cred();
    let n = k.groups.len();
    if size != 0 {
        if n > size as usize {
            return -EINVAL;
        }
        // Up to NGROUPS_MAX ids (256 KiB): fallible, ENOMEM instead of an abort under memory pressure.
        let mut bytes = alloc::vec::Vec::new();
        if bytes.try_reserve_exact(n * 4).is_err() {
            return -ENOMEM;
        }
        bytes.extend(k.groups.iter().flat_map(|g| g.to_le_bytes()));
        if c.proc.mm.copy_to_user(c.args[1], &bytes).is_err() {
            return -EFAULT;
        }
    }
    n as i64
}
crate::syscall_nr!(uapi::ab::nr_more::GETGROUPS, "getgroups", sys_getgroups);

/// `setgroups(size, list)` (groups.c:198): EPERM without CAP_SETGID, EINVAL above NGROUPS_MAX (size as
/// unsigned, so negative too), ENOMEM when the list cannot be held, then each id read in order: EFAULT at the
/// first unreadable one, EINVAL at the first -1 (`groups_from_user`).
fn sys_setgroups(c: &mut SysCtx<'_>) -> i64 {
    if !c.proc.capable(Cap::Setgid) {
        return -EPERM;
    }
    let n = c.args[0] as i32 as u32 as usize;
    if n > cred::NGROUPS_MAX {
        return -EINVAL;
    }
    let mut raw = alloc::vec::Vec::new();
    if raw.try_reserve_exact(n * 4).is_err() {
        return -ENOMEM;
    }
    raw.resize(n * 4, 0u8);
    // One copy; on a fault the ids before it decide between EINVAL and EFAULT, as Linux's loop does.
    let copied = if c.proc.mm.copy_from_user(&mut raw, c.args[1]).is_ok() { n } else { readable_ids(c, &mut raw, n) };
    let mut groups = alloc::vec::Vec::new();
    if groups.try_reserve_exact(n).is_err() {
        return -ENOMEM;
    }
    for ch in raw[..copied * 4].chunks_exact(4) {
        let g = u32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]]);
        if g == cred::NO_ID {
            return -EINVAL;
        }
        groups.push(g);
    }
    if copied < n {
        return -EFAULT;
    }
    let groups = cred::sorted_groups(groups);
    set(c, "setgroups", |o| cred::set_groups(o, groups))
}
crate::syscall_nr!(uapi::ab::nr_more::SETGROUPS, "setgroups", sys_setgroups);

/// How many of the `n` ids at args[1] can be read one by one (into `raw`), after a whole-list copy faulted.
fn readable_ids(c: &SysCtx<'_>, raw: &mut [u8], n: usize) -> usize {
    (0..n).take_while(|&i| c.proc.mm.copy_from_user(&mut raw[i * 4..i * 4 + 4], c.args[1].wrapping_add(i as u64 * 4)).is_ok()).count()
}

/// The release V2 reports: Linux 6.6 is the reference its semantics follow (`ref/linux-6.6`).
pub(crate) const RELEASE: &str = "6.6.0-bitsos-v2";
pub(crate) const VERSION: &str = "#1 SMP PREEMPT_DYNAMIC BitsOS V2";

/// `struct new_utsname`: six fields of 65 bytes, NUL-padded; "Linux" as sysname (programs compare it),
/// the host and domain names from `uts` (defaults "bitsos" and Linux's "(none)").
pub(crate) fn utsname() -> [u8; 390] {
    crate::uts::bytes()
}

fn sys_uname(c: &mut SysCtx<'_>) -> i64 {
    c.proc.mm.copy_to_user(c.args[0], &utsname()).map(|_| 0).unwrap_or(-EFAULT)
}
crate::syscall!(UNAME, sys_uname);

/// `set_tid_address`: the calling thread's clear-child-tid word (zeroed and futex-woken at its end, `proc`);
/// returns the thread's id.
fn sys_set_tid_address(c: &mut SysCtx<'_>) -> i64 {
    c.thread.clear_child_tid.store(c.args[0], core::sync::atomic::Ordering::Release);
    c.thread.tid() as i64
}
crate::syscall!(SET_TID_ADDRESS, sys_set_tid_address);

/// `sizeof(struct robust_list_head)` on x86-64.
const ROBUST_HEAD_SIZE: u64 = 24;

fn sys_set_robust_list(c: &mut SysCtx<'_>) -> i64 {
    if c.args[1] != ROBUST_HEAD_SIZE {
        return -EINVAL;
    }
    c.thread.robust_list.store(c.args[0], core::sync::atomic::Ordering::Release);
    0
}
crate::syscall!(SET_ROBUST_LIST, sys_set_robust_list);

/// `check_prlimit_permission` (linux-6.6/kernel/sys.c:1662) without the `current == task` case: the caller's
/// real uid equals the target's uid, euid and suid and its real gid the target's gid, egid and sgid, or the
/// caller has CAP_SYS_RESOURCE. Not the kill rule (review AB-5 finding 7).
pub(crate) fn may_prlimit(me: &Cred, t: &Cred) -> bool {
    let ids = [t.uid, t.euid, t.suid].iter().all(|&x| x == me.uid) && [t.gid, t.egid, t.sgid].iter().all(|&x| x == me.gid);
    ids || cred::capable(me, Cap::SysResource)
}

/// Process `pid` as the target of a query or change (0 = the caller; a thread id names its process, as
/// `find_task_by_vpid`): ESRCH when there is none, EPERM unless `may_prlimit`. Since UL-2 init runs as uid 0
/// (review INT-A1d user 4).
fn target(c: &SysCtx<'_>, pid: i32) -> Result<Arc<Process>, i64> {
    if pid == 0 || pid as u32 == c.proc.pid {
        return Ok(c.proc.clone());
    }
    if pid < 0 {
        return Err(-ESRCH);
    }
    let t = proc::find(pid as u32).or_else(|| proc::find_thread(pid as u32).and_then(|t| t.group().image())).ok_or(-ESRCH)?;
    if !may_prlimit(&c.proc.cred(), &t.cred()) {
        return Err(-EPERM);
    }
    Ok(t)
}

/// `get_robust_list(tid, head, len)`: the head of thread `tid` (0 = the caller); EPERM as `ptrace_may_access`
/// READ_REALCREDS (`Process::may_ptrace`).
fn sys_get_robust_list(c: &mut SysCtx<'_>) -> i64 {
    let [pid, head_ptr, len_ptr, ..] = c.args;
    let head = if pid as i32 == 0 {
        c.thread.robust_list.load(core::sync::atomic::Ordering::Acquire)
    } else {
        let tid = pid as i32;
        let Some(t) = (tid > 0).then(|| proc::find_thread(tid as u32)).flatten() else { return -ESRCH };
        let Some(owner) = t.group().image() else { return -ESRCH };
        if !c.proc.may_ptrace(&owner, false) {
            return -EPERM;
        }
        t.robust_list.load(core::sync::atomic::Ordering::Acquire)
    };
    // Linux writes the length first, then the head.
    if c.proc.mm.write_u64(len_ptr, ROBUST_HEAD_SIZE).is_err() {
        return -EFAULT;
    }
    c.proc.mm.write_u64(head_ptr, head).map(|_| 0).unwrap_or(-EFAULT)
}
crate::syscall!(GET_ROBUST_LIST, sys_get_robust_list);

fn get_rlimit(c: &SysCtx<'_>, addr: u64) -> Result<Rlimit, i64> {
    let mut b = [0u8; 16];
    c.proc.mm.copy_from_user(&mut b, addr).map_err(|_| -EFAULT)?;
    Ok(Rlimit::from_bytes(&b))
}

/// `do_prlimit` on `t` plus what acts on a change: RLIMIT_NOFILE sets the fd table's limit (FS). `privileged`
/// is the caller's CAP_SYS_RESOURCE (uid 0), which may raise a hard limit; not the target's.
fn prlimit(t: &Process, resource: u32, new: Option<Rlimit>, privileged: bool) -> Result<Rlimit, i64> {
    let old = t.limits.prlimit(resource, new, vfs::abi::NR_OPEN as u64, privileged).map_err(|e| -e)?;
    if let Some(n) = new
        && resource == RLIMIT_NOFILE
    {
        t.fds.set_limit(n.cur.min(vfs::abi::NR_OPEN as u64) as usize);
    }
    Ok(old)
}

fn sys_prlimit64(c: &mut SysCtx<'_>) -> i64 {
    let [pid, resource, new, old, ..] = c.args;
    let new = if new != 0 {
        match get_rlimit(c, new) {
            Ok(r) => Some(r),
            Err(e) => return e,
        }
    } else {
        None
    };
    let t = match target(c, pid as i32) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let prev = match prlimit(&t, resource as u32, new, c.proc.capable(Cap::SysResource)) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if old != 0 && c.proc.mm.copy_to_user(old, &prev.to_bytes()).is_err() {
        return -EFAULT;
    }
    0
}
crate::syscall!(PRLIMIT64, sys_prlimit64);

fn sys_getrlimit(c: &mut SysCtx<'_>) -> i64 {
    match prlimit(c.proc, c.args[0] as u32, None, c.proc.capable(Cap::SysResource)) {
        Ok(r) => c.proc.mm.copy_to_user(c.args[1], &r.to_bytes()).map(|_| 0).unwrap_or(-EFAULT),
        Err(e) => e,
    }
}
crate::syscall!(GETRLIMIT, sys_getrlimit);

fn sys_setrlimit(c: &mut SysCtx<'_>) -> i64 {
    let new = match get_rlimit(c, c.args[1]) {
        Ok(r) => r,
        Err(e) => return e,
    };
    prlimit(c.proc, c.args[0] as u32, Some(new), c.proc.capable(Cap::SysResource)).map(|_| 0).unwrap_or_else(|e| e)
}
crate::syscall!(SETRLIMIT, sys_setrlimit);

/// `MAX_RW_COUNT` (INT_MAX & PAGE_MASK): the most one read/write-like call moves.
const MAX_RW_COUNT: u64 = 0x7fff_f000;

/// x86-64 `access_ok` of 6.6: the end must not wrap and must stay below 2^63 (the copy itself then finds
/// unmapped or kernel pages).
pub(crate) fn access_ok(addr: u64, len: u64) -> bool {
    addr.checked_add(len).is_some_and(|end| (end as i64) >= 0)
}

/// getrandom: FS-2's ChaCha20 generator (always seeded, so GRND_NONBLOCK never sees EAGAIN). Bytes go out in
/// chunks; a fault after some bytes returns those, a kill ends it early (checked every 16 chunks, about a
/// page; Linux checks `signal_pending` once per page).
fn sys_getrandom(c: &mut SysCtx<'_>) -> i64 {
    let [buf, len, flags, ..] = c.args;
    let flags = flags as u32 as u64;
    if flags & !(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE) != 0 || flags & (GRND_INSECURE | GRND_RANDOM) == (GRND_INSECURE | GRND_RANDOM) {
        return -EINVAL;
    }
    let len = len.min(MAX_RW_COUNT);
    if !access_ok(buf, len) {
        return -EFAULT;
    }
    let mut chunk = [0u8; 256];
    let mut done: u64 = 0;
    let mut rounds = 0u32;
    while done < len {
        // Chunks never cross a page edge, so a fault stops exactly at the first unwritable byte (Linux's
        // copy_to_iter answers the bytes before it).
        let to_edge = 4096 - ((buf + done) & 4095);
        let n = (len - done).min(chunk.len() as u64).min(to_edge) as usize;
        proc::random_bytes(&mut chunk[..n]);
        if c.proc.mm.copy_to_user(buf + done, &chunk[..n]).is_err() {
            chunk.fill(0);
            return if done > 0 { done as i64 } else { -EFAULT };
        }
        done += n as u64;
        rounds += 1;
        if rounds % 16 == 0 && done < len {
            if c.proc.has_exited() {
                break;
            }
            sched::cond_resched();
        }
    }
    chunk.fill(0);
    done as i64
}
crate::syscall!(GETRANDOM, sys_getrandom);

fn sys_sched_yield(_c: &mut SysCtx<'_>) -> i64 {
    sched::yield_now();
    0
}
crate::syscall!(SCHED_YIELD, sys_sched_yield);

// sched_getaffinity moved to `sched_sys` with SC-6 (with sched_setaffinity, real masks per thread).

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utsname_fields() {
        let u = utsname();
        let field = |i: usize| {
            let f = &u[i * 65..(i + 1) * 65];
            alloc::string::String::from(core::str::from_utf8(&f[..f.iter().position(|&b| b == 0).unwrap()]).unwrap())
        };
        assert_eq!(field(0), "Linux");
        assert_eq!(field(2), RELEASE);
        assert_eq!(field(4), "x86_64");
        assert_eq!(field(5), "(none)");
        assert!(RELEASE.as_bytes()[0].is_ascii_digit());
    }

    #[test]
    fn access_ok_as_x86_64() {
        assert!(access_ok(0x1000, 16));
        assert!(access_ok(0, 0));
        // Linux tests the end itself (`(long)(ptr + size) >= 0`), so an end of exactly 2^63 is refused.
        assert!(access_ok(0x7fff_ffff_ffff_fff0, 15));
        assert!(!access_ok(0x7fff_ffff_ffff_fff0, 16));
        assert!(!access_ok(0xffff_8880_0000_0000, 1));
        assert!(!access_ok(u64::MAX, 2));
    }

    #[test]
    fn prlimit_permission_as_check_prlimit_permission() {
        let k = Cred::new;
        assert!(may_prlimit(&k(1000, 1000), &k(1000, 1000)), "same uid and gid");
        assert!(!may_prlimit(&k(1000, 1000), &k(1000, 100)), "same uid, other gid: EPERM (the kill rule allowed it)");
        assert!(!may_prlimit(&k(1000, 1000), &k(1001, 1000)), "other uid");
        assert!(may_prlimit(&k(0, 0), &k(1000, 1000)), "CAP_SYS_RESOURCE (root)");
        assert!(!may_prlimit(&k(1000, 1000), &k(0, 0)), "a user on root's process");
        let saved_root = Cred { suid: 0, ..k(1000, 1000) };
        assert!(!may_prlimit(&k(1000, 1000), &saved_root), "the target's saved uid counts");
    }
}
