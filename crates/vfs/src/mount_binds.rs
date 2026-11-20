//! UPD-4: a kernel mount that brings binds of its own directories with it, grafted in one step (Linux grafts a
//! prepared tree the same way: linux-7.3.1/fs/namespace.c:2634 `attach_recursive_mnt`, used by :3106 `do_loopback`
//! for a recursive bind). The boot volume is mounted at /boot with the running version's parts already bound over
//! their places: no program ever sees /boot without them. And its recursive unmount (:1831 `umount_tree`, as
//! `umount -l` detaches a whole subtree).

use crate::dentry::{Dentry, SuperBlock};
use crate::mount_ns::*;
use crate::mount_tree as tree;
use crate::Errno;
use alloc::sync::Arc;
use core::sync::atomic::Ordering;
use uapi::errno::*;

impl MountNs {
    /// `mount` of `sb`'s `root` on `at`, with every `(place, root)` of `binds` mounted on `place` (a directory of
    /// `sb` below `root`) showing `root` (another directory of `sb`), all visible at once. ENOTDIR: a place or
    /// a root is no directory; EBUSY: a place named twice.
    pub fn mount_with_binds(&self, at: &Path, sb: Arc<SuperBlock>, root: Arc<Dentry>, readonly: bool, binds: &[(Arc<Dentry>, Arc<Dentry>)]) -> Result<Arc<Mount>, Errno> {
        if !at.dentry.is_dir() || !root.is_dir() || binds.iter().any(|(p, r)| !p.is_dir() || !r.is_dir()) {
            return Err(ENOTDIR);
        }
        let at = cross_down(at.clone());
        if at.dentry.is_unlinked() {
            return Err(ENOENT);
        }
        if sb.mounts.load(Ordering::Acquire) == 0 && readonly {
            sb.set_ro(true);
        }
        let m = Mount::new(sb.clone(), root, readonly);
        let mut g = Grave::default();
        let r = (|| {
            let mut t = TOPO.lock();
            if at.mnt.ns_id() != self.id {
                return Err(EINVAL);
            }
            for (place, broot) in binds {
                let c = Mount::new(sb.clone(), broot.clone(), readonly);
                if let Err(e) = tree::attach(&mut t, &mut g, &c, &m, place) {
                    tree::umount_tree(&mut t, &mut g, &m, false);
                    g.mounts.push(c);
                    return Err(e);
                }
            }
            if let Err(e) = tree::graft(&mut t, &mut g, &m, &at.mnt, &at.dentry) {
                tree::umount_tree(&mut t, &mut g, &m, false);
                return Err(e);
            }
            Ok(())
        })();
        drop(g);
        r?;
        klog::event!(Info, "vfs", "mount", ns = self.id, mount = m.id, fs = m.sb.fstype, dev = m.sb.dev, ro = readonly, binds = binds.len());
        Ok(m)
    }

    /// The kernel's unmount of the mount root `p` together with everything mounted below it.
    pub fn umount_recursive(&self, p: &Path) -> Result<(), Errno> {
        if !p.is_mount_root() || Arc::ptr_eq(&p.mnt, &self.root_mount()) {
            return Err(EINVAL);
        }
        let mut g = Grave::default();
        let r = {
            let mut t = TOPO.lock();
            if p.mnt.ns_id() != self.id || p.mnt.mountpoint().is_none() {
                Err(EINVAL)
            } else {
                tree::umount_tree(&mut t, &mut g, &p.mnt, true);
                Ok(())
            }
        };
        drop(g);
        r?;
        klog::event!(Info, "vfs", "umount", ns = self.id, mount = p.mnt.id, fs = p.mnt.sb.fstype, recursive = true);
        Ok(())
    }
}
