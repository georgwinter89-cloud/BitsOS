//! tmpfs: the root file system and, since FS-5, the instances at `/tmp` and `/dev/shm`. Names live in the
//! dentry tree (`dentry.rs`), content in the page cache of each file (`pagecache.rs`); everything is RAM.
//! Every instance has its own pool (`SuperBlock::pool`): files are charged when their size grows, against the
//! system pool (ENOMEM) and the instance's own size if one is set (ENOSPC, Linux's `size=`; none by default:
//! `/tmp` and `/dev/shm` are bounded only by the pool, V2-PLAN R2).

use crate::dentry::{Dentry, SuperBlock};
use crate::inode::{Cred, Data, Inode};
use crate::mount_ns::{Mount, MountNs};
use crate::Errno;
use alloc::sync::Arc;
use uapi::errno::*;

/// A new tmpfs instance: its super block and root directory (`mode` incl. permission bits, owner root).
pub fn new_fs(perm: u32) -> (Arc<SuperBlock>, Arc<Dentry>) {
    new_fs_as("tmpfs", perm)
}

/// A tmpfs instance under another type name (FS-2: "devtmpfs" for `/dev`).
pub fn new_fs_as(fstype: &'static str, perm: u32) -> (Arc<SuperBlock>, Arc<Dentry>) {
    let sb = SuperBlock::new(fstype);
    sb.set_options(&options(perm, None));
    let root = Inode::new(sb.next_ino(), uapi::S_IFDIR | (perm & 0o7777), Cred::ROOT, Data::Dir);
    let d = Dentry::new_root(sb.clone(), root);
    (sb, d)
}

/// A mount of a new tmpfs (root 0755 like the initramfs root).
pub fn new_mount(parent: Option<(Arc<Mount>, Arc<Dentry>)>) -> Arc<Mount> {
    let (sb, root) = new_fs(0o755);
    Mount::new(sb, root, parent, false)
}

/// Mounts a new tmpfs on `path` of `ns` (made when missing) with root permissions `perm` and an optional own
/// size in pages. EBUSY when a tmpfs root is mounted right there already.
pub fn mount_new(ns: &MountNs, path: &str, perm: u32, size_pages: Option<u64>) -> Result<Arc<Mount>, Errno> {
    ns.mkdir_p(path)?;
    let at = ns.lookup_path(path.as_bytes(), true)?;
    if at.is_mount_root() && at.mnt.sb.fstype == "tmpfs" && !Arc::ptr_eq(&at.mnt, &ns.root_path().mnt) {
        return Err(EBUSY);
    }
    let (sb, root) = new_fs(perm);
    sb.pool.set_size(size_pages);
    sb.set_options(&options(perm, size_pages));
    ns.mount(&at, sb, root, false)
}

/// AB-7c: what Linux's `shmem_show_options` (linux-6.6/mm/shmem.c) prints for these mount options: the size
/// when one was given, the root mode unless 1777, and `inode64` (V2's inode numbers are 64-bit). `nr_inodes`
/// is always Linux's default (FS-15, statfs), which `shmem_show_options` leaves out; no uid/gid or huge pages.
pub fn options(perm: u32, size_pages: Option<u64>) -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    if let Some(p) = size_pages {
        let _ = write!(s, ",size={}k", p * 4);
    }
    if perm & 0o7777 != 0o1777 {
        let _ = write!(s, ",mode={:03o}", perm & 0o7777);
    }
    s.push_str(",inode64");
    s
}
