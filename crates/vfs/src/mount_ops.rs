//! SX-3a: what mount(2), umount2(2) and pivot_root(2) do to a view, with Linux's checks in Linux's order
//! (linux-6.6/fs/namespace.c `do_loopback`, `do_add_mount`, `do_reconfigure_mnt`, `do_remount`,
//! `do_change_type`, `do_move_mount`, `can_umount`/`do_umount`, `pivot_root`, `mnt_already_visible`). The caller
//! (vfs-sys) looked the paths up, copied the arguments and checked the capabilities (`may_mount` and the file
//! system's own); these functions check the tree.

use crate::dentry::{Dentry, SuperBlock};
use crate::mount_ns::*;
use crate::mount_tree::{self as tree, Prop};
use crate::Errno;
use alloc::sync::Arc;
use uapi::errno::*;

/// umount2's flags (include/linux/fs.h).
pub const MNT_FORCE: u32 = 1;
pub const MNT_DETACH: u32 = 2;
pub const MNT_EXPIRE: u32 = 4;
pub const UMOUNT_NOFOLLOW: u32 = 8;

/// A propagation change of mount(2) (`flags_to_propagation_type`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Propagation {
    Shared,
    Slave,
    Private,
    Unbindable,
}

fn locked_ok(m: &Mount, want: u32) -> bool {
    // `can_change_locked_flags`
    let fl = m.flags();
    let keeps = |lock: u32, flag: u32| fl & lock == 0 || want & flag != 0;
    keeps(MNT_LOCK_READONLY, MNT_READONLY)
        && keeps(MNT_LOCK_NODEV, MNT_NODEV)
        && keeps(MNT_LOCK_NOSUID, MNT_NOSUID)
        && keeps(MNT_LOCK_NOEXEC, MNT_NOEXEC)
        && (fl & MNT_LOCK_ATIME == 0 || fl & MNT_ATIME_MASK == want & MNT_ATIME_MASK)
}

impl MountNs {
    /// `lock_mount`: the topmost mount at `p`; ENOENT when the place was removed.
    pub fn lock_mount(&self, p: &Path) -> Result<Path, Errno> {
        let p = cross_down(p.clone());
        if p.dentry.is_unlinked() {
            return Err(ENOENT);
        }
        Ok(p)
    }

    /// `do_loopback` after its lookups: binds `from` (and with `recurse` everything mounted below it) onto the
    /// topmost mount at `at`. EINVAL: `at` or `from` not of this view, `from` unbindable, a non-recursive bind
    /// that would uncover locked mounts; ENOTDIR: a directory onto a file or back; EPERM: a recursive bind over
    /// a mount that is unbindable and locked.
    pub fn bind(&self, at: &Path, from: &Path, recurse: bool) -> Result<Arc<Mount>, Errno> {
        let at = self.lock_mount(at)?;
        let mut g = Grave::default();
        let r = (|| {
            let mut t = TOPO.lock();
            if !self.check_mnt(&at.mnt) {
                return Err(EINVAL);
            }
            let old = &from.mnt;
            if old.flags() & MNT_UNBINDABLE != 0 || !self.check_mnt(old) {
                return Err(EINVAL);
            }
            let locked_below = || tree::children(&t, old).iter().any(|c| c.flags() & MNT_LOCKED != 0 && c.mountpoint().is_some_and(|(_, d)| d.is_descendant_of(&from.dentry)));
            if !recurse && locked_below() {
                return Err(EINVAL);
            }
            let m = if recurse { tree::copy_tree(&mut t, &mut g, old, &from.dentry, 0, self.id)? } else { tree::clone_mnt(old, &from.dentry, 0, self.id) };
            m.store_flags(m.flags() & !MNT_LOCKED);
            if let Err(e) = tree::graft(&mut t, &mut g, &m, &at.mnt, &at.dentry) {
                tree::umount_tree(&mut t, &mut g, &m, false);
                return Err(e);
            }
            Ok(m)
        })();
        drop(g);
        let m = r?;
        klog::event!(Info, "vfs", "mount.bind", ns = self.id, mount = m.id, from = from.mnt.id, rec = recurse, fs = m.sb.fstype);
        Ok(m)
    }

    /// `do_add_mount`: a new file system (`sb` at `root`) onto the topmost mount at `at` with `mnt_flags`.
    /// EINVAL: `at` not in this view, a symlink as root; EBUSY: the same file system already right there.
    pub fn add_mount(&self, at: &Path, sb: Arc<SuperBlock>, root: Arc<Dentry>, mnt_flags: u32) -> Result<Arc<Mount>, Errno> {
        self.add_mount_from(at, sb, root, mnt_flags, None)
    }

    /// `add_mount` with mount(2)'s source name for this mount.
    pub fn add_mount_from(&self, at: &Path, sb: Arc<SuperBlock>, root: Arc<Dentry>, mnt_flags: u32, source: Option<&str>) -> Result<Arc<Mount>, Errno> {
        let place = self.lock_mount(at)?;
        if !self.check_mnt(&place.mnt) {
            return Err(EINVAL);
        }
        if Arc::ptr_eq(&place.mnt.sb, &sb) && place.is_mount_root() {
            return Err(EBUSY);
        }
        if root.inode.is_symlink() {
            return Err(EINVAL);
        }
        let m = Mount::new(sb, root, false);
        m.store_flags(mnt_flags & MNT_USER_SETTABLE);
        m.set_source(source.map(alloc::string::String::from));
        let mut g = Grave::default();
        let r = {
            let mut t = TOPO.lock();
            tree::graft(&mut t, &mut g, &m, &place.mnt, &place.dentry)
        };
        drop(g);
        r?;
        klog::event!(Info, "vfs", "mount", ns = self.id, mount = m.id, fs = m.sb.fstype, dev = m.sb.dev, flags = mnt_flags);
        Ok(m)
    }

    /// The checks MS_REMOUNT shares with MS_REMOUNT|MS_BIND: `p` is a mount root of this view (EINVAL) and the
    /// new flags keep what is locked (EPERM).
    pub fn remount_check(&self, p: &Path, mnt_flags: u32) -> Result<(), Errno> {
        if !self.check_mnt(&p.mnt) || !p.is_mount_root() {
            return Err(EINVAL);
        }
        if !locked_ok(&p.mnt, mnt_flags) {
            return Err(EPERM);
        }
        Ok(())
    }

    /// `set_mount_attributes`: the mount's settable flags become `mnt_flags`; locks and the rest stay. With
    /// `sb_ro` the file system's own read-only state changes too (MS_REMOUNT without MS_BIND).
    pub fn remount_apply(&self, p: &Path, mnt_flags: u32, sb_ro: Option<bool>) {
        let _t = TOPO.lock();
        if let Some(ro) = sb_ro {
            p.mnt.sb.set_ro(ro);
        }
        let f = (p.mnt.flags() & !MNT_USER_SETTABLE) | (mnt_flags & MNT_USER_SETTABLE);
        p.mnt.store_flags(f);
        klog::event!(Info, "vfs", "mount.remount", ns = self.id, mount = p.mnt.id, flags = f, sb_ro = p.mnt.sb.is_ro());
    }

    /// `do_change_type`: `p` must be a mount root (EINVAL); `recurse` changes everything below it too.
    pub fn change_type(&self, p: &Path, ty: Propagation, recurse: bool) -> Result<(), Errno> {
        if !p.is_mount_root() {
            return Err(EINVAL);
        }
        let prop = match ty {
            Propagation::Shared => Prop::Shared,
            Propagation::Slave => Prop::Slave,
            Propagation::Private => Prop::Private,
            Propagation::Unbindable => Prop::Unbindable,
        };
        let mut t = TOPO.lock();
        let list = if recurse { tree::subtree(&t, &p.mnt) } else { alloc::vec![p.mnt.clone()] };
        for m in list.iter() {
            tree::change_type(&mut t, m, prop);
        }
        drop(t);
        klog::event!(Info, "vfs", "mount.propagation", ns = self.id, mount = p.mnt.id, to = alloc::format!("{ty:?}"), rec = recurse, mounts = list.len());
        Ok(())
    }

    /// `do_move_mount` (MS_MOVE): the mount at the mount root `from` moves onto the topmost mount at `to`.
    /// EINVAL: not of this view, locked, not a mount root, a directory onto a file, a shared parent, an
    /// unbindable tree onto a shared place; ELOOP: onto itself or below.
    pub fn move_mount(&self, from: &Path, to: &Path) -> Result<(), Errno> {
        let to = self.lock_mount(to)?;
        let old = from.mnt.clone();
        let mut g = Grave::default();
        let r = (|| {
            let mut t = TOPO.lock();
            let to = tree::topmost(&t, &to);
            let Some((parent, _)) = old.mountpoint() else { return Err(EINVAL) };
            if !self.check_mnt(&to.mnt) || !self.check_mnt(&old) || old.flags() & MNT_LOCKED != 0 || !from.is_mount_root() {
                return Err(EINVAL);
            }
            if to.dentry.is_dir() != from.dentry.is_dir() || parent.propagation().0 != 0 {
                return Err(EINVAL);
            }
            if to.mnt.propagation().0 != 0 && tree::subtree(&t, &old).iter().any(|m| m.flags() & MNT_UNBINDABLE != 0) {
                return Err(EINVAL);
            }
            let mut p = to.mnt.clone();
            loop {
                if Arc::ptr_eq(&p, &old) {
                    return Err(ELOOP);
                }
                match p.mountpoint() {
                    Some((pm, _)) => p = pm,
                    None => break,
                }
            }
            tree::detach(&mut t, &mut g, &old);
            tree::graft(&mut t, &mut g, &old, &to.mnt, &to.dentry)
        })();
        drop(g);
        r?;
        klog::event!(Info, "vfs", "mount.move", ns = self.id, mount = from.mnt.id, to = to.mnt.id);
        Ok(())
    }

    /// `can_umount` after `may_mount` + `do_umount` for the mount root `p` (found with `LOOKUP_MOUNTPOINT`, the
    /// caller's lookup reference given here). `is_fs_root`: `p`'s mount is the caller's root mount (Linux then
    /// only remounts it read-only, which the caller decides). EINVAL: not a mount root, not of this view, locked,
    /// MNT_EXPIRE with MNT_FORCE/MNT_DETACH or on the root; EAGAIN: the first MNT_EXPIRE; EBUSY: in use.
    pub fn umount_user(&self, p: Path, flags: u32) -> Result<(), Errno> {
        if !p.is_mount_root() || !self.check_mnt(&p.mnt) || p.mnt.flags() & MNT_LOCKED != 0 {
            return Err(EINVAL);
        }
        let m = p.mnt.clone();
        drop(p);
        if flags & MNT_EXPIRE != 0 {
            if flags & (MNT_FORCE | MNT_DETACH) != 0 || m.mountpoint().is_none() {
                return Err(EINVAL);
            }
            let first = !core::mem::replace(&mut m.st.lock().expire, true);
            if first {
                return Err(EAGAIN);
            }
        }
        let mut g = Grave::default();
        let r = (|| {
            let mut t = TOPO.lock();
            if m.flags() & MNT_LOCKED != 0 {
                return Err(EINVAL);
            }
            if m.mountpoint().is_none() {
                // The view's root: only a lazy unmount takes it, and nothing changes (Linux keeps a root it
                // cannot detach from its namespace list).
                return if flags & MNT_DETACH != 0 { Ok(()) } else { Err(EBUSY) };
            }
            if flags & MNT_DETACH == 0 {
                // `propagate_mount_busy(mnt, 2)`: the table's reference and ours; anything more (an open file,
                // a cwd, a mount below) is a user.
                if Arc::strong_count(&m) > 2 || !tree::children(&t, &m).is_empty() {
                    return Err(EBUSY);
                }
            }
            tree::umount_tree(&mut t, &mut g, &m, true);
            Ok(())
        })();
        drop(g);
        r?;
        klog::event!(Info, "vfs", "umount", ns = self.id, mount = m.id, fs = m.sb.fstype, lazy = flags & MNT_DETACH != 0);
        Ok(())
    }

    /// pivot_root(2) after its lookups (`root` the caller's root, `new` and `old` found with LOOKUP_DIRECTORY):
    /// the mount at `new` becomes the root, the old root goes onto `old`. Linux's order: EINVAL (a shared place,
    /// a mount not of this view, `new` locked), ENOENT (`new` removed), EBUSY (`new` or `old` on the root
    /// mount), EINVAL (`root` or `new` not a mount root, `new` not attached, `old` not below `new`, `new` not
    /// below `root`). When the root mount is the view's root (V2's views have no rootfs below `/`), `new`
    /// becomes the view's root. The caller re-points roots and cwds (`chroot_fs_refs`).
    pub fn pivot_root(&self, root: &Path, new: &Path, old: &Path) -> Result<(), Errno> {
        let old = self.lock_mount(old)?;
        let mut g = Grave::default();
        let r = (|| {
            let mut t = TOPO.lock();
            // The old root's place, stepped up under the lock: nothing can sit on it when it is attached below,
            // and the root's own place is free once the root is detached. Both attaches below therefore find
            // their places free; every refusal comes before the first change.
            let old = tree::topmost(&t, &old);
            let (new_mnt, root_mnt, old_mnt) = (&new.mnt, &root.mnt, &old.mnt);
            let ex_parent = new_mnt.mountpoint();
            let root_place = root_mnt.mountpoint();
            let shared = |m: &Option<(Arc<Mount>, Arc<Dentry>)>| m.as_ref().is_some_and(|(p, _)| p.propagation().0 != 0);
            if old_mnt.propagation().0 != 0 || shared(&ex_parent) || shared(&root_place) {
                return Err(EINVAL);
            }
            if !self.check_mnt(root_mnt) || !self.check_mnt(new_mnt) || new_mnt.flags() & MNT_LOCKED != 0 {
                return Err(EINVAL);
            }
            if new.dentry.is_unlinked() {
                return Err(ENOENT);
            }
            if Arc::ptr_eq(new_mnt, root_mnt) || Arc::ptr_eq(old_mnt, root_mnt) {
                return Err(EBUSY);
            }
            let is_view_root = Arc::ptr_eq(root_mnt, &self.root_mount());
            if !root.is_mount_root() || (root_place.is_none() && !is_view_root) {
                return Err(EINVAL);
            }
            if !new.is_mount_root() || ex_parent.is_none() {
                return Err(EINVAL);
            }
            if !reachable(old_mnt, &old.dentry, new) || !reachable(new_mnt, &new.dentry, root) {
                return Err(EINVAL);
            }
            tree::detach(&mut t, &mut g, new_mnt);
            match &root_place {
                Some(_) => tree::detach(&mut t, &mut g, root_mnt),
                None => {
                    // The view's root moves: its table entry goes to `new_mnt`.
                    g.mounts.extend(t.put((0, self.id), new_mnt));
                    g.mounts.push(self.set_root_mount(new_mnt.clone()));
                }
            }
            if root_mnt.flags() & MNT_LOCKED != 0 {
                new_mnt.store_flags(new_mnt.flags() | MNT_LOCKED);
                root_mnt.store_flags(root_mnt.flags() & !MNT_LOCKED);
            }
            tree::attach(&mut t, &mut g, root_mnt, old_mnt, &old.dentry)?;
            if let Some((rp, rd)) = &root_place {
                tree::attach(&mut t, &mut g, new_mnt, rp, rd)?;
            }
            Ok(())
        })();
        drop(g);
        r?;
        klog::event!(Info, "vfs", "pivot_root", ns = self.id, new = new.mnt.id, old_root = root.mnt.id, at = old.mnt.id);
        Ok(())
    }

    /// `mnt_already_visible`: this view already shows a whole mount of a file system of `fstype` (its root
    /// is the file system's root, nothing locked hides part of it) whose locked flags allow `new_flags`; its
    /// locks carry over to `new_flags`.
    pub fn already_visible(&self, fstype: &str, new_flags: &mut u32) -> bool {
        let t = TOPO.lock();
        for m in t.view(self.id).iter().filter(|m| m.sb.fstype == fstype) {
            if m.root.parent().is_some() {
                continue;
            }
            let mut f = m.flags();
            if m.sb.is_ro() {
                f |= MNT_LOCK_READONLY;
            }
            if f & MNT_LOCK_READONLY != 0 && *new_flags & MNT_READONLY == 0 {
                continue;
            }
            if f & MNT_LOCK_ATIME != 0 && f & MNT_ATIME_MASK != *new_flags & MNT_ATIME_MASK {
                continue;
            }
            // Only an empty directory may be covered by a locked mount (Linux: a permanently empty one).
            let hides = tree::children(&t, m).iter().any(|c| c.flags() & MNT_LOCKED != 0 && c.mountpoint().is_some_and(|(_, d)| !d.is_dir() || d.child_count() != 0));
            if hides {
                continue;
            }
            *new_flags |= f & (MNT_LOCK_READONLY | MNT_LOCK_ATIME);
            return true;
        }
        false
    }
}
