//! SX-3a: mount namespaces (linux-6.6/fs/namespace.c `alloc_mnt_ns`, `copy_mnt_ns`, `mntns_install`,
//! `chroot_fs_refs`): a `MntNs` is FS's view (`vfs::MountNs`) with what the process layer knows about it, its
//! nsfs inode number and its owning user namespace. The view itself (mounts, propagation, locks) is vfs's.
//!
//! The view's owner key for vfs is the owning user namespace's inode number (0 for the initial one): vfs
//! compares keys to decide whether a copy or a propagated mount crosses into a less privileged view.

use super::count::{self, Charge, Counted};
use super::user::{self, UserNs};
use crate::Process;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use ksync::Once;
use vfs::{Errno, MountNs, Path};

pub struct MntNs {
    pub inum: u32,
    pub user_ns: Arc<UserNs>,
    pub view: Arc<MountNs>,
    _charge: Option<Charge>,
}

static INIT: Once<Arc<MntNs>> = Once::new();
static LIVE: AtomicUsize = AtomicUsize::new(0);

impl Drop for MntNs {
    fn drop(&mut self) {
        if self._charge.is_some() {
            LIVE.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// The key vfs knows `ns` by (0: the initial user namespace).
pub fn owner_key(ns: &UserNs) -> u32 {
    if ns.is_init() { 0 } else { ns.inum }
}

/// The initial mount namespace: the boot view.
pub fn init_mnt() -> &'static Arc<MntNs> {
    INIT.get_or_init(|| Arc::new(MntNs { inum: super::alloc_inum(), user_ns: user::init_user_ns().clone(), view: vfs::root_ns(), _charge: None }))
}

/// Mount namespaces made after boot that still live (ktests).
pub fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/// `copy_mnt_ns` for CLONE_NEWNS: a copy of `old`'s view owned by `user_ns`, charged to `euid` (ENOSPC past
/// `max_mnt_namespaces`). Another owner gets slaves of shared mounts and a locked copy (vfs `copy_for`).
pub(crate) fn copy(old: &MntNs, user_ns: &Arc<UserNs>, euid: u32) -> Result<Arc<MntNs>, Errno> {
    let c = count::charge(Counted::Mnt, user_ns, euid)?;
    let view = Arc::new(old.view.copy_for(owner_key(user_ns)));
    LIVE.fetch_add(1, Ordering::Relaxed);
    let n = Arc::new(MntNs { inum: super::alloc_inum(), user_ns: user_ns.clone(), view, _charge: Some(c) });
    klog::event!(Info, "ns", "mnt.new", inum = n.inum, view = n.view.id, owner = owner_key(user_ns));
    Ok(n)
}

/// The copier's own state moves into the new view at once (`copy_mnt_ns`'s second pass); a root or cwd that
/// was the old view's root is set explicitly, so callers that still fall back to `Process::ns` (the boot
/// view) find the new view's places.
pub(crate) fn enter(fs: &vfs::FsState, ns: &MntNs) {
    fs.move_into(&ns.view);
    if fs.root().is_none() {
        fs.set_root(ns.view.root_path());
    }
    if fs.cwd().is_none() {
        fs.set_cwd(ns.view.root_path());
    }
}

/// `mntns_install`'s root: the view's root with whatever is mounted on it (`LOOKUP_DOWN`).
pub(crate) fn top_root(ns: &MntNs) -> Path {
    ns.view.cross_down(ns.view.root_path())
}

/// `chroot_fs_refs` (fs/fs_struct.c): every task whose root or cwd is `old` gets `new` instead (pivot_root).
/// The processes are collected first, their states changed outside `proc.table`.
pub fn chroot_fs_refs(old: &Path, new: &Path) -> usize {
    let mut states: Vec<Arc<vfs::FsState>> = Vec::new();
    for p in crate::processes() {
        if p.is_released() {
            continue;
        }
        states.push(p.image_fs());
        states.extend(p.threads().iter().filter_map(|t| t.own_fs()));
    }
    states.sort_by_key(|s| Arc::as_ptr(s) as usize);
    states.dedup_by_key(|s| Arc::as_ptr(s) as usize);
    let mut n = 0;
    for s in states {
        n += s.replace_refs(old, new);
    }
    n
}

impl Process {
    /// SX-3a: the mount view this process's calls use (Linux `current->nsproxy->mnt_ns`). `Process::ns` stays
    /// the boot view the process was started in.
    pub fn mount_ns(&self) -> Arc<MountNs> {
        self.nsset().mnt.view.clone()
    }
    /// The process's mount namespace.
    pub fn mnt_ns(&self) -> Arc<MntNs> {
        self.nsset().mnt.clone()
    }
}
