//! fcntl(2) complete and flock(2) (AB-5; linux-6.6/fs/fcntl.c `do_fcntl` :316, `SYSCALL_DEFINE3(fcntl)` :441,
//! fs/locks.c `fcntl_getlk`/`fcntl_setlk`/`flock`, `fcntl_setlease`, fs/notify/dnotify `fcntl_dirnotify`).
//!
//! fcntl: the fd is looked up raw (an O_PATH fd allows only F_DUPFD, F_DUPFD_CLOEXEC, F_GETFD, F_SETFD,
//! F_GETFL and F_DUPFD_QUERY, else EBADF). Then:
//! - F_DUPFD, F_DUPFD_CLOEXEC, F_GETFD, F_SETFD, F_GETFL, F_SETFL, F_SETPIPE_SZ, F_GETPIPE_SZ and unknown
//!   commands go to FS's `vfs::sys::fcntl` (FS-1/FS-3, host-tested there); F_SETFL first gets Linux's `setfl`
//!   refusals FS's part leaves out (O_NOATIME for a file the caller does not own: EPERM; O_DIRECT on a file
//!   without direct I/O, i.e. anything but a FIFO or a regular tmpfs/FAT file: EINVAL);
//! - record locks F_GETLK/F_SETLK/F_SETLKW and the OFD variants: `proc::locks` (real POSIX and OFD locks:
//!   conflicts between processes, waits, EDEADLK, release at close and exit);
//! - F_ADD_SEALS/F_GET_SEALS: the file's own `File::fcntl` (FS-10's memfd; EINVAL for every other file, as
//!   `memfd_fcntl` answers);
//! - F_SETOWN/F_GETOWN(_EX), F_GETOWNER_UIDS, F_SETSIG/F_GETSIG: the open file description's owner, kept here
//!   (`OWNERS`); no SIGIO is sent yet (no FASYNC sources in V2);
//! - F_SETLEASE/F_GETLEASE: Linux's checks, then every lease request answers EAGAIN (V2 cannot break leases;
//!   EAGAIN is Linux's own answer for a conflicting open), F_GETLEASE F_UNLCK;
//! - F_NOTIFY: EINVAL, Linux's answer with `fs.dir-notify-enable = 0` (no dnotify in V2);
//! - F_GET_RW_HINT/F_SET_RW_HINT: the inode's write-life hint (kept here; the block layer has no use yet);
//! - F_DUPFD_QUERY (Linux 6.10, CachyOS): whether another fd names the same open file.
//!
//! `sys_fcntl` and the lock-releasing `sys_close`/`sys_dup2`/`sys_dup3` are registered by FS's vfs-sys's
//! plain versions until the desk applies AB-5's integration patch, which removes those four `fs_call!` lines
//! and registers these here (plan/notes/ab-AB-5.md); the ktests call them directly meanwhile, and
//! `ab.ab5.programs` runs the fcntl transcript once they are registered. flock (73) and close_range (436, with
//! the lock release for each fd it closes) are registered here.

use proc::cred::Cap;
use crate::SysCtx;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use ksync::SpinLock;
use proc::Process;
use proc::locks::{self, LockType, OFFSET_MAX, Owner};
use uapi::ab::fcntl::*;
use uapi::errno::*;
use vfs::{Errno, OpenFile};

const SEEK_SET: i16 = 0;
const SEEK_CUR: i16 = 1;
const SEEK_END: i16 = 2;
const O_DIRECT: u64 = 0o40000;
const O_NOATIME: u64 = 0o1000000;
const S_IFMT: u32 = 0o170000;
const S_IFIFO: u32 = 0o010000;
const S_IFREG: u32 = 0o100000;
/// `_NSIG`.
const NSIG: u64 = 64;

/// The calling process's memory for FS's `vfs::sys` calls (the same checked copies vfs-sys uses).
struct Mem<'a>(&'a Process);

impl vfs::sys::UserMem for Mem<'_> {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        self.0.mm.copy_from_user(buf, addr).is_ok()
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        self.0.mm.copy_to_user(addr, data).is_ok()
    }
    fn interrupted(&self) -> bool {
        self.0.has_exited()
    }
    fn read_cstr(&self, addr: u64, max: usize) -> Result<Vec<u8>, Errno> {
        let mut v = Vec::new();
        match hal::usercopy::strncpy_from_user(self.0.mm.root(), addr, &mut v, max) {
            Ok(_) => Ok(v),
            Err(hal::usercopy::StrError::Fault) => Err(EFAULT),
            Err(hal::usercopy::StrError::TooLong) => Err(ENAMETOOLONG),
        }
    }
}

/// One of FS's `vfs::sys` calls with the caller's context (what vfs-sys's `run` builds).
fn vfs_call(c: &SysCtx<'_>, f: vfs::sys::SysFn) -> i64 {
    let p = c.proc;
    let mem = Mem(p);
    let s = vfs::sys::Sys { mem: &mem, fds: &p.fds, ns: &p.ns, cred: p.fs_cred(), subject: p.subject() };
    f(&s, c.args)
}

/// FS's fcntl for the commands it implements.
fn vfs_fcntl(c: &SysCtx<'_>) -> i64 {
    vfs_call(c, vfs::sys::fcntl)
}

// ---- close and dup2/dup3 over an open fd: the POSIX locks go (`filp_close` → `locks_remove_posix`) ------

/// `close(fd)`: FS's close, then the caller's POSIX locks on that file go (linux-6.6/fs/open.c `filp_close`).
/// Registered by the integration patch in place of vfs-sys's `fs_call!(CLOSE, v::close)`.
pub fn sys_close(c: &mut SysCtx<'_>) -> i64 {
    let old = c.proc.fds.get(c.args[0] as u32 as usize).ok();
    let r = vfs_call(c, vfs::sys::close);
    if r == 0
        && let Some(f) = old
    {
        locks::closed(&c.proc.fds, &f);
    }
    r
}

/// dup2/dup3 that replaced an open fd: `do_dup2` calls `filp_close` on the replaced file whatever it was, also
/// when it is the very open file now installed there (linux-6.6/fs/file.c:1139), so the caller's POSIX locks
/// on it go. dup2(fd, fd) replaces nothing.
fn dup_over(c: &mut SysCtx<'_>, f: vfs::sys::SysFn) -> i64 {
    let (old, new) = (c.args[0] as u32, c.args[1] as u32);
    let replaced = if old != new { c.proc.fds.get(new as usize).ok() } else { None };
    let r = vfs_call(c, f);
    if r >= 0
        && let Some(file) = replaced
    {
        locks::closed(&c.proc.fds, &file);
    }
    r
}

/// `dup2(old, new)` with the lock release (registered by the integration patch).
pub fn sys_dup2(c: &mut SysCtx<'_>) -> i64 {
    dup_over(c, vfs::sys::dup2)
}

/// `dup3(old, new, flags)` with the lock release (registered by the integration patch).
pub fn sys_dup3(c: &mut SysCtx<'_>) -> i64 {
    dup_over(c, vfs::sys::dup3)
}

const CLOSE_RANGE_UNSHARE: u64 = 1 << 1;
const CLOSE_RANGE_CLOEXEC: u64 = 1 << 2;

/// `close_range(fd, max_fd, flags)` (linux-6.6/fs/file.c `__close_range` :732): EINVAL for unknown flags or
/// fd > max_fd; then every open fd in the range (up to the table's end) is closed, each close dropping the
/// caller's POSIX locks on its file, or with CLOSE_RANGE_CLOEXEC only marked close-on-exec.
/// CLOSE_RANGE_UNSHARE on a table nobody else uses changes nothing (as Linux, whose `unshare_fd` then keeps
/// it); on a table another process shares (CLONE_FILES) V2 cannot swap in a private copy (`Process::fds` is
/// fixed), so it answers EINVAL, the answer of a kernel without that flag, which callers fall back from.
fn sys_close_range(c: &mut SysCtx<'_>) -> i64 {
    let (first, last, flags) = (c.args[0] as u32 as usize, c.args[1] as u32 as usize, c.args[2] as u32 as u64);
    if flags & !(CLOSE_RANGE_UNSHARE | CLOSE_RANGE_CLOEXEC) != 0 || first > last {
        return -EINVAL;
    }
    let fds = &c.proc.fds;
    if flags & CLOSE_RANGE_UNSHARE != 0 && proc::fds_shared(fds, c.proc.pid) {
        klog::event!(Info, "syscall", "close_range.unshare_shared", program = c.proc.name.as_str(); "CLOSE_RANGE_UNSHARE on a shared fd table: EINVAL (V2 cannot give the process a private copy)");
        return -EINVAL;
    }
    let end = last.min(fds.capacity().saturating_sub(1));
    for fd in first..=end {
        if flags & CLOSE_RANGE_CLOEXEC != 0 {
            let _ = fds.set_cloexec(fd, true);
        } else if let Ok(f) = fds.get(fd)
            && fds.close(fd).is_ok()
        {
            locks::closed(fds, &f);
        }
    }
    0
}
crate::syscall_nr!(uapi::ab::nr_more::CLOSE_RANGE, "close_range", sys_close_range);

// AB-5 integration patch: these four move here from vfs-sys (they call FS's `vfs::sys` for the work).
crate::syscall!(FCNTL, sys_fcntl);
crate::syscall!(CLOSE, sys_close);
crate::syscall!(DUP2, sys_dup2);
crate::syscall!(DUP3, sys_dup3);

fn ret(r: Result<i64, Errno>) -> i64 {
    r.unwrap_or_else(|e| -e)
}

/// `fcntl(fd, cmd, arg)`.
pub fn sys_fcntl(c: &mut SysCtx<'_>) -> i64 {
    let fd = c.args[0] as u32 as usize;
    let cmd = c.args[1] as u32 as u64;
    let arg = c.args[2];
    let Ok(f) = c.proc.fds.get(fd) else { return -EBADF };
    if f.is_path_only() && !matches!(cmd, F_DUPFD | F_DUPFD_CLOEXEC | F_GETFD | F_SETFD | F_GETFL | F_DUPFD_QUERY) {
        return -EBADF;
    }
    match cmd {
        F_SETFL => match setfl_checks(c, &f, arg as u32 as u64) {
            Ok(()) => vfs_fcntl(c),
            Err(e) => -e,
        },
        F_GETLK | F_OFD_GETLK => ret(getlk(c, &f, cmd, arg)),
        F_SETLK | F_SETLKW | F_OFD_SETLK | F_OFD_SETLKW => ret(setlk(c, &f, cmd, arg)),
        // FS-10's memfd answers in its `File::fcntl`; a file without its own answer goes to FS's fcntl (EINVAL
        // today, `memfd_fcntl` for a non-shmem file), so FS-10 may put the seals in either place.
        F_ADD_SEALS | F_GET_SEALS => match f.file.fcntl(cmd, arg as u32 as u64, c.proc.fs_cred()) {
            Some(r) => ret(r),
            None => vfs_fcntl(c),
        },
        F_GETOWN => owner_of(&f).getown() as i64,
        F_SETOWN => ret(setown(c, &f, arg as i32)),
        F_GETOWN_EX => ret(getown_ex(c, &f, arg)),
        F_SETOWN_EX => ret(setown_ex(c, &f, arg)),
        F_GETOWNER_UIDS => {
            let o = owner_of(&f);
            let mut b = [0u8; 8];
            b[..4].copy_from_slice(&o.uid.to_le_bytes());
            b[4..].copy_from_slice(&o.uid.to_le_bytes());
            c.proc.mm.copy_to_user(arg, &b).map(|_| 0).unwrap_or(-EFAULT)
        }
        F_GETSIG => owner_of(&f).signum as i64,
        F_SETSIG => {
            // `valid_signal(argi)`: the int as an unsigned long.
            let s = arg as i32 as i64 as u64;
            if s > NSIG {
                return -EINVAL;
            }
            update_owner(&f, |o| o.signum = s as u32);
            0
        }
        F_GETLEASE => LockType::Unlock.linux() as i64,
        F_SETLEASE => ret(setlease(c, &f, arg as i32)),
        F_NOTIFY => -EINVAL,
        F_GET_RW_HINT | F_SET_RW_HINT => ret(rw_hint(c, &f, cmd, arg)),
        F_DUPFD_QUERY => {
            // `f_dupfd_query` (Linux 6.10+, CachyOS): whether the raw fd `argi` names this very open file;
            // EBADF when it names none.
            let other = arg as i32;
            match (other >= 0).then(|| c.proc.fds.get(other as usize).ok()).flatten() {
                Some(g) => Arc::ptr_eq(&g, &f) as i64,
                None => -EBADF,
            }
        }
        _ => vfs_fcntl(c),
    }
}

/// `setfl`'s refusals (fs/fcntl.c:34).
fn setfl_checks(c: &SysCtx<'_>, f: &OpenFile, arg: u64) -> Result<(), Errno> {
    // `inode_owner_or_capable`: the fsuid owns it, or CAP_FOWNER.
    if arg & O_NOATIME != 0 && f.status_flags() & O_NOATIME == 0 && !c.proc.capable(Cap::Fowner) && f.stat().uid != c.proc.cred().fsuid {
        return Err(EPERM);
    }
    let ty = f.file.mode() & S_IFMT;
    let direct_io = f.inode().is_some_and(|i| ty == S_IFREG && matches!(i.data, vfs::inode::Data::Reg(_) | vfs::inode::Data::External(_)));
    if ty != S_IFIFO && arg & O_DIRECT != 0 && !direct_io {
        return Err(EINVAL);
    }
    Ok(())
}

// ---- record locks ------------------------------------------------------------------------------------

/// `struct flock` as Linux reads it.
struct Flock {
    raw: [u8; FLOCK_SIZE],
    l_type: i16,
    whence: i16,
    start: i64,
    len: i64,
    pid: i32,
}

fn read_flock(c: &SysCtx<'_>, arg: u64) -> Result<Flock, Errno> {
    let mut raw = [0u8; FLOCK_SIZE];
    c.proc.mm.copy_from_user(&mut raw, arg).map_err(|_| EFAULT)?;
    let l_type = i16::from_le_bytes([raw[0], raw[1]]);
    let whence = i16::from_le_bytes([raw[2], raw[3]]);
    let start = i64::from_le_bytes(raw[8..16].try_into().unwrap_or([0; 8]));
    let len = i64::from_le_bytes(raw[16..24].try_into().unwrap_or([0; 8]));
    let pid = i32::from_le_bytes(raw[24..28].try_into().unwrap_or([0; 4]));
    Ok(Flock { raw, l_type, whence, start, len, pid })
}

/// `flock64_to_posix_lock`: the inclusive range and the type.
pub(crate) fn to_range(f: &OpenFile, l_type: i16, whence: i16, start: i64, len: i64) -> Result<(LockType, i64, i64), Errno> {
    let base: i64 = match whence {
        SEEK_SET => 0,
        SEEK_CUR => f.pos() as i64,
        SEEK_END => f.file.size() as i64,
        _ => return Err(EINVAL),
    };
    range(base, start, len).and_then(|(s, e)| LockType::from_linux(l_type).map(|t| (t, s, e)).ok_or(EINVAL))
}

/// The pure part of `flock64_to_posix_lock` (host-tested): `base` + `start`, `len` (0 = to the end, negative =
/// the bytes before).
pub(crate) fn range(base: i64, start: i64, len: i64) -> Result<(i64, i64), Errno> {
    if start > OFFSET_MAX - base {
        return Err(EOVERFLOW);
    }
    let s = base + start;
    if s < 0 {
        return Err(EINVAL);
    }
    if len > 0 {
        if len - 1 > OFFSET_MAX - s {
            return Err(EOVERFLOW);
        }
        Ok((s, s + (len - 1)))
    } else if len < 0 {
        if s + len < 0 {
            return Err(EINVAL);
        }
        Ok((s + len, s - 1))
    } else {
        Ok((s, OFFSET_MAX))
    }
}

fn owner_for(c: &SysCtx<'_>, f: &Arc<OpenFile>, ofd: bool) -> Owner {
    if ofd { Owner::Ofd(f.clone()) } else { Owner::Posix { fds: c.proc.fds.clone(), pid: c.proc.pid } }
}

/// `fcntl_getlk`.
fn getlk(c: &SysCtx<'_>, f: &Arc<OpenFile>, cmd: u64, arg: u64) -> Result<i64, Errno> {
    let mut l = read_flock(c, arg)?;
    if cmd != F_OFD_GETLK && l.l_type != LockType::Read.linux() && l.l_type != LockType::Write.linux() {
        return Err(EINVAL);
    }
    let (typ, start, end) = to_range(f, l.l_type, l.whence, l.start, l.len)?;
    let ofd = cmd == F_OFD_GETLK;
    if ofd && l.pid != 0 {
        return Err(EINVAL);
    }
    let r = locks::test(f, &owner_for(c, f, ofd), typ, start, end)?;
    match r {
        None => l.raw[0..2].copy_from_slice(&LockType::Unlock.linux().to_le_bytes()),
        Some(k) => {
            // `posix_lock_to_flock`.
            l.raw[0..2].copy_from_slice(&k.typ.linux().to_le_bytes());
            l.raw[2..4].copy_from_slice(&0i16.to_le_bytes());
            l.raw[8..16].copy_from_slice(&k.start.to_le_bytes());
            l.raw[16..24].copy_from_slice(&k.len().to_le_bytes());
            l.raw[24..28].copy_from_slice(&k.pid.to_le_bytes());
        }
    }
    c.proc.mm.copy_to_user(arg, &l.raw).map_err(|_| EFAULT)?;
    Ok(0)
}

/// `fcntl_setlk`.
fn setlk(c: &SysCtx<'_>, f: &Arc<OpenFile>, cmd: u64, arg: u64) -> Result<i64, Errno> {
    let l = read_flock(c, arg)?;
    let (typ, start, end) = to_range(f, l.l_type, l.whence, l.start, l.len)?;
    // `check_fmode_for_setlk`.
    if (typ == LockType::Read && !f.can_read()) || (typ == LockType::Write && !f.can_write()) {
        return Err(EBADF);
    }
    let ofd = matches!(cmd, F_OFD_SETLK | F_OFD_SETLKW);
    if ofd && l.pid != 0 {
        return Err(EINVAL);
    }
    let wait = matches!(cmd, F_SETLKW | F_OFD_SETLKW);
    let p = c.proc.clone();
    locks::set(f, &owner_for(c, f, ofd), typ, start, end, wait, &|| p.signal_pending())?;
    Ok(0)
}

/// `flock(fd, cmd)` (fs/locks.c `SYSCALL_DEFINE2(flock)`).
pub(crate) fn sys_flock(c: &mut SysCtx<'_>) -> i64 {
    let fd = c.args[0] as u32 as usize;
    let cmd = c.args[1] as u32 as u64;
    if cmd & LOCK_MAND != 0 {
        // Removed from Linux: ignored with a warning, success.
        klog::event!(Debug, "syscall", "flock.mand_ignored", pid = c.proc.pid);
        return 0;
    }
    let typ = match cmd & !LOCK_NB {
        LOCK_SH => LockType::Read,
        LOCK_EX => LockType::Write,
        LOCK_UN => LockType::Unlock,
        _ => return -EINVAL,
    };
    let f = match c.proc.fds.get(fd) {
        Ok(f) if !f.is_path_only() => f,
        _ => return -EBADF,
    };
    if typ != LockType::Unlock && !f.can_read() && !f.can_write() {
        return -EBADF;
    }
    let p = c.proc.clone();
    ret(locks::flock(&f, typ, cmd & LOCK_NB == 0, &|| p.signal_pending()).map(|_| 0))
}
crate::syscall_nr!(uapi::ab::nr_more::FLOCK, "flock", sys_flock);

// ---- owner and signal (`struct fown_struct`) --------------------------------------------------------

/// Linux's pid types an owner can be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PidType {
    /// PIDTYPE_PID: a thread (F_OWNER_TID). A fresh file's `f_owner` is zeroed, so this is the default.
    Pid,
    /// PIDTYPE_TGID: a process (F_SETOWN with a positive id, F_OWNER_PID).
    Tgid,
    /// PIDTYPE_PGID: a process group (F_SETOWN with a negative id, F_OWNER_PGRP).
    Pgid,
}

#[derive(Clone, Copy)]
struct FOwner {
    pid: u32,
    ty: PidType,
    /// The uid of whoever set a non-zero owner (`f_owner.uid`/`euid`).
    uid: u32,
    signum: u32,
}

impl FOwner {
    const NONE: FOwner = FOwner { pid: 0, ty: PidType::Pid, uid: 0, signum: 0 };

    /// `pid_task(f_owner.pid, f_owner.pid_type)`: the owner still exists.
    fn alive(&self) -> bool {
        self.pid != 0
            && match self.ty {
                PidType::Pid => proc::find_thread(self.pid).is_some(),
                PidType::Tgid => proc::find(self.pid).is_some(),
                PidType::Pgid => proc::job::pgrp_exists(self.pid),
            }
    }
    /// `f_getown`: the id (negative for a group) while its owner exists, else 0.
    fn getown(&self) -> i32 {
        if !self.alive() {
            return 0;
        }
        if self.ty == PidType::Pgid { -(self.pid as i32) } else { self.pid as i32 }
    }
}

/// The owner state of every open file description that set one, by its address (weakly: an entry whose open
/// file is gone is dropped at the next change).
static OWNERS: SpinLock<BTreeMap<usize, (Weak<OpenFile>, FOwner)>> = SpinLock::named("ab.fown", BTreeMap::new());

fn owner_of(f: &Arc<OpenFile>) -> FOwner {
    OWNERS.lock().get(&(Arc::as_ptr(f) as usize)).filter(|(w, _)| w.strong_count() > 0).map_or(FOwner::NONE, |(_, o)| *o)
}

fn update_owner(f: &Arc<OpenFile>, change: impl FnOnce(&mut FOwner)) {
    let key = Arc::as_ptr(f) as usize;
    let gone: Vec<(Weak<OpenFile>, FOwner)> = {
        let mut m = OWNERS.lock();
        let mut cur = m.get(&key).filter(|(w, _)| w.strong_count() > 0).map_or(FOwner::NONE, |(_, o)| *o);
        change(&mut cur);
        m.insert(key, (Arc::downgrade(f), cur));
        // Entries of open files that are gone (bounded by the files that ever set an owner).
        let dead: Vec<usize> = m.iter().filter(|(_, (w, _))| w.strong_count() == 0).map(|(k, _)| *k).collect();
        dead.into_iter().filter_map(|k| m.remove(&k)).collect()
    };
    drop(gone);
}

/// `find_vpid(id)` exists: a process, a thread, or a process group or session that still has members.
fn pid_exists(id: u32) -> bool {
    proc::find(id).is_some() || proc::find_thread(id).is_some() || proc::job::pgrp_exists(id) || proc::processes().iter().any(|p| p.sid() == id)
}

/// `__f_setown(filp, pid, type, force = 1)`.
fn set_owner(c: &SysCtx<'_>, f: &Arc<OpenFile>, pid: u32, ty: PidType) {
    let uid = c.proc.cred().uid;
    update_owner(f, |o| {
        o.pid = pid;
        o.ty = ty;
        if pid != 0 {
            o.uid = uid;
        }
    });
}

/// `f_setown(filp, who, 1)`.
fn setown(c: &SysCtx<'_>, f: &Arc<OpenFile>, who: i32) -> Result<i64, Errno> {
    let (ty, id) = if who < 0 {
        if who == i32::MIN {
            return Err(EINVAL);
        }
        (PidType::Pgid, (-who) as u32)
    } else {
        (PidType::Tgid, who as u32)
    };
    if id != 0 && !pid_exists(id) {
        return Err(ESRCH);
    }
    set_owner(c, f, id, ty);
    Ok(0)
}

/// `f_setown_ex`.
fn setown_ex(c: &SysCtx<'_>, f: &Arc<OpenFile>, arg: u64) -> Result<i64, Errno> {
    let mut b = [0u8; 8];
    c.proc.mm.copy_from_user(&mut b, arg).map_err(|_| EFAULT)?;
    let ty = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let pid = i32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    let ty = match ty {
        F_OWNER_TID => PidType::Pid,
        F_OWNER_PID => PidType::Tgid,
        F_OWNER_PGRP => PidType::Pgid,
        _ => return Err(EINVAL),
    };
    // `find_vpid(owner.pid)`: a negative id names nothing.
    if pid != 0 && (pid < 0 || !pid_exists(pid as u32)) {
        return Err(ESRCH);
    }
    set_owner(c, f, pid.max(0) as u32, ty);
    Ok(0)
}

/// `f_getown_ex`.
fn getown_ex(c: &SysCtx<'_>, f: &Arc<OpenFile>, arg: u64) -> Result<i64, Errno> {
    let o = owner_of(f);
    let ty = match o.ty {
        PidType::Pid => F_OWNER_TID,
        PidType::Tgid => F_OWNER_PID,
        PidType::Pgid => F_OWNER_PGRP,
    };
    let pid = if o.alive() { o.pid as i32 } else { 0 };
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&ty.to_le_bytes());
    b[4..].copy_from_slice(&pid.to_le_bytes());
    c.proc.mm.copy_to_user(arg, &b).map_err(|_| EFAULT)?;
    Ok(0)
}

// ---- leases and write hints ----------------------------------------------------------------------------

static LEASE_NOTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// `fcntl_setlease`: the type (`lease_alloc` → `assign_type`), then `vfs_setlease`'s owner check (EACCES) and
/// regular-file check (EINVAL); an unlock without a lease and every lease request answer EAGAIN.
fn setlease(c: &SysCtx<'_>, f: &OpenFile, arg: i32) -> Result<i64, Errno> {
    let typ = i16::try_from(arg).ok().and_then(LockType::from_linux).ok_or(EINVAL)?;
    let st = f.stat();
    // `generic_setlease`: the fsuid owns the file, or CAP_LEASE.
    if st.uid != c.proc.cred().fsuid && !c.proc.capable(Cap::Lease) {
        return Err(EACCES);
    }
    if f.file.mode() & S_IFMT != S_IFREG {
        return Err(EINVAL);
    }
    if typ != LockType::Unlock && !LEASE_NOTED.swap(true, core::sync::atomic::Ordering::Relaxed) {
        klog::event!(Info, "syscall", "lease.refused", program = c.proc.name.as_str(); "file leases are not built (nothing could break them): EAGAIN, Linux's answer for a conflicting open (logged once)");
    }
    Err(EAGAIN)
}

/// What a write-life hint belongs to, held weakly.
enum HintOwner {
    Inode(Weak<vfs::Inode>),
    Object(Weak<dyn vfs::File>),
}

impl HintOwner {
    fn alive(&self) -> bool {
        match self {
            HintOwner::Inode(w) => w.strong_count() > 0,
            HintOwner::Object(w) => w.strong_count() > 0,
        }
    }
}

/// Write-life hints per inode (`inode->i_write_hint`), by the inode's (or nameless object's) address.
static HINTS: SpinLock<BTreeMap<usize, (HintOwner, u64)>> = SpinLock::named("ab.rw_hints", BTreeMap::new());

fn hint_key(f: &OpenFile) -> (usize, HintOwner) {
    match f.inode() {
        Some(i) => (Arc::as_ptr(i) as *const u8 as usize, HintOwner::Inode(Arc::downgrade(i))),
        None => (Arc::as_ptr(&f.file) as *const u8 as usize, HintOwner::Object(Arc::downgrade(&f.file))),
    }
}

/// `fcntl_rw_hint`.
fn rw_hint(c: &SysCtx<'_>, f: &OpenFile, cmd: u64, arg: u64) -> Result<i64, Errno> {
    let (key, weak) = hint_key(f);
    if cmd == F_GET_RW_HINT {
        let h = HINTS.lock().get(&key).filter(|(w, _)| w.alive()).map_or(0, |(_, h)| *h);
        c.proc.mm.copy_to_user(arg, &h.to_le_bytes()).map_err(|_| EFAULT)?;
        return Ok(0);
    }
    let mut b = [0u8; 8];
    c.proc.mm.copy_from_user(&mut b, arg).map_err(|_| EFAULT)?;
    // `(enum rw_hint) h`: the low 32 bits, valid up to RWH_WRITE_LIFE_EXTREME.
    let h = u64::from_le_bytes(b) as u32 as u64;
    if h > RWH_WRITE_LIFE_EXTREME {
        return Err(EINVAL);
    }
    let gone: Vec<(HintOwner, u64)> = {
        let mut m = HINTS.lock();
        m.insert(key, (weak, h));
        let dead: Vec<usize> = m.iter().filter(|(_, (w, _))| !w.alive()).map(|(k, _)| *k).collect();
        dead.into_iter().filter_map(|k| m.remove(&k)).collect()
    };
    drop(gone);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_as_flock64_to_posix_lock() {
        assert_eq!(range(0, 10, 5), Ok((10, 14)));
        assert_eq!(range(0, 10, 0), Ok((10, OFFSET_MAX)), "length 0 = to the end");
        assert_eq!(range(0, 10, -4), Ok((6, 9)), "negative length = the bytes before");
        assert_eq!(range(0, 10, -11), Err(EINVAL));
        assert_eq!(range(0, -1, 5), Err(EINVAL));
        assert_eq!(range(100, -50, 1), Ok((50, 50)), "SEEK_CUR/SEEK_END base");
        assert_eq!(range(1, i64::MAX, 1), Err(EOVERFLOW));
        assert_eq!(range(0, i64::MAX, 2), Err(EOVERFLOW));
        assert_eq!(range(0, i64::MAX, 1), Ok((i64::MAX, i64::MAX)));
    }

    #[test]
    fn owner_answers() {
        let o = FOwner::NONE;
        assert_eq!(o.getown(), 0);
        assert_eq!(o.ty, PidType::Pid, "a fresh file's owner type is PIDTYPE_PID (F_OWNER_TID)");
    }
}
