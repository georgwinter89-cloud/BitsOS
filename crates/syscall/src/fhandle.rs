//! L7-RESTE-AB 2: file handles of pidfds. name_to_handle_at (linux-7.3.1/fs/fhandle.c:134, `do_sys_name_to_handle`
//! :18) and open_by_handle_at (:461, `handle_to_path` :349, `get_path_anchor` :180, `may_decode_fh` :307) with
//! pidfs's export operations (fs/pidfs.c:881 `pidfs_encode_fh`, :917 `pidfs_fh_to_dentry`, :973
//! `pidfs_export_permission`): a pidfd's handle is its pid's pidfs inode number (type FILEID_KERNFS, 8 bytes),
//! and it opens a pidfd again while that pid has its task and the caller's pid namespace is the pid's or above.
//!
//! AT_HANDLE_FID of any other file encodes what its Linux file system would (`exportfs_encode_inode_fh`,
//! fs/exportfs/expfs.c:389): tmpfs's own handle (`shmem_encode_fh`, mm/shmem.c:4408), ext4's and vfat's
//! `generic_encode_ino32_fh` (fs/libfs.c:1446), and for file systems without export operations (procfs, sysfs,
//! devpts, pipes, sockets, anonymous inodes) the generic 64-bit one (`exportfs_encode_ino64_fid` :355). V2
//! keeps no inode generation, so it is 0. A handle that can be decoded exists only for pidfs: every other file
//! answers EOPNOTSUPP without AT_HANDLE_FID, and a handle decoded under any other anchor is ESTALE after Linux's
//! permission step (decoding for vfs's file systems and nsfs handles is not built).

use crate::SysCtx;
use crate::socket::ProcMem;
use alloc::vec;
use proc::cred::{self, Cap};
use proc::pidfd::Target;
use uapi::ab::fhandle::*;
use uapi::ab::pidfd::{FD_NSFS_ROOT, FD_PIDFS_ROOT};
use uapi::errno::*;
use vfs::Errno;
use vfs::abi::{AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_FOLLOW, O_ACCMODE, O_CLOEXEC, O_DIRECTORY, O_EXCL, O_LARGEFILE, O_NONBLOCK, PATH_MAX};
use vfs::sys::UserMem;

/// `struct file_handle`'s fixed part: `handle_bytes` and `handle_type`.
const HEAD: usize = 8;
/// A pidfs handle: the 64-bit inode number (`*max_len = 2` dwords).
const PIDFS_BYTES: u32 = 8;

/// What `filename_lookup` found for name_to_handle_at.
enum Found {
    /// A pidfd: its target and the mount id fdinfo shows.
    Pidfd(Target, u64),
    /// Any other object: its file system's AT_HANDLE_FID encoding (None: V2 builds none), inode number, mount id.
    Other(Option<Fid>, u64, u64),
}

/// The AT_HANDLE_FID encodings of the file systems V2 has.
#[derive(Clone, Copy)]
enum Fid {
    /// tmpfs, devtmpfs and memfds: generation, then the inode number's low and high half; type 1.
    Shmem,
    /// ext4, vfat: the inode number's low 32 bits and the generation; FILEID_INO32_GEN.
    Ino32,
    /// No export operations: the 64-bit inode number and the generation; FILEID_INO64_GEN.
    Ino64,
}

impl Fid {
    /// By the file system's name; None for an object without one (pipefs, sockfs, anon_inodefs).
    fn of(fstype: Option<&str>) -> Option<Fid> {
        match fstype {
            None | Some("proc" | "sysfs" | "devpts" | "mqueue" | "debugfs") => Some(Fid::Ino64),
            Some("tmpfs" | "devtmpfs") => Some(Fid::Shmem),
            Some("ext4" | "vfat") => Some(Fid::Ino32),
            // overlay, cgroup2: their own handles are not built.
            Some(_) => None,
        }
    }
    /// (dwords, type, the handle's bytes) with generation 0.
    fn encode(self, ino: u64) -> (u32, i32, vec::Vec<u8>) {
        let (lo, hi) = ((ino as u32).to_le_bytes(), ((ino >> 32) as u32).to_le_bytes());
        match self {
            Fid::Shmem => (3, FILEID_INO32_GEN, [[0u8; 4], lo, hi].concat()),
            Fid::Ino32 => (2, FILEID_INO32_GEN, [lo, [0u8; 4]].concat()),
            Fid::Ino64 => (3, FILEID_INO64_GEN, [lo, hi, [0u8; 4]].concat()),
        }
    }
}

/// EFAULT/ENAMETOOLONG for the name, ENOENT for an empty one without AT_EMPTY_PATH, then the lookup's errors.
fn lookup(c: &SysCtx<'_>, dfd: i32, name: u64, flags: u64) -> Result<Found, Errno> {
    let p = c.proc;
    let mem = ProcMem(p);
    let mut path = mem.read_cstr(name, PATH_MAX)?;
    let fd = |n: i32| (n >= 0).then(|| p.fds.get(n as usize).ok()).flatten().ok_or(EBADF);
    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return Err(ENOENT);
        }
        if dfd != AT_FDCWD {
            let f = fd(dfd)?;
            if let Some(t) = proc::pidfd::target_of(&f) {
                return Ok(Found::Pidfd(t, f.stat().mnt_id));
            }
            let a = f.stat();
            // A namespace file is nsfs's (its own handle, not built: open point 2 of the notes).
            let fid = if proc::ns::file::ns_of_file(&f).is_some() { None } else { Fid::of(f.path.as_ref().map(|x| x.dentry.sb.fstype)) };
            return Ok(Found::Other(fid, a.ino, a.mnt_id));
        }
        // AT_FDCWD with an empty name is the working directory.
        path = vec![b'.'];
    }
    let base = if path[0] == b'/' || dfd == AT_FDCWD { None } else { Some(fd(dfd)?.path.clone().filter(|x| x.dentry.is_dir()).ok_or(ENOTDIR)?) };
    let fs = c.thread.fs(p);
    let ns = p.mount_ns();
    let sys = vfs::sys::Sys { mem: &mem, fds: &p.fds, fs: &fs, ns: &ns, cred: p.fs_cred(), subject: p.subject() };
    let found = sys.ctx().lookup(base.as_ref(), &path, flags & AT_SYMLINK_FOLLOW != 0)?;
    // No name leads to a pidfs inode in V2 (Linux reaches one through /proc/<pid>/fd/<n> with AT_SYMLINK_FOLLOW).
    let a = found.attr();
    Ok(Found::Other(Fid::of(Some(found.dentry.sb.fstype)), a.ino, a.mnt_id))
}

fn sys_name_to_handle_at(c: &mut SysCtx<'_>) -> i64 {
    let (dfd, name, uh, umnt, flags) = (c.args[0] as i32, c.args[1], c.args[2], c.args[3], c.args[4] as u32 as u64);
    if flags & !(AT_SYMLINK_FOLLOW | AT_EMPTY_PATH | AT_HANDLE_FID | AT_HANDLE_MNT_ID_UNIQUE | AT_HANDLE_CONNECTABLE) != 0 {
        return -EINVAL;
    }
    // A connectable handle needs the parent, which neither a fid nor an fd without a name has.
    if flags & AT_HANDLE_CONNECTABLE != 0 && flags & (AT_HANDLE_FID | AT_EMPTY_PATH) != 0 {
        return -EINVAL;
    }
    // `exportfs_can_encode_fh`: pidfs encodes every handle; other files only a fid, where V2 builds one.
    let ((dwords, ty, bytes), mnt_id) = match lookup(c, dfd, name, flags) {
        Ok(Found::Pidfd(t, mnt)) => ((PIDFS_BYTES / 4, FILEID_KERNFS, t.ino().to_le_bytes().to_vec()), mnt),
        Ok(Found::Other(Some(fid), ino, mnt)) if flags & AT_HANDLE_FID != 0 => (fid.encode(ino), mnt),
        Ok(Found::Other(..)) => return -EOPNOTSUPP,
        Err(e) => return -e,
    };
    let mem = ProcMem(c.proc);
    let mut head = [0u8; HEAD];
    if !mem.read(uh, &mut head) {
        return -EFAULT;
    }
    let room = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    if room > MAX_HANDLE_SZ {
        return -EINVAL;
    }
    // The encoder's dwords or FILEID_INVALID with the size it needs; then only the fixed part is copied.
    let fits = room >> 2 >= dwords;
    let mut out = vec![0u8; HEAD];
    out[..4].copy_from_slice(&(dwords * 4).to_le_bytes());
    out[4..].copy_from_slice(&(if fits { ty } else { FILEID_INVALID }).to_le_bytes());
    if fits {
        out.extend_from_slice(&bytes);
    }
    // The mount id goes first; its fault wins and the handle is not copied.
    let wrote = if flags & AT_HANDLE_MNT_ID_UNIQUE != 0 { mem.write(umnt, &mnt_id.to_le_bytes()) } else { mem.write(umnt, &(mnt_id as u32).to_le_bytes()) };
    if !wrote || !mem.write(uh, &out) {
        return -EFAULT;
    }
    if fits { 0 } else { -EOVERFLOW }
}
crate::syscall_nr!(uapi::ab::nr_more::NAME_TO_HANDLE_AT, "name_to_handle_at", sys_name_to_handle_at);

/// What a handle is decoded under (`get_path_anchor`).
enum Anchor {
    /// pidfs: its root, or a pidfd (whose pid is its dentry).
    Pidfs(Option<Target>),
    /// nsfs: its root or a namespace file.
    Nsfs,
    /// Any other file or directory, the cwd for AT_FDCWD.
    Other,
}

fn anchor(c: &SysCtx<'_>, mfd: i32) -> Result<Anchor, Errno> {
    if mfd >= 0 {
        let f = c.proc.fds.get(mfd as usize).map_err(|_| EBADF)?;
        if let Some(t) = proc::pidfd::target_of(&f) {
            return Ok(Anchor::Pidfs(Some(t)));
        }
        return Ok(if proc::ns::file::ns_of_file(&f).is_some() { Anchor::Nsfs } else { Anchor::Other });
    }
    match mfd {
        AT_FDCWD => Ok(Anchor::Other),
        FD_PIDFS_ROOT => Ok(Anchor::Pidfs(None)),
        FD_NSFS_ROOT => Ok(Anchor::Nsfs),
        _ => Err(EBADF),
    }
}

/// `may_decode_fh` for an anchor without its own permission op: CAP_DAC_READ_SEARCH; else only O_DIRECTORY, by
/// one privileged over the file system or the anchor's mount namespace, with CAP_DAC_READ_SEARCH in its own user
/// namespace. V2's file systems belong to the initial user namespace and the caller's mount namespace stands for
/// the anchor's (locked mounts below the anchor are not modelled: decoding fails with ESTALE after this anyway).
fn may_decode(c: &SysCtx<'_>, oflags: u64) -> Result<(), Errno> {
    let cr = c.proc.cred();
    if cred::capable(&cr, Cap::DacReadSearch) {
        return Ok(());
    }
    if oflags & O_DIRECTORY == 0 {
        return Err(EPERM);
    }
    let owner = c.proc.mnt_ns().user_ns.clone();
    if !cred::capable(&cr, Cap::SysAdmin) && !cred::ns_capable(&cr, &owner, Cap::SysAdmin) {
        return Err(EPERM);
    }
    if !cred::own_capable(&cr, Cap::DacReadSearch) {
        return Err(EPERM);
    }
    Ok(())
}

/// `pidfs_fh_to_dentry` and `vfs_dentry_acceptable`: the task of the handle's inode number, seen from the caller's
/// pid namespace. A handle with FILEID_IS_DIR names no directory (ESTALE); one with FILEID_IS_CONNECTABLE is
/// accepted only under a pidfd of the same pid (its dentry is the anchor) and by a caller whose user namespace
/// maps pidfs's owner, root.
fn decode(c: &SysCtx<'_>, at: Option<Target>, bytes: u32, ty: i32, fh: &[u8]) -> Result<Target, Errno> {
    let user = ty as u32 & FILEID_USER_FLAGS_MASK;
    if bytes >> 2 < PIDFS_BYTES / 4 || (ty as u32 & !FILEID_USER_FLAGS_MASK) as i32 != FILEID_KERNFS {
        return Err(ESTALE);
    }
    let ino = u64::from_le_bytes([fh[0], fh[1], fh[2], fh[3], fh[4], fh[5], fh[6], fh[7]]);
    let t = proc::pidfd::by_ino(ino).filter(|t| crate::pidfd::visible(c, t)).ok_or(ESTALE)?;
    if user & FILEID_IS_DIR != 0 {
        return Err(ESTALE);
    }
    if user & FILEID_IS_CONNECTABLE != 0 {
        let ns = c.proc.cred().user_ns.clone();
        let maps_root = ns.from_kernel(proc::ns::user::MapKind::Uid, 0).is_some() && ns.from_kernel(proc::ns::user::MapKind::Gid, 0).is_some();
        if !maps_root || !at.is_some_and(|a| a.ino() == ino) {
            return Err(ESTALE);
        }
    }
    Ok(t)
}

fn sys_open_by_handle_at(c: &mut SysCtx<'_>) -> i64 {
    // `force_o_largefile` on x86_64.
    let (mfd, uh, oflags) = (c.args[0] as i32, c.args[1], c.args[2] as u32 as u64 | O_LARGEFILE);
    let mem = ProcMem(c.proc);
    let mut head = [0u8; HEAD];
    if !mem.read(uh, &mut head) {
        return -EFAULT;
    }
    let bytes = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    let ty = i32::from_le_bytes([head[4], head[5], head[6], head[7]]);
    if bytes > MAX_HANDLE_SZ || bytes == 0 || ty < 0 || ty as u32 & FILEID_USER_FLAGS_MASK & !(FILEID_IS_CONNECTABLE | FILEID_IS_DIR) != 0 {
        return -EINVAL;
    }
    let at = match anchor(c, mfd) {
        Ok(a) => a,
        Err(e) => return -e,
    };
    // The anchor's file system decides the permission (`eops->permission`, else `may_decode_fh`).
    let allowed = match &at {
        // `pidfs_export_permission`: open flags that make sense for a pidfd only.
        Anchor::Pidfs(_) if oflags & !(O_ACCMODE | O_NONBLOCK | O_CLOEXEC | O_EXCL | O_LARGEFILE) != 0 => Err(EINVAL),
        Anchor::Pidfs(_) | Anchor::Nsfs => Ok(()),
        Anchor::Other => may_decode(c, oflags),
    };
    if let Err(e) = allowed {
        return -e;
    }
    let mut fh = vec![0u8; bytes as usize];
    if !mem.read(uh + HEAD as u64, &mut fh) {
        return -EFAULT;
    }
    let Anchor::Pidfs(anchor_pid) = at else { return -ESTALE };
    let t = match decode(c, anchor_pid, bytes, ty, &fh) {
        Ok(t) => t,
        Err(e) => return -e,
    };
    // A leader's pid opened with PIDFD_THREAD names the leader thread, as pidfd_open(PIDFD_THREAD) does.
    let t = match t.thread {
        None if oflags & O_EXCL != 0 => Target { thread: Some((t.group.tgid, None)), group: t.group },
        _ => t,
    };
    let f = proc::pidfd::open_handle(t, oflags & !O_LARGEFILE);
    match c.proc.fds.install_flags(f, 0, oflags & O_CLOEXEC != 0) {
        Ok(fd) => fd as i64,
        Err(e) => -e,
    }
}
crate::syscall_nr!(uapi::ab::nr_more::OPEN_BY_HANDLE_AT, "open_by_handle_at", sys_open_by_handle_at);
