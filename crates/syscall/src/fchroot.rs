//! L7-PROC: fchroot(fd, flags) (linux-7.3.1/fs/open.c:621, syscall 472): chroot to an open directory, or with
//! FD_FAILFS_ROOT to the root of failfs (fs/failfs.c), where every operation fails with EOPNOTSUPP. Without
//! CAP_SYS_CHROOT failfs is allowed only to a no_new_privs caller whose cwd/root state nobody else shares and
//! that is not chrooted yet (moving there would lift the old root's ".." barrier).
//!
//! V2's failfs root is a synthetic directory whose permission op, lookups, listing and open answer EOPNOTSUPP,
//! and a walk that jumps onto it ("/" as the root: stat, chdir, an O_PATH open) is refused too
//! (`failfs_d_weak_revalidate`, V2's `DirSource::jumped_to`).

use crate::SysCtx;
use alloc::sync::Arc;
use alloc::vec::Vec;
use proc::cred::Cap;
use uapi::ab::pidfd::FD_FAILFS_ROOT;
use uapi::errno::*;
use vfs::Errno;
use vfs::synth::{DirSource, SynthEntry};

/// `failfs_lookup`, `failfs_iterate`, `failfs_permission`, `failfs_d_weak_revalidate`: EOPNOTSUPP for everything.
struct FailFs;

impl DirSource for FailFs {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Err(EOPNOTSUPP)
    }
    fn lookup(&self, _name: &[u8]) -> Result<Option<SynthEntry>, Errno> {
        Err(EOPNOTSUPP)
    }
    fn may_open(&self) -> Result<(), Errno> {
        Err(EOPNOTSUPP)
    }
    fn permission(&self, _who: vfs::Cred, _want: u32) -> Option<Result<(), Errno>> {
        Some(Err(EOPNOTSUPP))
    }
    fn jumped_to(&self) -> Result<(), Errno> {
        Err(EOPNOTSUPP)
    }
}

/// failfs's root (`failfs_get_root`): one directory below a private empty view, made once (the view is kept
/// with it).
fn failfs_root() -> vfs::Path {
    static ROOT: ksync::Once<(Arc<vfs::MountNs>, vfs::Path)> = ksync::Once::new();
    ROOT.get_or_init(|| {
        let view = Arc::new(vfs::MountNs::new_empty());
        let base = view.root_path();
        let sd = vfs::synth::SynthDir::new();
        sd.add(Arc::new(FailFs));
        let inode = vfs::inode::Inode::new(1, uapi::S_IFDIR | 0o555, vfs::Cred::ROOT, vfs::inode::Data::SynthDir(Arc::new(sd)));
        let dentry = vfs::dentry::Dentry::ephemeral(&base.dentry, b"failfs", inode);
        let path = vfs::Path { mnt: base.mnt, dentry };
        (view, path)
    })
    .1
    .clone()
}

fn sys_fchroot(c: &mut SysCtx<'_>) -> i64 {
    let (fd, flags) = (c.args[0] as i32, c.args[1] as u32);
    if flags != 0 {
        return -EINVAL;
    }
    let p = c.proc;
    let fs = c.thread.fs(p);
    let privileged = proc::cred::own_capable(&p.cred(), Cap::SysChroot);
    let root = if fd == FD_FAILFS_ROOT {
        if !privileged {
            if !c.thread.prctl().no_new_privs() {
                return -EPERM;
            }
            // `current->fs->users != 1`: a sibling sharing the state could exec a set-id program past the check.
            let here = p.group().threads().iter().any(|o| !Arc::ptr_eq(o, c.thread) && Arc::ptr_eq(&o.fs(p), &fs));
            if here || proc::fs_shared(&fs, p.pid) {
                return -EINVAL;
            }
            if p.chrooted(c.thread) {
                return -EPERM;
            }
        }
        failfs_root()
    } else {
        // `fd_raw`: an O_PATH fd counts.
        let Some(f) = (fd >= 0).then(|| p.fds.get(fd as usize).ok()).flatten() else { return -EBADF };
        let Some(path) = f.path.clone().filter(|x| x.dentry.is_dir()) else { return -ENOTDIR };
        // `file_permission(MAY_EXEC | MAY_CHDIR)`: the search bit.
        if let Err(e) = path.dentry.inode.permission(p.fs_cred(), vfs::inode::MAY_EXEC) {
            return -e;
        }
        if !privileged {
            klog::event!(Info, "ns", "chroot.refused", pid = p.pid, uid = p.cred().euid);
            return -EPERM;
        }
        path
    };
    crate::ns_sys::set_root(&fs, p.pid, root)
}
crate::syscall_nr!(uapi::ab::nr_more::FCHROOT, "fchroot", sys_fchroot);
