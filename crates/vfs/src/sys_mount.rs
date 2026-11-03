//! SX-3a: mount(2), umount2(2) and pivot_root(2) (linux-6.6/fs/namespace.c `SYSCALL_DEFINE5(mount)`,
//! `path_mount`, `ksys_umount`, `SYSCALL_DEFINE2(pivot_root)`): argument copies, lookups and the dispatch, in
//! Linux's order. What only the process layer knows (capabilities in which user namespace, the mounter's ids,
//! re-pointing every task's root) comes through `MountPerms` (vfs-sys implements it; host tests fake it).

use crate::abi::PATH_MAX;
use crate::dentry::SuperBlock;
use crate::mount_fs::{self, Mounter};
use crate::mount_ns::*;
use crate::mount_ops::{MNT_DETACH, MNT_EXPIRE, MNT_FORCE, Propagation, UMOUNT_NOFOLLOW};
use crate::sys::{Args, Sys, ret};
use crate::Errno;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;

pub const MS_RDONLY: u64 = 1;
pub const MS_NOSUID: u64 = 2;
pub const MS_NODEV: u64 = 4;
pub const MS_NOEXEC: u64 = 8;
pub const MS_SYNCHRONOUS: u64 = 16;
pub const MS_REMOUNT: u64 = 32;
pub const MS_MANDLOCK: u64 = 64;
pub const MS_DIRSYNC: u64 = 128;
pub const MS_NOSYMFOLLOW: u64 = 256;
pub const MS_NOATIME: u64 = 1024;
pub const MS_NODIRATIME: u64 = 2048;
pub const MS_BIND: u64 = 4096;
pub const MS_MOVE: u64 = 8192;
pub const MS_REC: u64 = 16384;
pub const MS_SILENT: u64 = 32768;
pub const MS_UNBINDABLE: u64 = 1 << 17;
pub const MS_PRIVATE: u64 = 1 << 18;
pub const MS_SLAVE: u64 = 1 << 19;
pub const MS_SHARED: u64 = 1 << 20;
pub const MS_RELATIME: u64 = 1 << 21;
pub const MS_STRICTATIME: u64 = 1 << 24;
pub const MS_NOUSER: u64 = 1 << 31;
const MS_MGC_VAL: u64 = 0xC0ED_0000;
const MS_MGC_MSK: u64 = 0xFFFF_0000;

/// What the process layer answers for a mount call (vfs-sys: the caller's credentials and namespaces).
pub trait MountPerms {
    /// `may_mount`: CAP_SYS_ADMIN in the caller's mount namespace's owner.
    fn may_mount(&self) -> bool;
    /// `capable(CAP_SYS_ADMIN)` in the initial user namespace (MNT_FORCE).
    fn capable_init(&self) -> bool;
    /// `mount_capable` for a new mount of `fstype`: CAP_SYS_ADMIN in the namespace that will own it (tmpfs,
    /// devpts: the caller's; proc: its pid namespace's owner; sysfs: its network namespace's; mqueue: its IPC
    /// namespace's).
    fn may_mount_fs(&self, fstype: &str) -> bool;
    /// That owner, for the new super block (None: the initial user namespace).
    fn fs_owner(&self, fstype: &str) -> Option<Arc<dyn core::any::Any + Send + Sync>>;
    /// `ns_capable(sb->s_user_ns, CAP_SYS_ADMIN)`.
    fn admin_over(&self, sb: &SuperBlock) -> bool;
    /// The caller's mount namespace is owned by the initial user namespace (`mount_too_revealing`).
    fn ns_owned_by_init(&self) -> bool;
    /// The caller as mounter (ids and how option ids map).
    fn with_mounter(&self, f: &mut dyn FnMut(&Mounter<'_>) -> Result<(), Errno>) -> Result<(), Errno>;
    /// The system's pty multiplexer (`/dev/ptmx`) for a devpts instance's `ptmx`.
    fn ptmx(&self) -> Option<Arc<dyn crate::File>>;
    /// The machine's RAM in bytes (tmpfs `size=N%`).
    fn ram_bytes(&self) -> u64;
    /// `chroot_fs_refs` after pivot_root: every task whose root or cwd is `old` gets `new`.
    fn chroot_fs_refs(&self, old: &Path, new: &Path);
    /// SX-3b: the caller's active pid namespace (`task_active_pid_ns(current)`), opaque, for a new proc
    /// instance; None: a mount of proc shows the boot instance (SX-3a's behaviour, host tests).
    fn pid_ns(&self) -> Option<Arc<dyn core::any::Any + Send + Sync>> {
        None
    }
}

/// `strndup_user(s, PATH_MAX)`: None for a null pointer, EFAULT, EINVAL when longer than PATH_MAX.
fn string_arg(s: &Sys<'_>, ptr: u64) -> Result<Option<Vec<u8>>, Errno> {
    if ptr == 0 {
        return Ok(None);
    }
    match s.mem.read_cstr(ptr, PATH_MAX) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e == ENAMETOOLONG => Err(EINVAL),
        Err(e) => Err(e),
    }
}

/// `copy_mount_options`: one page, as far as it is readable (EFAULT when not even its first byte is).
fn options_arg(s: &Sys<'_>, ptr: u64) -> Result<Option<Vec<u8>>, Errno> {
    if ptr == 0 {
        return Ok(None);
    }
    let mut page = alloc::vec![0u8; 4096];
    if s.mem.read(ptr, &mut page) {
        return Ok(Some(page));
    }
    let mut got = 0;
    while got < page.len() && s.mem.read(ptr + got as u64, &mut page[got..got + 1]) {
        got += 1;
    }
    if got == 0 {
        return Err(EFAULT);
    }
    page.truncate(got);
    Ok(Some(page))
}

/// `path_mount`'s split of the per-mount flags.
fn mnt_flags_of(flags: u64, cur: Option<u32>) -> u32 {
    let mut f = 0;
    if flags & MS_NOATIME == 0 {
        f |= MNT_RELATIME;
    }
    for (ms, mnt) in [(MS_NOSUID, MNT_NOSUID), (MS_NODEV, MNT_NODEV), (MS_NOEXEC, MNT_NOEXEC), (MS_NOATIME, MNT_NOATIME), (MS_NODIRATIME, MNT_NODIRATIME), (MS_RDONLY, MNT_READONLY), (MS_NOSYMFOLLOW, MNT_NOSYMFOLLOW)] {
        if flags & ms != 0 {
            f |= mnt;
        }
    }
    if flags & MS_STRICTATIME != 0 {
        f &= !(MNT_RELATIME | MNT_NOATIME);
    }
    // The default atime for remount is preservation.
    if let Some(c) = cur
        && flags & (MS_NOATIME | MS_NODIRATIME | MS_RELATIME | MS_STRICTATIME) == 0
    {
        f = (f & !MNT_ATIME_MASK) | (c & MNT_ATIME_MASK);
    }
    f
}

/// mount(2).
pub fn mount(s: &Sys<'_>, a: Args, perms: &dyn MountPerms) -> i64 {
    ret(do_mount(s, a, perms).map(|_| 0))
}

fn do_mount(s: &Sys<'_>, a: Args, perms: &dyn MountPerms) -> Result<(), Errno> {
    let fstype = string_arg(s, a[2])?;
    let dev = string_arg(s, a[0])?;
    let data = options_arg(s, a[4])?;
    let dir = s.path(a[1], false)?;
    let c = s.ctx();
    let path = c.lookup(None, &dir, true)?;
    let mut flags = a[3];
    if flags & MS_MGC_MSK == MS_MGC_VAL {
        flags &= !MS_MGC_MSK;
    }
    if flags & MS_NOUSER != 0 {
        return Err(EINVAL);
    }
    if !perms.may_mount() {
        return Err(EPERM);
    }
    let ns = s.ns;
    let remount = flags & MS_REMOUNT != 0;
    let mnt_flags = mnt_flags_of(flags, remount.then(|| path.mnt.flags()));
    if remount && flags & MS_BIND != 0 {
        ns.remount_check(&path, mnt_flags)?;
        ns.remount_apply(&path, mnt_flags, None);
        return Ok(());
    }
    if remount {
        ns.remount_check(&path, mnt_flags)?;
        // do_remount: parse first (EINVAL), then the capability over the super block (EPERM), and only then
        // change anything (reconfigure_super), so a refused call leaves the size as it was.
        let o = mount_fs::split(data.as_deref().unwrap_or(b""))?;
        let size = if path.mnt.sb.fstype == "tmpfs" { tmpfs_new_size(&o, perms)? } else { None };
        if !perms.admin_over(&path.mnt.sb) {
            return Err(EPERM);
        }
        if let Some(p) = size {
            path.mnt.sb.pool.set_size(Some(p));
        }
        ns.remount_apply(&path, mnt_flags, Some(flags & MS_RDONLY != 0));
        return Ok(());
    }
    if flags & MS_BIND != 0 {
        let from = dev.filter(|d| !d.is_empty()).ok_or(EINVAL)?;
        let old = c.lookup(None, &from, true)?;
        ns.bind(&path, &old, flags & MS_REC != 0)?;
        return Ok(());
    }
    if flags & (MS_SHARED | MS_PRIVATE | MS_SLAVE | MS_UNBINDABLE) != 0 {
        if !path.is_mount_root() {
            return Err(EINVAL);
        }
        let ty = match flags & !(MS_REC | MS_SILENT) {
            MS_SHARED => Propagation::Shared,
            MS_PRIVATE => Propagation::Private,
            MS_SLAVE => Propagation::Slave,
            MS_UNBINDABLE => Propagation::Unbindable,
            _ => return Err(EINVAL),
        };
        return ns.change_type(&path, ty, flags & MS_REC != 0);
    }
    if flags & MS_MOVE != 0 {
        let from = dev.filter(|d| !d.is_empty()).ok_or(EINVAL)?;
        let old = c.lookup(None, &from, true)?;
        return ns.move_mount(&old, &path);
    }
    new_mount(s, &path, fstype, dev, data, flags, mnt_flags, perms)
}

/// `shmem_reconfigure`'s parse step: the new size in pages, if one is given (the other options are taken as
/// Linux takes them). Nothing is applied here.
fn tmpfs_new_size(o: &mount_fs::Opts, perms: &dyn MountPerms) -> Result<Option<u64>, Errno> {
    let mut size = None;
    perms.with_mounter(&mut |who| {
        let t = mount_fs::tmpfs_opts(o, who, perms.ram_bytes())?;
        size = t.size_pages;
        Ok(())
    })?;
    Ok(size)
}

/// `do_new_mount`: EINVAL without a type, ENODEV for an unknown one, EINVAL for a bad option, EPERM without
/// the capability for that type or when proc/sysfs would show more than the view shows already.
#[allow(clippy::too_many_arguments)]
fn new_mount(s: &Sys<'_>, at: &Path, fstype: Option<Vec<u8>>, dev: Option<Vec<u8>>, data: Option<Vec<u8>>, flags: u64, mut mnt_flags: u32, perms: &dyn MountPerms) -> Result<(), Errno> {
    let fstype = fstype.ok_or(EINVAL)?;
    let ty = mount_fs::known(&fstype).ok_or(ENODEV)?;
    let o = mount_fs::split(data.as_deref().unwrap_or(b""))?;
    let source = o.source.clone().or(dev).unwrap_or_else(|| b"none".to_vec());
    let overlay = if ty == "overlay" { Some(crate::overlay::parse(&o)?) } else { None };
    let mut made = None;
    perms.with_mounter(&mut |who| {
        made = Some(match ty {
            "tmpfs" => {
                let t = mount_fs::tmpfs_opts(&o, who, perms.ram_bytes())?;
                Some(mount_fs::tmpfs(&t))
            }
            "devpts" => {
                let d = mount_fs::devpts_opts(&o, who)?;
                Some(mount_fs::devpts(&d, who, perms.ptmx()))
            }
            "mqueue" => Some(mount_fs::mqueue(&o, who)?),
            "proc" => {
                mount_fs::proc_opts(&o, who)?;
                // SX-3b: a new super block per mount, bound to the mounter's pid namespace (`proc_get_tree`).
                match perms.pid_ns() {
                    Some(ns) => Some(crate::proc_instance::instance(ns)?),
                    None => None,
                }
            }
            "overlay" => None,
            _ => {
                if !o.rest.is_empty() {
                    return Err(EINVAL);
                }
                None
            }
        });
        Ok(())
    })?;
    if !perms.may_mount_fs(ty) {
        return Err(EPERM);
    }
    if let Some((layers, show)) = overlay {
        // `ovl_get_lowerstack`: each layer looked up as the mounter, a directory.
        let c = s.ctx();
        let mut paths = Vec::new();
        for l in layers.iter() {
            let p = c.lookup(None, l, true)?;
            if !p.dentry.is_dir() {
                return Err(ENOTDIR);
            }
            paths.push(p);
        }
        made = Some(Some(crate::overlay::new(paths, &show)));
    }
    let (sb, root) = match made.flatten() {
        Some(x) => x,
        None => mount_fs::boot_instance(s.ns, ty).ok_or(ENODEV)?,
    };
    let source = alloc::string::String::from_utf8_lossy(&source).into_owned();
    let fresh = sb.mounts.load(core::sync::atomic::Ordering::Acquire) == 0;
    if fresh {
        sb.set_source(&source);
        if let Some(owner) = perms.fs_owner(ty) {
            sb.set_owner(owner);
        }
        sb.set_ro(sb.is_ro() || flags & MS_RDONLY != 0 || o.sb_ro == Some(true));
    }
    // `mount_too_revealing`: proc and sysfs in a view not owned by the initial user namespace only where the
    // view already shows them whole; their locked flags come along.
    if matches!(ty, "proc" | "sysfs") && !perms.ns_owned_by_init() && !s.ns.already_visible(ty, &mut mnt_flags) {
        klog::event!(Info, "vfs", "mount.too_revealing", fs = ty);
        return Err(EPERM);
    }
    let m = s.ns.add_mount_from(at, sb, root, mnt_flags & MNT_USER_SETTABLE, Some(&source))?;
    if mnt_flags & (MNT_LOCK_READONLY | MNT_LOCK_ATIME) != 0 {
        m.set_locks(mnt_flags & (MNT_LOCK_READONLY | MNT_LOCK_ATIME));
    }
    Ok(())
}

/// umount2(2).
pub fn umount2(s: &Sys<'_>, a: Args, perms: &dyn MountPerms) -> i64 {
    ret(do_umount(s, a, perms).map(|_| 0))
}

fn do_umount(s: &Sys<'_>, a: Args, perms: &dyn MountPerms) -> Result<(), Errno> {
    let flags = a[1] as u32;
    if flags & !(MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW) != 0 {
        return Err(EINVAL);
    }
    let name = s.path(a[0], false)?;
    let c = s.ctx();
    // LOOKUP_MOUNTPOINT: the last component steps onto what is mounted there.
    let p = s.ns.cross_down(c.lookup(None, &name, flags & UMOUNT_NOFOLLOW == 0)?);
    if !perms.may_mount() {
        return Err(EPERM);
    }
    if !p.is_mount_root() || !s.ns.check_mnt(&p.mnt) || p.mnt.flags() & MNT_LOCKED != 0 {
        return Err(EINVAL);
    }
    if flags & MNT_FORCE != 0 && !perms.capable_init() {
        return Err(EPERM);
    }
    // `do_umount`: the caller's own root mount is only made read-only (`do_umount_root`).
    if Arc::ptr_eq(&p.mnt, &c.root.mnt) && flags & MNT_DETACH == 0 {
        if flags & MNT_EXPIRE != 0 {
            return Err(EINVAL);
        }
        if !perms.admin_over(&p.mnt.sb) {
            return Err(EPERM);
        }
        p.mnt.sb.set_ro(true);
        klog::event!(Info, "vfs", "umount.root_ro", mount = p.mnt.id);
        return Ok(());
    }
    drop(c);
    s.ns.umount_user(p, flags)
}

/// pivot_root(2).
pub fn pivot_root(s: &Sys<'_>, a: Args, perms: &dyn MountPerms) -> i64 {
    ret(do_pivot(s, a, perms).map(|_| 0))
}

fn do_pivot(s: &Sys<'_>, a: Args, perms: &dyn MountPerms) -> Result<(), Errno> {
    if !perms.may_mount() {
        return Err(EPERM);
    }
    let c = s.ctx();
    let dir = |ptr: u64| -> Result<Path, Errno> {
        let name = s.path(ptr, false)?;
        let p = c.lookup(None, &name, true)?;
        if p.dentry.is_dir() { Ok(p) } else { Err(ENOTDIR) }
    };
    let new = dir(a[0])?;
    let old = dir(a[1])?;
    let root = c.root.clone();
    s.ns.pivot_root(&root, &new, &old)?;
    perms.chroot_fs_refs(&root, &new);
    Ok(())
}
