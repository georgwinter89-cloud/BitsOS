//! L7-PROC: the pidfs ioctls (linux-7.3.1/fs/pidfs.c:499 `pidfs_ioctl_valid`, :530 `pidfd_ioctl`, :365
//! `pidfd_info`): PIDFD_GET_INFO with the caller's struct size, the namespace fds PIDFD_GET_*_NAMESPACE and
//! FS_IOC_GETVERSION. V2 has no cgroups: PIDFD_INFO_CGROUPID is never set (a kernel without CONFIG_CGROUPS
//! answers so), and no cgroup or time namespaces (EOPNOTSUPP, as such a kernel). Core dumps are not written, so
//! PIDFD_INFO_COREDUMP only reports what the dumpable flag would have decided.

use crate::SysCtx;
use proc::ns::Kind;
use proc::ns::file::NsRef;
use proc::pidfd::Target;
use uapi::ab::pidfd::*;
use uapi::errno::*;

/// `_IOC_NR`, `_IOC_TYPE`, `_IOC_SIZE`, `_IOC_DIR` of an ioctl number.
fn ioc(cmd: u64) -> (u64, u64, usize, u64) {
    (cmd & 0xff, (cmd >> 8) & 0xff, ((cmd >> 16) & 0x3fff) as usize, cmd >> 30)
}

/// `PIDFD_INFO_SUPPORTED`: every bit this kernel's struct knows.
const SUPPORTED: u64 = 0xff;

/// The pidfs ioctl `cmd` on pidfd `t` (None: not a pidfd ioctl number, the generic path answers ENOTTY).
pub(crate) fn pidfd_ioctl(c: &SysCtx<'_>, t: &Target, cmd: u64, arg: u64) -> Option<i64> {
    let (nr, ty, size, dir) = ioc(cmd);
    if cmd == FS_IOC_GETVERSION {
        // `i_generation` is 0 on 64-bit (`pidfs_gen`).
        if arg == 0 {
            return Some(-EINVAL);
        }
        return Some(c.proc.mm.copy_to_user(arg, &0u32.to_le_bytes()).map(|_| 0).unwrap_or(-EFAULT));
    }
    if ty != PIDFS_IOCTL_MAGIC {
        return None;
    }
    // `extensible_ioctl_valid(cmd, PIDFD_GET_INFO, VER0)`: _IOWR, type 0xFF, nr 11, at least the first struct.
    if nr == PIDFD_GET_INFO_NR {
        return (dir == 3 && size >= PIDFD_INFO_SIZE_VER0).then(|| info(c, t, arg, size));
    }
    if dir != 0 || size != 0 || !(1..=10).contains(&nr) {
        return None;
    }
    Some(namespace(c, t, nr, arg))
}

/// `pidfd_info`: EINVAL without a pointer, EFAULT for the mask, EREMOTE outside the caller's pid namespace tree;
/// a pid without its task gives only exit information (ESRCH when none was asked for).
fn info(c: &SysCtx<'_>, t: &Target, arg: u64, usize_: usize) -> i64 {
    if arg == 0 {
        return -EINVAL;
    }
    let mut m = [0u8; 8];
    if c.proc.mm.copy_from_user(&mut m, arg).is_err() {
        return -EFAULT;
    }
    let want = u64::from_le_bytes(m);
    if !super::visible(c, t) {
        return -EREMOTE;
    }
    let mut k = [0u8; PIDFD_INFO_SIZE_VER3];
    let mut mask = 0u64;
    let put32 = |k: &mut [u8; PIDFD_INFO_SIZE_VER3], off: usize, v: u32| k[off..off + 4].copy_from_slice(&v.to_le_bytes());
    if want & PIDFD_INFO_EXIT != 0
        && let Some(code) = t.exit_code()
    {
        mask |= PIDFD_INFO_EXIT;
        put32(&mut k, 60, code);
    }
    match proc::pidfd::process_of(&t.group).filter(|_| t.attached()) {
        None if want & PIDFD_INFO_EXIT == 0 => return -ESRCH,
        None => {}
        Some(p) => {
            if want & PIDFD_INFO_COREDUMP != 0 {
                let d = match p.mm_flags.dumpable() {
                    1 => PIDFD_COREDUMP_USER,
                    2 => PIDFD_COREDUMP_ROOT,
                    _ => PIDFD_COREDUMP_SKIP,
                };
                put32(&mut k, 64, d);
                mask |= PIDFD_INFO_COREDUMP;
            }
            let cr = p.cred();
            let me = c.proc.cred();
            let (u, g) = (|x| crate::ns_sys::uid_view(&me, x), |x| crate::ns_sys::gid_view(&me, x));
            for (off, v) in [(28, u(cr.uid)), (32, g(cr.gid)), (36, u(cr.euid)), (40, g(cr.egid)), (44, u(cr.suid)), (48, g(cr.sgid)), (52, u(cr.fsuid)), (56, g(cr.fsgid))] {
                put32(&mut k, off, v);
            }
            mask |= PIDFD_INFO_CREDS;
            let (pid, tgid, ppid) = (c.proc.vnr(t.id()), c.proc.vnr(t.group.tgid), c.proc.vnr(t.group.ppid()));
            if pid == 0 || tgid == 0 {
                return -ESRCH;
            }
            put32(&mut k, 16, pid);
            put32(&mut k, 20, tgid);
            put32(&mut k, 24, ppid);
            mask |= PIDFD_INFO_PID;
        }
    }
    if want & PIDFD_INFO_SUPPORTED_MASK != 0 {
        mask |= PIDFD_INFO_SUPPORTED_MASK;
        k[80..88].copy_from_slice(&SUPPORTED.to_le_bytes());
    }
    k[..8].copy_from_slice(&mask.to_le_bytes());
    // `copy_struct_to_user`: the caller's size; bytes beyond the kernel's struct are cleared.
    let n = usize_.min(k.len());
    if c.proc.mm.copy_to_user(arg, &k[..n]).is_err() {
        return -EFAULT;
    }
    if usize_ > n {
        let zero = alloc::vec![0u8; usize_ - n];
        if c.proc.mm.copy_to_user(arg + n as u64, &zero).is_err() {
            return -EFAULT;
        }
    }
    0
}

/// PIDFD_GET_*_NAMESPACE (`_IO(0xFF, nr)`): ESRCH without the task, EINVAL with an argument, EACCES without
/// ptrace READ access (fs credentials), ESRCH once it ended (no nsproxy), EOPNOTSUPP for a kind V2 lacks; else a
/// close-on-exec namespace fd.
fn namespace(c: &SysCtx<'_>, t: &Target, nr: u64, arg: u64) -> i64 {
    let Some(p) = proc::pidfd::process_of(&t.group).filter(|_| t.attached()) else { return -ESRCH };
    if arg != 0 {
        return -EINVAL;
    }
    if !c.proc.may_ptrace(&p, true) {
        return -EACCES;
    }
    if p.is_finished() {
        return -ESRCH;
    }
    let kind = match nr {
        2 => Kind::Ipc,
        3 => Kind::Mnt,
        4 => Kind::Net,
        5 => Kind::Pid,
        6 => Kind::PidForChildren,
        9 => Kind::User,
        10 => Kind::Uts,
        // cgroup (1), time (7) and time_for_children (8).
        _ => return -EOPNOTSUPP,
    };
    let ns: NsRef = proc::ns::file::ns_of(&p, kind);
    let f = proc::ns::file::open_file(ns, 0);
    c.proc.fds.install_flags(f, 0, true).map(|fd| fd as i64).unwrap_or_else(|e| -e)
}
