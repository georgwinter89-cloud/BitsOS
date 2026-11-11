//! GM-2a CR 5: the xattr family (188-199), readahead (187) and sync_file_range (277). No file system of V2 has extended
//! attributes, so every inode is Linux's inode without IOP_XATTR (linux-6.6/fs/xattr.c:62-65 `xattr_resolve_name`). The
//! order of the checks is CachyOS 7.3's, the ABI oracle's, which copies from the program before it looks at the path
//! (Linux 6.6 looked at the path first; seen in `cargo xtask abi-diff abi-gm2a`):
//! - set: flags EINVAL, the name (EFAULT, or ERANGE when empty or 256 bytes and more), a value above 64 KiB E2BIG, the value
//!   EFAULT; the path or EBADF; a read-only mount EROFS; `xattr_permission`; EOPNOTSUPP.
//! - get: the name; the path or EBADF; `xattr_permission`; EOPNOTSUPP. The size is not looked at.
//! - list: the path or EBADF; 0 (nothing to list; the host's LSM attributes have no counterpart here).
//! - remove: the name; the path or EBADF; EROFS; `xattr_permission`; EOPNOTSUPP.

use crate::abi::{AT_FDCWD, AT_SYMLINK_NOFOLLOW};
use crate::fd::OpenFile;
use crate::inode::{Cred, MAY_READ, MAY_WRITE};
use crate::mount_ns::Path;
use crate::sys::{at_lookup, ret, zeroed, Args, Sys};
use crate::Errno;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;

const XATTR_CREATE: u64 = 1;
const XATTR_REPLACE: u64 = 2;
/// `sizeof(struct xattr_name)`: XATTR_NAME_MAX + 1 (linux-6.6/include/uapi/linux/limits.h).
const NAME_BUF: usize = 256;
const XATTR_SIZE_MAX: u64 = 65536;
const SYNC_FILE_RANGE_WRITE: u64 = 2;
/// WAIT_BEFORE | WRITE | WAIT_AFTER (linux-6.6/fs/sync.c `VALID_FLAGS`).
const SYNC_VALID_FLAGS: u64 = 7;

/// A named node, or an open file.
enum Node {
    Path(Path),
    Open(Arc<OpenFile>),
}

impl Node {
    fn of_path(s: &Sys<'_>, path: u64, follow: bool) -> Result<Node, Errno> {
        at_lookup(s, AT_FDCWD as u64, path, if follow { 0 } else { AT_SYMLINK_NOFOLLOW }, true).map(Node::Path)
    }
    fn of_fd(s: &Sys<'_>, fd: u64) -> Result<Node, Errno> {
        s.file(fd).map(Node::Open)
    }
    /// `mnt_want_write`, `mnt_want_write_file`.
    fn want_write(&self) -> Result<(), Errno> {
        let p = match self {
            Node::Path(p) => Some(p),
            Node::Open(f) => f.path.as_ref(),
        };
        if p.is_some_and(|p| p.mnt.is_readonly()) { Err(EROFS) } else { Ok(()) }
    }
    fn mode_and_owner(&self) -> (u32, u32) {
        match self {
            Node::Path(p) => (p.dentry.inode.meta.mode(), p.dentry.inode.meta.uid()),
            Node::Open(f) => f.inode().map_or_else(
                || {
                    let st = f.stat();
                    (st.mode, st.uid)
                },
                |i| (i.meta.mode(), i.meta.uid()),
            ),
        }
    }
    /// `inode_permission`; an object without an inode of its own (a pipe's, a socket's) is its mode's.
    fn permission(&self, cred: Cred, want: u32) -> Result<(), Errno> {
        let i = match self {
            Node::Path(p) => Some(&p.dentry.inode),
            Node::Open(f) => f.inode(),
        };
        i.map_or(Ok(()), |i| i.permission(cred, want))
    }
}

/// `strncpy_from_user(kname, name, 256)`: empty or without a NUL in 256 bytes ERANGE, a fault EFAULT.
fn name_of(s: &Sys<'_>, ptr: u64) -> Result<Vec<u8>, Errno> {
    match s.mem.read_cstr(ptr, NAME_BUF) {
        Ok(n) if n.is_empty() => Err(ERANGE),
        Ok(n) => Ok(n),
        Err(ENAMETOOLONG) => Err(ERANGE),
        Err(e) => Err(e),
    }
}

/// `xattr_permission` (xattr.c:129) with V2's capability rule (uid 0, as in `setattr`).
fn may_access(n: &Node, name: &[u8], cred: Cred, write: bool) -> Result<(), Errno> {
    if name.starts_with(b"security.") || name.starts_with(b"system.") {
        return Ok(());
    }
    let refused = if write { EPERM } else { ENODATA };
    if name.starts_with(b"trusted.") {
        return if cred.uid == 0 { Ok(()) } else { Err(refused) };
    }
    let (mode, owner) = n.mode_and_owner();
    if name.starts_with(b"user.") {
        let ty = mode & uapi::S_IFMT;
        if ty != uapi::S_IFREG && ty != uapi::S_IFDIR {
            return Err(refused);
        }
        if ty == uapi::S_IFDIR && mode & crate::abi::S_ISVTX != 0 && write && owner != cred.uid && cred.uid != 0 {
            return Err(EPERM);
        }
    }
    n.permission(cred, if write { MAY_WRITE } else { MAY_READ })
}

/// How a call names its node: a path (following a last link or not), or an fd.
#[derive(Clone, Copy)]
enum By {
    Path(u64, bool),
    Fd(u64),
}

impl By {
    fn node(self, s: &Sys<'_>) -> Result<Node, Errno> {
        match self {
            By::Path(p, follow) => Node::of_path(s, p, follow),
            By::Fd(fd) => Node::of_fd(s, fd),
        }
    }
}

fn set(s: &Sys<'_>, by: By, name: u64, value: u64, size: u64, flags: u64) -> i64 {
    ret((|| {
        if flags as u32 as u64 & !(XATTR_CREATE | XATTR_REPLACE) != 0 {
            return Err(EINVAL);
        }
        let k = name_of(s, name)?;
        if size != 0 {
            if size > XATTR_SIZE_MAX {
                return Err(E2BIG);
            }
            let mut v = zeroed(size as usize)?;
            if !s.mem.read(value, &mut v) {
                return Err(EFAULT);
            }
        }
        let n = by.node(s)?;
        n.want_write()?;
        may_access(&n, &k, s.cred, true)?;
        Err(EOPNOTSUPP)
    })())
}

fn get(s: &Sys<'_>, by: By, name: u64) -> i64 {
    ret((|| {
        let k = name_of(s, name)?;
        let n = by.node(s)?;
        may_access(&n, &k, s.cred, false)?;
        Err(EOPNOTSUPP)
    })())
}

fn list(s: &Sys<'_>, by: By) -> i64 {
    ret(by.node(s).map(|_| 0))
}

fn remove(s: &Sys<'_>, by: By, name: u64) -> i64 {
    ret((|| {
        let k = name_of(s, name)?;
        let n = by.node(s)?;
        n.want_write()?;
        may_access(&n, &k, s.cred, true)?;
        Err(EOPNOTSUPP)
    })())
}

pub fn setxattr(s: &Sys<'_>, a: Args) -> i64 {
    set(s, By::Path(a[0], true), a[1], a[2], a[3], a[4])
}
pub fn lsetxattr(s: &Sys<'_>, a: Args) -> i64 {
    set(s, By::Path(a[0], false), a[1], a[2], a[3], a[4])
}
pub fn fsetxattr(s: &Sys<'_>, a: Args) -> i64 {
    set(s, By::Fd(a[0]), a[1], a[2], a[3], a[4])
}
pub fn getxattr(s: &Sys<'_>, a: Args) -> i64 {
    get(s, By::Path(a[0], true), a[1])
}
pub fn lgetxattr(s: &Sys<'_>, a: Args) -> i64 {
    get(s, By::Path(a[0], false), a[1])
}
pub fn fgetxattr(s: &Sys<'_>, a: Args) -> i64 {
    get(s, By::Fd(a[0]), a[1])
}
pub fn listxattr(s: &Sys<'_>, a: Args) -> i64 {
    list(s, By::Path(a[0], true))
}
pub fn llistxattr(s: &Sys<'_>, a: Args) -> i64 {
    list(s, By::Path(a[0], false))
}
pub fn flistxattr(s: &Sys<'_>, a: Args) -> i64 {
    list(s, By::Fd(a[0]))
}
pub fn removexattr(s: &Sys<'_>, a: Args) -> i64 {
    remove(s, By::Path(a[0], true), a[1])
}
pub fn lremovexattr(s: &Sys<'_>, a: Args) -> i64 {
    remove(s, By::Path(a[0], false), a[1])
}
pub fn fremovexattr(s: &Sys<'_>, a: Args) -> i64 {
    remove(s, By::Fd(a[0]), a[1])
}

fn file_mode(f: &OpenFile) -> u32 {
    f.inode().map_or_else(|| f.stat().mode, |i| i.meta.mode()) & uapi::S_IFMT
}

/// readahead(fd, offset, count) (linux-6.6/mm/readahead.c:721 and `generic_fadvise`): EBADF unless open for reading, EINVAL
/// unless a regular file or block device (CachyOS 7.3) or for an offset or count negative as `loff_t` (the offset: CachyOS
/// 7.3 on tmpfs); then what fadvise's WILLNEED does (FS-12).
pub fn readahead(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let f = s.file(a[0] as u32 as u64)?;
        if !f.can_read() {
            return Err(EBADF);
        }
        if !matches!(file_mode(&f), uapi::S_IFREG | crate::abi::S_IFBLK) || (a[1] as i64) < 0 || (a[2] as i64) < 0 {
            return Err(EINVAL);
        }
        if let Some(pc) = f.inode().and_then(|i| i.page_cache()) {
            pc.will_need(a[1], a[2]);
        }
        Ok(0)
    })())
}

/// sync_file_range(fd, offset, nbytes, flags) (linux-6.6/fs/sync.c:228, :355): EBADF; EINVAL for other flags, a negative
/// offset or end, or a wrap; ESPIPE unless a regular file, block device, directory or link. SYNC_FILE_RANGE_WRITE writes
/// the whole file back (fsync), V2 keeps no write-back per range; only an I/O error reaches the caller, as in Linux.
pub fn sync_file_range(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let f = s.file(a[0] as u32 as u64)?;
        let (off, n, flags) = (a[1] as i64, a[2] as i64, a[3] as u32 as u64);
        if flags & !SYNC_VALID_FLAGS != 0 || off < 0 {
            return Err(EINVAL);
        }
        let end = off.wrapping_add(n);
        if end < 0 || end < off {
            return Err(EINVAL);
        }
        if !matches!(file_mode(&f), uapi::S_IFREG | crate::abi::S_IFBLK | uapi::S_IFDIR | uapi::S_IFLNK) {
            return Err(ESPIPE);
        }
        if flags & SYNC_FILE_RANGE_WRITE != 0 && f.file.fsync() == Err(EIO) {
            return Err(EIO);
        }
        Ok(0)
    })())
}

#[cfg(test)]
#[path = "sys_xattr_tests.rs"]
mod tests;
