//! FS-1b: changing a node's mode, owner and times (chmod, chown, utimensat and their relatives) as Linux's
//! `notify_change` does (linux-6.6/fs/attr.c:381): `may_setattr`, the time stamps, the suid/sgid kill bits,
//! `setattr_prepare` (owner and CAP rules), the file system's own `setattr` (FAT: `FsOps::setattr`), then
//! `setattr_copy`. V2's credentials have no capability set and no supplementary groups: CAP_CHOWN, CAP_FOWNER
//! and CAP_FSETID are uid 0, group membership is the caller's gid.

use crate::abi::{Time, S_ISGID, S_ISUID};
use crate::inode::{self, Cred, Meta, MAY_WRITE};
use crate::mount_ns::Path;
use crate::ops::Ctx;
use crate::Errno;
use core::sync::atomic::Ordering;
use uapi::errno::*;

// `ia_valid` bits (linux-6.6/include/linux/fs.h).
pub const ATTR_MODE: u32 = 1 << 0;
pub const ATTR_UID: u32 = 1 << 1;
pub const ATTR_GID: u32 = 1 << 2;
pub const ATTR_ATIME: u32 = 1 << 4;
pub const ATTR_MTIME: u32 = 1 << 5;
pub const ATTR_CTIME: u32 = 1 << 6;
pub const ATTR_ATIME_SET: u32 = 1 << 7;
pub const ATTR_MTIME_SET: u32 = 1 << 8;
pub const ATTR_KILL_SUID: u32 = 1 << 11;
pub const ATTR_KILL_SGID: u32 = 1 << 12;
pub const ATTR_TIMES_SET: u32 = 1 << 16;
pub const ATTR_TOUCH: u32 = 1 << 17;

/// `struct iattr`: what changes (`valid`) and to what. `mode` holds the permission bits (07777).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetAttr {
    pub valid: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: Time,
    pub mtime: Time,
    pub ctime: Time,
}

/// UTIME_NOW / UTIME_OMIT in a timespec's nanoseconds.
pub const UTIME_NOW: i64 = (1 << 30) - 1;
pub const UTIME_OMIT: i64 = (1 << 30) - 2;

fn owner_or_capable(m: &Meta, c: Cred) -> bool {
    c.uid == m.uid() || c.uid == 0
}

fn in_group_or_capable(c: Cred, gid: u32) -> bool {
    crate::inode::in_group(c, gid) || c.uid == 0
}

/// chmod_common: the new permission bits with the type kept.
pub fn chmod(mode: u32) -> SetAttr {
    SetAttr { valid: ATTR_MODE | ATTR_CTIME, mode: mode & 0o7777, ..Default::default() }
}

/// chown_common (linux-6.6/fs/open.c:751): -1 keeps that id; a non-directory loses its suid bit and, per
/// `setattr_should_drop_sgid`, its sgid bit.
pub fn chown(m: &Meta, c: Cred, uid: u32, gid: u32) -> SetAttr {
    let mut a = SetAttr { valid: ATTR_CTIME, ..Default::default() };
    if uid != u32::MAX {
        a.valid |= ATTR_UID;
        a.uid = uid;
    }
    if gid != u32::MAX {
        a.valid |= ATTR_GID;
        a.gid = gid;
    }
    if !m.is_dir() {
        a.valid |= ATTR_KILL_SUID;
        let mode = m.mode();
        if mode & S_ISGID != 0 && (mode & 0o010 != 0 || !in_group_or_capable(c, m.gid())) {
            a.valid |= ATTR_KILL_SGID;
        }
    }
    a
}

/// vfs_utimes (linux-6.6/fs/utimes.c:22): `times` = [atime, mtime] as (sec, nsec), None = both now.
/// EINVAL for a nanosecond value out of range. Both UTIME_NOW is the same as None.
pub fn utimes(times: Option<[(i64, i64); 2]>) -> Result<SetAttr, Errno> {
    let valid_ns = |n: i64| n == UTIME_NOW || n == UTIME_OMIT || (0..=999_999_999).contains(&n);
    let times = match times {
        Some(t) if !valid_ns(t[0].1) || !valid_ns(t[1].1) => return Err(EINVAL),
        Some(t) if t[0].1 == UTIME_NOW && t[1].1 == UTIME_NOW => None,
        t => t,
    };
    let mut a = SetAttr { valid: ATTR_CTIME | ATTR_MTIME | ATTR_ATIME, ..Default::default() };
    match times {
        None => a.valid |= ATTR_TOUCH,
        Some([(asec, ans), (msec, mns)]) => {
            if ans == UTIME_OMIT {
                a.valid &= !ATTR_ATIME;
            } else if ans != UTIME_NOW {
                a.atime = Time { sec: asec, nsec: ans as u32 };
                a.valid |= ATTR_ATIME_SET;
            }
            if mns == UTIME_OMIT {
                a.valid &= !ATTR_MTIME;
            } else if mns != UTIME_NOW {
                a.mtime = Time { sec: msec, nsec: mns as u32 };
                a.valid |= ATTR_MTIME_SET;
            }
            a.valid |= ATTR_TIMES_SET;
        }
    }
    Ok(a)
}

/// `setattr_prepare` without ATTR_SIZE (truncate has its own path).
fn prepare(m: &Meta, c: Cred, a: &mut SetAttr) -> Result<(), Errno> {
    if a.valid & ATTR_UID != 0 && !((c.uid == m.uid() && a.uid == m.uid()) || c.uid == 0) {
        return Err(EPERM);
    }
    if a.valid & ATTR_GID != 0 && !((c.uid == m.uid() && (a.gid == m.gid() || a.gid == c.gid)) || c.uid == 0) {
        return Err(EPERM);
    }
    if a.valid & ATTR_MODE != 0 {
        if !owner_or_capable(m, c) {
            return Err(EPERM);
        }
        let gid = if a.valid & ATTR_GID != 0 { a.gid } else { m.gid() };
        if !in_group_or_capable(c, gid) {
            a.mode &= !S_ISGID;
        }
    }
    if a.valid & (ATTR_MTIME_SET | ATTR_ATIME_SET | ATTR_TIMES_SET) != 0 && !owner_or_capable(m, c) {
        return Err(EPERM);
    }
    Ok(())
}

/// `notify_change` on `m` (`immutable`: Linux S_IMMUTABLE). `fs` is the file system's `setattr` after the
/// generic checks: it may refuse, adjust `a` (drop a mode change, round times) and write the change to its disk.
pub fn notify_change(m: &Meta, immutable: bool, c: Cred, a: SetAttr, fs: impl FnOnce(&mut SetAttr) -> Result<(), Errno>) -> Result<(), Errno> {
    let Some(mut a) = checks(m, immutable, c, a)? else { return Ok(()) };
    fs(&mut a)?;
    // setattr_copy: the sgid bit by the group the node has after this change.
    if a.valid & ATTR_MODE != 0 {
        let gid = if a.valid & ATTR_GID != 0 { a.gid } else { m.gid() };
        if !in_group_or_capable(c, gid) {
            a.mode &= !S_ISGID;
        }
    }
    m.apply(&a);
    Ok(())
}

/// `may_setattr` and the symlink-mode refusal: what `notify_change` asks before any file system's `setattr`.
pub fn may_setattr(m: &Meta, immutable: bool, c: Cred, a: &SetAttr) -> Result<(), Errno> {
    if a.valid & (ATTR_MODE | ATTR_UID | ATTR_GID | ATTR_TIMES_SET) != 0 && immutable {
        return Err(EPERM);
    }
    if a.valid & ATTR_TOUCH != 0 {
        if immutable {
            return Err(EPERM);
        }
        if !owner_or_capable(m, c) {
            m.permission(c, MAY_WRITE)?;
        }
    }
    if a.valid & ATTR_MODE != 0 && m.file_type() == uapi::S_IFLNK {
        return Err(EOPNOTSUPP);
    }
    Ok(())
}

/// The generic part of `notify_change` up to the file system's hook: `may_setattr`, the times, the kill bits,
/// `setattr_prepare`. None: nothing is left to change.
fn checks(m: &Meta, immutable: bool, c: Cred, mut a: SetAttr) -> Result<Option<SetAttr>, Errno> {
    may_setattr(m, immutable, c, &a)?;
    let now = inode::now();
    a.ctime = now;
    if a.valid & ATTR_ATIME_SET == 0 {
        a.atime = now;
    }
    if a.valid & ATTR_MTIME_SET == 0 {
        a.mtime = now;
    }
    let mode = m.mode();
    if a.valid & ATTR_KILL_SUID != 0 && mode & S_ISUID != 0 {
        a.valid |= ATTR_MODE;
        a.mode = mode & 0o7777 & !S_ISUID;
    }
    if a.valid & ATTR_KILL_SGID != 0 && mode & S_ISGID != 0 {
        if a.valid & ATTR_MODE == 0 {
            a.valid |= ATTR_MODE;
            a.mode = mode & 0o7777;
        }
        a.mode &= !S_ISGID;
    }
    if a.valid & !(ATTR_KILL_SUID | ATTR_KILL_SGID) == 0 {
        return Ok(None);
    }
    prepare(m, c, &mut a)?;
    Ok(Some(a))
}

impl Ctx<'_> {
    /// A setattr on a named node: `mnt_want_write` (EROFS on a read-only mount), the B3 question (a change of
    /// the node counts as a write), the freeze guard, then `notify_change` under the super block's names lock
    /// (Linux's inode lock; it also serialises the file system's hook with its name operations).
    /// `build` makes the change from the node as it is under the lock (chown's kill bits read its mode).
    /// Linux's own answers (EPERM, EACCES) come before the B3 question, which may wait and so is asked
    /// without the lock; the checks run again under it.
    pub fn setattr(&self, p: &Path, build: impl Fn(&Meta) -> SetAttr) -> Result<(), Errno> {
        if p.mnt.readonly {
            return Err(EROFS);
        }
        let i = &p.dentry.inode;
        let immutable = i.immutable.load(Ordering::Relaxed);
        checks(&i.meta, immutable, self.cred, build(&i.meta))?;
        self.policy(p, None, true, false)?;
        let sb = &p.dentry.sb;
        let _w = sb.write_begin()?;
        let _g = sb.lock_names();
        let a = build(&i.meta);
        let r = notify_change(&i.meta, immutable, self.cred, a, |a| {
            if sb.fstype == "proc" && proc_refuses(&p.dentry, a.valid) {
                return Err(EPERM);
            }
            if a.valid & ATTR_MODE != 0 {
                // shmem_setattr (linux-6.6/mm/shmem.c:1166): F_SEAL_EXEC keeps the exec bits.
                crate::memfd::check_mode_change(i, (i.meta.mode() & uapi::S_IFMT) | a.mode)?;
            }
            match &sb.ops {
                Some(o) => o.setattr(&p.dentry, a),
                None => Ok(()),
            }
        });
        match r {
            Err(e) => klog::event!(Debug, "vfs", "setattr.refused", ino = i.meta.ino, fs = sb.fstype, valid = a.valid, errno = e),
            // FS-14: `fsnotify_change` after a change (nothing to change: no event, as `notify_change` returns first).
            Ok(()) if a.valid & !(ATTR_KILL_SUID | ATTR_KILL_SGID) != 0 => crate::inotify::hooks::changed(&p.dentry, a.valid, false),
            Ok(()) => {}
        }
        r
    }
}

/// /proc's own setattr answers: `proc_setattr` (linux-6.6/fs/proc/base.c:689) refuses a mode change under
/// /proc/<pid>, /proc/self and /proc/thread-self; `proc_sys_setattr` (fs/proc/proc_sysctl.c:826) also an owner
/// change under /proc/sys. The other /proc entries take both (`proc_notify_change`, fs/proc/generic.c:118).
fn proc_refuses(d: &alloc::sync::Arc<crate::dentry::Dentry>, valid: u32) -> bool {
    let mut cur = d.clone();
    let mut below = None;
    while let Some(up) = cur.parent() {
        below = Some(cur);
        cur = up;
    }
    let Some(top) = below else { return false };
    let n = top.name();
    if &n[..] == b"sys" {
        return valid & (ATTR_MODE | ATTR_UID | ATTR_GID) != 0;
    }
    valid & ATTR_MODE != 0 && (n.iter().all(u8::is_ascii_digit) || matches!(&n[..], b"self" | b"thread-self"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: Cred = Cred { uid: 1000, gid: 1000 };

    fn meta(mode: u32, uid: u32, gid: u32) -> Meta {
        Meta::new(7, uapi::S_IFREG | mode, Cred { uid, gid })
    }

    #[test]
    fn chmod_rules() {
        let m = meta(0o644, 1000, 1000);
        notify_change(&m, false, USER, chmod(0o4755), |_| Ok(())).unwrap();
        assert_eq!(m.mode(), uapi::S_IFREG | 0o4755);
        // Not the owner: EPERM; root may.
        assert_eq!(notify_change(&m, false, Cred { uid: 1001, gid: 1000 }, chmod(0o600), |_| Ok(())), Err(EPERM));
        notify_change(&m, false, Cred::ROOT, chmod(0o600), |_| Ok(())).unwrap();
        // sgid for a group the owner is not in is dropped.
        let g = meta(0o644, 1000, 50);
        notify_change(&g, false, USER, chmod(0o2755), |_| Ok(())).unwrap();
        assert_eq!(g.mode() & 0o7777, 0o755);
        assert_eq!(notify_change(&m, true, Cred::ROOT, chmod(0o600), |_| Ok(())), Err(EPERM), "immutable");
        let l = Meta::new(8, uapi::S_IFLNK | 0o777, USER);
        assert_eq!(notify_change(&l, false, USER, chmod(0o600), |_| Ok(())), Err(EOPNOTSUPP));
    }

    #[test]
    fn chown_rules() {
        let m = meta(0o6755, 1000, 1000);
        // The owner may "change" to its own uid and to its own group; chown kills suid and the group-x sgid.
        notify_change(&m, false, USER, chown(&m, USER, 1000, 1000), |_| Ok(())).unwrap();
        assert_eq!(m.mode() & 0o7777, 0o755);
        assert_eq!(notify_change(&m, false, USER, chown(&m, USER, 0, u32::MAX), |_| Ok(())), Err(EPERM));
        assert_eq!(notify_change(&m, false, USER, chown(&m, USER, u32::MAX, 5), |_| Ok(())), Err(EPERM));
        notify_change(&m, false, Cred::ROOT, chown(&m, Cred::ROOT, 5, 6), |_| Ok(())).unwrap();
        assert_eq!((m.uid(), m.gid()), (5, 6));
        // A directory keeps its bits.
        let d = Meta::new(9, uapi::S_IFDIR | 0o2775, USER);
        notify_change(&d, false, USER, chown(&d, USER, u32::MAX, u32::MAX), |_| Ok(())).unwrap();
        assert_eq!(d.mode() & 0o7777, 0o2775);
    }

    #[test]
    fn utimes_rules() {
        assert_eq!(utimes(Some([(0, 1_000_000_000), (0, 0)])), Err(EINVAL));
        let m = meta(0o666, 1, 1);
        // Explicit times need the owner (EPERM); "now" needs write access only.
        let t = utimes(Some([(100, 5), (200, UTIME_OMIT)])).unwrap();
        assert_eq!(notify_change(&m, false, USER, t, |_| Ok(())), Err(EPERM));
        notify_change(&m, false, USER, utimes(None).unwrap(), |_| Ok(())).unwrap();
        notify_change(&m, false, Cred { uid: 1, gid: 1 }, t, |_| Ok(())).unwrap();
        let (a, mt, _) = m.times();
        assert_eq!(a, Time { sec: 100, nsec: 5 });
        assert_ne!(mt, Time { sec: 200, nsec: 0 }, "UTIME_OMIT keeps mtime");
        let ro = meta(0o644, 1, 1);
        assert_eq!(notify_change(&ro, false, USER, utimes(None).unwrap(), |_| Ok(())), Err(EACCES));
        assert_eq!(utimes(Some([(0, UTIME_NOW), (0, UTIME_NOW)])).unwrap().valid & ATTR_TOUCH, ATTR_TOUCH);
    }
}
