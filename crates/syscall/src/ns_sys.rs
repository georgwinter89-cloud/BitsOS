//! SX-2 glue: unshare(2), setns(2), chroot(2) and the id views every other call uses (linux-6.6/kernel/fork.c
//! `ksys_unshare`, kernel/nsproxy.c `setns`, fs/open.c:593 `chroot`). The namespaces themselves are in
//! `proc::ns`.
//!
//! Id views: a program names processes by its pid namespace's numbers and users by its user namespace's ids.
//! `find`/`find_thread` turn a number from a program into the process or thread (None: not visible, ESRCH);
//! `vnr` turns a global id into the caller's number (0: not visible). `uid_view`/`gid_view` show kernel ids in
//! the caller's user namespace (the overflow id 65534 when unmapped), `kuid`/`kgid` make kernel ids of what the
//! program passed (None: no mapping, EINVAL for set*id). In the initial namespaces all of them are the identity.
//!
//! chroot: every check of Linux runs here (EFAULT, ENAMETOOLONG, ENOENT, ENOTDIR, EACCES on the path and on the
//! directory, then EPERM without CAP_SYS_CHROOT in the caller's user namespace); the root lives in the
//! calling thread's `vfs::FsState` (FS-16: apart from the fd table, plan/notes/fs-FS-16.md), which CLONE_FS shares and absolute
//! lookups, getcwd, `/proc/<pid>/root` and execve follow. A root that is the directory of something gone (the
//! fdinfo directory of an ended task) answers ENOENT to every lookup below it.

use crate::SysCtx;
use alloc::sync::Arc;
use proc::cred::Cap;
use proc::ns::user::MapKind;
use proc::{Process, Thread};
use uapi::errno::*;

/// linux-6.6/arch/x86/entry/syscalls/syscall_64.tbl (uapi is desk-owned: CR 4).
pub(crate) const NR_CHROOT: usize = uapi::nr::CHROOT;
pub(crate) const NR_UNSHARE: usize = uapi::nr::UNSHARE;
pub(crate) const NR_SETNS: usize = uapi::nr::SETNS;

/// The global id of number `nr` in the caller's pid namespace.
pub(crate) fn id(c: &SysCtx<'_>, nr: u32) -> Option<u32> {
    c.proc.pid_ns.id(nr)
}

/// The process number `nr` names for the caller (its image; a thread's number names its process).
pub(crate) fn find(c: &SysCtx<'_>, nr: u32) -> Option<Arc<Process>> {
    proc::find(id(c, nr)?)
}

/// The thread number `nr` names for the caller.
pub(crate) fn find_thread(c: &SysCtx<'_>, nr: u32) -> Option<Arc<Thread>> {
    proc::find_thread(id(c, nr)?)
}

/// Global id `g` as the caller numbers it (0: not visible from its pid namespace).
pub(crate) fn vnr(c: &SysCtx<'_>, g: u32) -> u32 {
    c.proc.vnr(g)
}

/// Kernel uid `k` as `cred`'s user namespace shows it (`from_kuid_munged`).
pub(crate) fn uid_view(cred: &proc::cred::Cred, k: u32) -> u32 {
    cred.user_ns.view(MapKind::Uid, k)
}

pub(crate) fn gid_view(cred: &proc::cred::Cred, k: u32) -> u32 {
    cred.user_ns.view(MapKind::Gid, k)
}

/// `make_kuid(current_user_ns(), id)`; -1 stays -1 (the set*id calls' "unchanged").
pub(crate) fn kuid(cred: &proc::cred::Cred, id: u32) -> Option<u32> {
    if id == u32::MAX { Some(id) } else { cred.user_ns.to_kernel(MapKind::Uid, id) }
}

pub(crate) fn kgid(cred: &proc::cred::Cred, id: u32) -> Option<u32> {
    if id == u32::MAX { Some(id) } else { cred.user_ns.to_kernel(MapKind::Gid, id) }
}

/// unshare(flags): `Process::unshare` (the flag rules, the user namespace first, the rest).
fn sys_unshare(c: &mut SysCtx<'_>) -> i64 {
    match c.proc.unshare(c.thread, c.args[0]) {
        Ok(()) => 0,
        Err(e) => {
            klog::event!(Debug, "ns", "unshare.failed", pid = c.proc.pid, flags = klog::Hex(c.args[0]), errno = e);
            -e
        }
    }
}
crate::syscall_nr!(NR_UNSHARE, "unshare", sys_unshare);

/// setns(fd, nstype): EBADF for a bad fd; a namespace file goes to `Process::setns`; a pidfd is not built
/// (EINVAL, event); any other file EINVAL (`setns`).
fn sys_setns(c: &mut SysCtx<'_>) -> i64 {
    let fd = c.args[0] as u32 as i32;
    let nstype = c.args[1] as u32 as i32;
    if fd < 0 {
        return -EBADF;
    }
    let Ok(f) = c.proc.fds.get(fd as usize) else { return -EBADF };
    if let Some(ns) = proc::ns::file::ns_of_file(&f) {
        drop(f);
        return match c.proc.setns(c.thread, &ns, nstype as u32 as u64) {
            Ok(()) => 0,
            Err(e) => {
                klog::event!(Debug, "ns", "setns.failed", pid = c.proc.pid, kind = ns.kind().name(), errno = e);
                -e
            }
        };
    }
    if proc::pidfd::group_of(&f).is_some() {
        klog::event!(Info, "ns", "unsupported", pid = c.proc.pid, flags = klog::Hex(nstype as u32 as u64), what = "setns through a pidfd");
    }
    -EINVAL
}
crate::syscall_nr!(NR_SETNS, "setns", sys_setns);

/// chroot(path) (fs/open.c:593): the directory looked up as chdir does (`user_path_at` with LOOKUP_FOLLOW |
/// LOOKUP_DIRECTORY, then `path_permission(MAY_EXEC | MAY_CHDIR)`), then CAP_SYS_CHROOT in the caller's user
/// namespace (EPERM). See the module doc for the root itself.
fn sys_chroot(c: &mut SysCtx<'_>) -> i64 {
    let p = c.proc;
    let mem = crate::socket::ProcMem(p);
    let fs = c.thread.fs(p);
    let ns = p.mount_ns();
    let sys = vfs::sys::Sys { mem: &mem, fds: &p.fds, fs: &fs, ns: &ns, cred: p.fs_cred(), subject: p.subject() };
    use vfs::sys::UserMem;
    // `getname`: EFAULT, ENAMETOOLONG at PATH_MAX, ENOENT for an empty path.
    let path = match mem.read_cstr(c.args[0], vfs::abi::PATH_MAX) {
        Ok(v) if v.is_empty() => return -ENOENT,
        Ok(v) => v,
        Err(e) => return -e,
    };
    let target = match sys.ctx().chdir_target(None, &path) {
        Ok(t) => t,
        Err(e) => return -e,
    };
    if !proc::cred::own_capable(&p.cred(), Cap::SysChroot) {
        klog::event!(Info, "ns", "chroot.refused", pid = p.pid, uid = p.cred().euid);
        return -EPERM;
    }
    set_root(&fs, p.pid, target)
}
crate::syscall_nr!(NR_CHROOT, "chroot", sys_chroot);

/// Puts `root` in place as the calling thread's root (`set_fs_root`): in its `vfs::FsState`, which every CLONE_FS
/// sharer sees (Chromium's chroot child changes its parent's root this way) and a CLONE_FILES-only sibling does
/// not (FS-16: the state is apart from the fd table).
fn set_root(fs: &vfs::FsState, pid: u32, root: vfs::Path) -> i64 {
    fs.set_root(root);
    klog::event!(Info, "ns", "chroot", pid = pid);
    0
}
