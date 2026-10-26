//! A disk file system's side of the name operations (FS-7 FAT; FS-8 ext4 takes the same shape).
//!
//! The VFS keeps the dentry tree as the cache of names (`dentry`). A super block with `FsOps`:
//! - fills a directory's children on first use (`load_dir`, through `Dentry::load`, once per directory);
//! - compares names by its own key (`fold`: FAT is case-insensitive and ignores trailing dots) and may know a
//!   second name of a child (`alias`: FAT's 8.3 alias), so lookups find what Linux's vfat finds;
//! - makes every change of names on its disk first, under the super block's names lock (`create` from
//!   `Dentry::create_child`, `remove` from unlink/rmdir, `rename`); the tree follows only when the disk
//!   change succeeded, and the file system's own rules (FAT: the boot stick's protected names) decide there,
//!   whoever calls (programs, MCP's file tools, the kernel's own `kapi` helpers);
//! - answers whether a node's content may change (`may_write`: open for writing, truncate, access W_OK).
//!
//! Hooks are called with `vfs.sb.ns` held (except `load_dir`, `alias`, `may_write`, `sync`) and must never
//! take it or call back into name operations; they may sleep on their disk.

use crate::dentry::Dentry;
use crate::inode::{Cred, Inode};
use crate::Errno;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;

pub trait FsOps: Send + Sync {
    /// The longest name component in bytes the VFS lets through to this file system (longer ones are
    /// ENAMETOOLONG before it is asked). A file system that counts in other units checks its own limit at
    /// `create`/`rename` (vfat: 255 UTF-16 units, so 255 umlauts are 510 bytes and valid).
    fn name_max(&self) -> usize {
        crate::abi::NAME_MAX
    }
    /// The key names are compared by in this file system's directories.
    fn fold(&self, name: &[u8]) -> Vec<u8> {
        name.to_vec()
    }
    /// Puts every child of `dir` into the tree (`Dentry::attach_loaded`). Called once per directory.
    fn load_dir(&self, dir: &Arc<Dentry>) -> Result<(), Errno>;
    /// The real name of the child of `dir` that `name` names in another way (FAT: its 8.3 alias), if any.
    fn alias(&self, _dir: &Dentry, _name: &[u8]) -> Option<Vec<u8>> {
        None
    }
    /// Whether the content of `d` may change (open for writing, O_TRUNC, truncate, access W_OK).
    fn may_write(&self, _d: &Dentry) -> Result<(), Errno> {
        Ok(())
    }
    /// Whether `victim` may be removed or renamed away (asked before the emptiness check, so a protected
    /// region answers EROFS first, as a read-only mount does).
    fn may_remove(&self, _victim: &Dentry) -> Result<(), Errno> {
        Ok(())
    }
    /// A new regular file or directory (`mode` carries the type) named `name` in `dir`, made on the disk.
    /// Other types are the file system's to refuse (FAT: EPERM, as vfat has no symlink/mknod).
    fn create(&self, dir: &Arc<Dentry>, name: &[u8], mode: u32, cred: Cred) -> Result<Arc<Inode>, Errno>;
    /// FS-8: a new node of any type with what tmpfs would have kept for it (`data`: a symlink's target, a
    /// device's or a FIFO's file object, which the returned inode should share). Default: `create`.
    fn create_node(&self, dir: &Arc<Dentry>, name: &[u8], mode: u32, cred: Cred, _data: &crate::inode::Data) -> Result<Arc<Inode>, Errno> {
        self.create(dir, name, mode, cred)
    }
    /// A hard link (vfat has none: EPERM).
    fn link(&self, _dir: &Arc<Dentry>, _name: &[u8], _inode: &Arc<Inode>) -> Result<(), Errno> {
        Err(EPERM)
    }
    /// unlink or rmdir of `victim`, a child of `dir` (the VFS checked type and emptiness).
    fn remove(&self, dir: &Arc<Dentry>, victim: &Arc<Dentry>) -> Result<(), Errno>;
    /// rename of `old` (in `odir`) to `nname` in `ndir`, replacing `target` when it exists.
    fn rename(&self, odir: &Arc<Dentry>, old: &Arc<Dentry>, ndir: &Arc<Dentry>, nname: &[u8], target: Option<&Arc<Dentry>>) -> Result<(), Errno>;
    /// RENAME_EXCHANGE (refused unless the file system can swap two entries).
    fn exchange(&self, _a: &Arc<Dentry>, _b: &Arc<Dentry>) -> Result<(), Errno> {
        Err(EINVAL)
    }
    /// fsync of a directory, syncfs: every change so far on stable storage.
    fn sync(&self) -> Result<(), Errno> {
        Ok(())
    }
    /// B1 (FS-8b): the file system's part of `FsFreeze::freeze` (Linux `freeze_fs`), called after the VFS
    /// drained the writers: write back its files' pages, commit, leave the disk consistent and hold its own
    /// background changes until `thaw_fs`. An error ends the freeze (the VFS thaws).
    fn freeze_fs(&self) -> Result<(), Errno> {
        Ok(())
    }
    /// The end of a freeze (Linux `unfreeze_fs`).
    fn thaw_fs(&self) {}
    /// Whether the file system has a `freeze_fs` of its own (Linux `s_op->freeze_fs != NULL`): FIFREEZE on
    /// one without answers EOPNOTSUPP (linux-6.6/fs/ioctl.c:395). The default is no.
    fn can_freeze(&self) -> bool {
        false
    }
    /// FS-1b: the file system's `setattr` after the VFS's generic checks (`setattr::notify_change`): it may
    /// refuse, adjust `a` (FAT drops a mode its attributes cannot hold and rounds times) and must make the
    /// change on its disk; the VFS then copies `a` into the inode. Called with `vfs.sb.ns` held.
    fn setattr(&self, _d: &Dentry, _a: &mut crate::setattr::SetAttr) -> Result<(), Errno> {
        Ok(())
    }
    /// FS-1b: statfs of this file system (`f_flags` is filled by the VFS from the mount).
    fn statfs(&self) -> Result<crate::statfs::StatFs, Errno>;
    /// stat of a directory of this file system: (st_size, st_blocks in 512-byte units) as its disk holds
    /// them (ext4: the directory's blocks); None keeps the VFS's own (tmpfs-style, 20 bytes a name).
    fn dir_attr(&self, _dir: &crate::inode::Inode) -> Option<(u64, u64)> {
        None
    }
}
