//! FS-1b syscalls that change or report a node's attributes: chmod/chown/utimes and their *at and f-forms
//! (linux-6.6/fs/open.c:629-868, fs/utimes.c), statfs/fstatfs (fs/statfs.c), sync/syncfs (fs/sync.c).
//! Argument checks and error order as Linux; the change itself is `setattr`.

use crate::abi::{AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW};
use crate::fd::OpenFile;
use crate::inode::Meta;
use crate::mount_ns::Path;
use crate::setattr::{self, SetAttr};
use crate::sys::{at_lookup, ret, Args, Sys, SysFn};
use crate::Errno;
use alloc::sync::Arc;
use uapi::errno::*;

/// What a setattr call acts on: a named node, or an open file (which may have no name: a pipe).
enum Node {
    Path(Path),
    Open(Arc<OpenFile>),
}

fn apply(s: &Sys<'_>, n: Node, build: impl Fn(&Meta) -> SetAttr) -> Result<i64, Errno> {
    let f = match n {
        Node::Path(p) => return s.ctx().setattr(&p, build).map(|_| 0),
        Node::Open(f) => f,
    };
    if let Some(p) = &f.path {
        return s.ctx().setattr(p, build).map(|_| 0);
    }
    // A pipe's own pipefs inode changes. Objects without metadata of their own are checked as their fstat
    // shows them: a socket (sockfs has a setattr) passes without keeping the change; an anonymous inode answers
    // EOPNOTSUPP after `may_setattr` (CachyOS's `anon_inode_setattr`; 6.6 changed the shared inode).
    if let Some(m) = f.file.meta() {
        return setattr::notify_change(m, false, s.cred, build(m), |_| Ok(())).map(|_| 0);
    }
    let st = f.stat();
    let m = Meta::new(st.ino, st.mode, crate::inode::Cred { uid: st.uid, gid: st.gid });
    let a = build(&m);
    if st.mode & uapi::S_IFMT == crate::abi::S_IFSOCK {
        return setattr::notify_change(&m, false, s.cred, a, |_| Ok(())).map(|_| 0);
    }
    setattr::may_setattr(&m, false, s.cred, &a)?;
    Err(EOPNOTSUPP)
}

/// The node of a path call with `flags` (only AT_SYMLINK_NOFOLLOW and AT_EMPTY_PATH allowed: EINVAL).
fn at_node(s: &Sys<'_>, dirfd: u64, path: u64, flags: u64) -> Result<Node, Errno> {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(EINVAL);
    }
    at_lookup(s, dirfd, path, flags, true).map(Node::Path)
}

fn mode_of(a: u64) -> u32 {
    a as u16 as u32
}

fn do_fchmodat(s: &Sys<'_>, dirfd: u64, path: u64, mode: u64, flags: u64) -> i64 {
    ret(at_node(s, dirfd, path, flags).and_then(|n| apply(s, n, |_| setattr::chmod(mode_of(mode)))))
}

pub fn chmod(s: &Sys<'_>, a: Args) -> i64 {
    do_fchmodat(s, AT_FDCWD as u64, a[0], a[1], 0)
}

pub fn fchmodat(s: &Sys<'_>, a: Args) -> i64 {
    do_fchmodat(s, a[0], a[1], a[2], 0)
}

pub fn fchmodat2(s: &Sys<'_>, a: Args) -> i64 {
    do_fchmodat(s, a[0], a[1], a[2], a[3] as u32 as u64)
}

pub fn fchmod(s: &Sys<'_>, a: Args) -> i64 {
    ret(s.file(a[0]).and_then(|f| apply(s, Node::Open(f), |_| setattr::chmod(mode_of(a[1])))))
}

fn chown_with(s: &Sys<'_>, n: Node, uid: u64, gid: u64) -> Result<i64, Errno> {
    let cred = s.cred;
    apply(s, n, |m| setattr::chown(m, cred, uid as u32, gid as u32))
}

fn do_fchownat(s: &Sys<'_>, dirfd: u64, path: u64, uid: u64, gid: u64, flags: u64) -> i64 {
    ret(at_node(s, dirfd, path, flags).and_then(|n| chown_with(s, n, uid, gid)))
}

pub fn chown(s: &Sys<'_>, a: Args) -> i64 {
    do_fchownat(s, AT_FDCWD as u64, a[0], a[1], a[2], 0)
}

pub fn lchown(s: &Sys<'_>, a: Args) -> i64 {
    do_fchownat(s, AT_FDCWD as u64, a[0], a[1], a[2], AT_SYMLINK_NOFOLLOW)
}

pub fn fchownat(s: &Sys<'_>, a: Args) -> i64 {
    do_fchownat(s, a[0], a[1], a[2], a[3], a[4] as u32 as u64)
}

pub fn fchown(s: &Sys<'_>, a: Args) -> i64 {
    ret(s.file(a[0]).and_then(|f| chown_with(s, Node::Open(f), a[1], a[2])))
}

fn read_i64s<const N: usize>(s: &Sys<'_>, addr: u64) -> Result<[i64; N], Errno> {
    let mut raw = [0u8; 64];
    if !s.mem.read(addr, &mut raw[..N * 8]) {
        return Err(EFAULT);
    }
    let mut out = [0i64; N];
    for (i, v) in out.iter_mut().enumerate() {
        *v = i64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]));
    }
    Ok(out)
}

/// `do_utimes`: a NULL path with a real dirfd is the fd itself (flags must be 0); otherwise a path call.
/// The times are checked after the lookup (`vfs_utimes`).
fn do_utimes(s: &Sys<'_>, dirfd: u64, path: u64, times: Option<[(i64, i64); 2]>, flags: u64) -> Result<i64, Errno> {
    let node = if path == 0 && dirfd as u32 as i32 != AT_FDCWD {
        if flags != 0 {
            return Err(EINVAL);
        }
        Node::Open(s.file(dirfd)?)
    } else {
        at_node(s, dirfd, path, flags)?
    };
    let a = setattr::utimes(times)?;
    apply(s, node, |_| a)
}

pub fn utimensat(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let times = match a[2] {
            0 => None,
            p => {
                let t = read_i64s::<4>(s, p)?;
                if t[1] == setattr::UTIME_OMIT && t[3] == setattr::UTIME_OMIT {
                    return Ok(0);
                }
                Some([(t[0], t[1]), (t[2], t[3])])
            }
        };
        do_utimes(s, a[0], a[1], times, a[3] as u32 as u64)
    })())
}

/// futimesat/utimes: two `timeval`s; a microsecond value out of range is EINVAL (so UTIME_* cannot pass).
fn do_futimesat(s: &Sys<'_>, dirfd: u64, path: u64, tv: u64) -> i64 {
    ret((|| {
        let times = match tv {
            0 => None,
            p => {
                let t = read_i64s::<4>(s, p)?;
                if !(0..1_000_000).contains(&t[1]) || !(0..1_000_000).contains(&t[3]) {
                    return Err(EINVAL);
                }
                Some([(t[0], t[1] * 1000), (t[2], t[3] * 1000)])
            }
        };
        do_utimes(s, dirfd, path, times, 0)
    })())
}

pub fn futimesat(s: &Sys<'_>, a: Args) -> i64 {
    do_futimesat(s, a[0], a[1], a[2])
}

pub fn utimes(s: &Sys<'_>, a: Args) -> i64 {
    do_futimesat(s, AT_FDCWD as u64, a[0], a[1])
}

/// utime: a `utimbuf` (actime, modtime) in whole seconds.
pub fn utime(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let times = match a[1] {
            0 => None,
            p => {
                let t = read_i64s::<2>(s, p)?;
                Some([(t[0], 0), (t[1], 0)])
            }
        };
        do_utimes(s, AT_FDCWD as u64, a[0], times, 0)
    })())
}

fn put_statfs(s: &Sys<'_>, buf: u64, st: &crate::statfs::StatFs) -> Result<i64, Errno> {
    s.copy_out(buf, &st.bytes()).map(|_| 0)
}

pub fn statfs(s: &Sys<'_>, a: Args) -> i64 {
    ret(at_lookup(s, AT_FDCWD as u64, a[0], 0, true).and_then(|p| put_statfs(s, a[1], &crate::statfs::of(&p.dentry.sb, p.mnt.readonly)?)))
}

pub fn fstatfs(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let f = s.raw(a[0])?;
        let st = match &f.path {
            Some(p) => crate::statfs::of(&p.dentry.sb, p.mnt.readonly)?,
            None => crate::statfs::of_object(f.file.mode()),
        };
        put_statfs(s, a[1], &st)
    })())
}

/// sync: everything durable (`ksys_sync`); always 0.
pub fn sync(_s: &Sys<'_>, _a: Args) -> i64 {
    let p = crate::writeback::sync_durable();
    if p.failed > 0 {
        klog::event!(Warn, "vfs", "sync.failed", flushed = p.flushed, failed = p.failed);
    }
    0
}

/// syncfs: the files' dirty pages (all of them: V2 keeps one write-back list), then the fd's file system's
/// metadata and device cache; its error is the answer (`sync_filesystem`).
pub fn syncfs(s: &Sys<'_>, a: Args) -> i64 {
    ret((|| {
        let f = s.file(a[0])?;
        crate::writeback::sync_durable();
        match f.path.as_ref().and_then(|p| p.dentry.sb.ops.clone()) {
            Some(o) => o.sync().map(|_| 0),
            None => Ok(0),
        }
    })())
}

pub const CALLS: &[(usize, &str, SysFn)] = &[
    (90, "chmod", chmod),
    (91, "fchmod", fchmod),
    (92, "chown", chown),
    (93, "fchown", fchown),
    (94, "lchown", lchown),
    (132, "utime", utime),
    (137, "statfs", statfs),
    (138, "fstatfs", fstatfs),
    (162, "sync", sync),
    (235, "utimes", utimes),
    (260, "fchownat", fchownat),
    (261, "futimesat", futimesat),
    (268, "fchmodat", fchmodat),
    (280, "utimensat", utimensat),
    (306, "syncfs", syncfs),
    (452, "fchmodat2", fchmodat2),
];
