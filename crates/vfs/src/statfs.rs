//! FS-1b: statfs/fstatfs answers per file system (linux-6.6/fs/statfs.c; tmpfs `shmem_statfs`, proc/sysfs/
//! devpts `simple_statfs`, vfat `fat_statfs` through `FsOps::statfs`, pipefs/sockfs/anon_inodefs for objects
//! without a name).

use crate::dentry::SuperBlock;
use crate::Errno;

pub const TMPFS_MAGIC: i64 = 0x0102_1994;
pub const PROC_SUPER_MAGIC: i64 = 0x9fa0;
pub const SYSFS_MAGIC: i64 = 0x6265_6572;
pub const DEVPTS_SUPER_MAGIC: i64 = 0x1cd1;
pub const MSDOS_SUPER_MAGIC: i64 = 0x4d44;
pub const PIPEFS_MAGIC: i64 = 0x5049_5045;
pub const SOCKFS_MAGIC: i64 = 0x534F_434B;
pub const ANON_INODE_FS_MAGIC: i64 = 0x0904_1934;

pub const ST_RDONLY: i64 = 0x0001;
pub const ST_VALID: i64 = 0x0020;
pub const ST_RELATIME: i64 = 0x1000;

const PAGE: i64 = 4096;

/// `struct kstatfs`; `bytes` gives x86-64's `struct statfs` (120 bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatFs {
    pub ftype: i64,
    pub bsize: i64,
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub fsid: u64,
    pub namelen: i64,
    pub frsize: i64,
    pub flags: i64,
}

impl StatFs {
    /// `simple_statfs`: type, page-sized blocks, NAME_MAX; nothing counted.
    pub fn simple(ftype: i64) -> StatFs {
        StatFs { ftype, bsize: PAGE, namelen: crate::abi::NAME_MAX as i64, ..Default::default() }
    }
    pub fn bytes(&self) -> [u8; 120] {
        let fsid = [(self.fsid as u32).to_le_bytes(), ((self.fsid >> 32) as u32).to_le_bytes()].concat();
        let words: [u64; 7] = [self.ftype as u64, self.bsize as u64, self.blocks, self.bfree, self.bavail, self.files, self.ffree];
        let tail: [u64; 3] = [self.namelen as u64, self.frsize as u64, self.flags as u64];
        let mut o = [0u8; 120];
        for (i, w) in words.iter().enumerate() {
            o[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        o[56..64].copy_from_slice(&fsid);
        for (i, w) in tail.iter().enumerate() {
            o[64 + i * 8..72 + i * 8].copy_from_slice(&w.to_le_bytes());
        }
        o
    }
}

/// tmpfs (`shmem_statfs`): pages; a mount with its own size reports it, else the system pool (the tmpfs
/// budget all instances share, V2-PLAN R2) as the size. Inodes (FS-15): `f_files` is Linux's default
/// `nr_inodes` (`shmem_default_max_inodes`, linux-6.6/mm/shmem.c:139-145: RAM pages / 2 on x86-64, no highmem),
/// `f_ffree` that minus the slots in use. The number is advisory (R1): V2 refuses inodes only when the pool
/// refuses their metadata (ENOSPC), never at a count.
fn tmpfs(sb: &SuperBlock) -> StatFs {
    tmpfs_at(sb, crate::pool::ram_pages())
}

/// `tmpfs` for `ram_pages` of RAM (None: no frame allocator, host tests; inodes then stay 0 as `nr_inodes=0`).
pub(crate) fn tmpfs_at(sb: &SuperBlock, ram_pages: Option<u64>) -> StatFs {
    let mut s = StatFs::simple(TMPFS_MAGIC);
    let sys_limit = crate::pool::system_limit();
    let sys_free = sys_limit.saturating_sub(crate::pool::system_used());
    let (blocks, free) = match sb.pool.size() {
        Some(size) => (size, size.saturating_sub(sb.pool.used()).min(sys_free)),
        None if sys_limit != u64::MAX => (sys_limit, sys_free),
        None => (0, 0),
    };
    s.blocks = blocks;
    s.bfree = free;
    s.bavail = free;
    // max_inodes 0 means no limit in Linux: the fields stay 0 like simple_statfs.
    if let Some(max) = ram_pages.map(|p| p / 2).filter(|&m| m > 0) {
        // The root inode takes its slot at mount (`shmem_fill_super` -> `shmem_get_inode`); V2 does not charge it.
        let used = sb.pool.inodes().saturating_add(1);
        s.files = max;
        s.ffree = max.saturating_sub(used);
    }
    s
}

/// statfs of a mounted file system (`vfs_statfs`): its own answer, `f_frsize` defaulting to `f_bsize`, the
/// fsid from the instance's device number when the file system gives none, and `f_flags` from the mount
/// (`calculate_f_flags`; V2 mounts are relatime, as /proc/mounts says).
pub fn of(sb: &SuperBlock, readonly: bool) -> Result<StatFs, Errno> {
    let mut s = match &sb.ops {
        Some(o) => o.statfs()?,
        None => match sb.fstype {
            "proc" => StatFs::simple(PROC_SUPER_MAGIC),
            "sysfs" => StatFs::simple(SYSFS_MAGIC),
            "devpts" => StatFs::simple(DEVPTS_SUPER_MAGIC),
            "tmpfs" | "devtmpfs" => tmpfs(sb),
            other => {
                // A pseudo file system without its magic here: simple_statfs with type 0, never tmpfs's.
                klog::event!(Debug, "vfs", "statfs.unknown_type", fs = other, dev = sb.dev);
                StatFs::simple(0)
            }
        },
    };
    if s.frsize == 0 {
        s.frsize = s.bsize;
    }
    if s.fsid == 0 && s.ftype != PROC_SUPER_MAGIC && s.ftype != SYSFS_MAGIC && s.ftype != DEVPTS_SUPER_MAGIC {
        s.fsid = sb.dev;
    }
    s.flags = ST_VALID | ST_RELATIME | if readonly { ST_RDONLY } else { 0 };
    Ok(s)
}

/// fstatfs of an object without a name: its pseudo file system by type (internal mounts: only ST_VALID).
pub fn of_object(mode: u32) -> StatFs {
    let ftype = match mode & uapi::S_IFMT {
        uapi::S_IFIFO => PIPEFS_MAGIC,
        crate::abi::S_IFSOCK => SOCKFS_MAGIC,
        _ => ANON_INODE_FS_MAGIC,
    };
    StatFs { frsize: PAGE, flags: ST_VALID, ..StatFs::simple(ftype) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let s = StatFs { ftype: 1, bsize: 2, blocks: 3, bfree: 4, bavail: 5, files: 6, ffree: 7, fsid: 0x0000_0009_0000_0008, namelen: 10, frsize: 11, flags: 12 };
        let b = s.bytes();
        let w = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        assert_eq!((w(0), w(6), w(8), w(9), w(10), w(11)), (1, 7, 10, 11, 12, 0));
        assert_eq!((u32::from_le_bytes(b[56..60].try_into().unwrap()), u32::from_le_bytes(b[60..64].try_into().unwrap())), (8, 9));
    }

    #[test]
    fn tmpfs_with_size() {
        let (sb, _) = crate::tmpfs::new_fs(0o755);
        sb.pool.set_size(Some(100));
        let s = of(&sb, true).unwrap();
        assert_eq!((s.ftype, s.bsize, s.frsize, s.blocks, s.namelen), (TMPFS_MAGIC, 4096, 4096, 100, 255));
        assert_eq!(s.flags, ST_VALID | ST_RELATIME | ST_RDONLY);
        assert!(s.bfree <= 100 && s.fsid == sb.dev);
        let (dsb, _) = crate::tmpfs::new_fs_as("devtmpfs", 0o755);
        assert_eq!(of(&dsb, false).unwrap().ftype, TMPFS_MAGIC);
        assert_eq!((of(&sb, false).unwrap().files, of(&sb, false).unwrap().ffree), (0, 0), "no RAM known: nr_inodes=0");
        let (xsb, _) = crate::tmpfs::new_fs_as("cgroup2", 0o755);
        assert_eq!(of(&xsb, false).unwrap().ftype, 0, "an unknown type is not reported as tmpfs");
    }

    /// FS-15: f_files = RAM pages / 2, f_ffree counts live inodes (root included) and extra hard links of
    /// non-directories, as `shmem_reserve_inode`/`shmem_free_inode` do.
    #[test]
    fn tmpfs_inodes() {
        use crate::abi::{O_CREAT, O_RDWR};
        let ns = crate::mount_ns::MountNs::new_empty();
        let c = crate::ops::Ctx::kernel(&ns);
        let m = crate::tmpfs::mount_new(&ns, "/t", 0o1777, None).unwrap();
        let st = || tmpfs_at(&m.sb, Some(1000));
        assert_eq!((st().files, st().ffree), (500, 499), "a fresh mount: the root's slot");
        let f = c.open(None, b"/t/a", O_CREAT | O_RDWR, 0o600).unwrap();
        c.mkdir(None, b"/t/d", 0o755).unwrap();
        c.symlink(b"a", None, b"/t/s").unwrap();
        assert_eq!(st().ffree, 496, "file, directory, symlink");
        c.link(None, b"/t/a", None, b"/t/d/b", false).unwrap();
        assert_eq!(st().ffree, 495, "a hard link takes a slot");
        c.unlink(None, b"/t/a").unwrap();
        assert_eq!(st().ffree, 496, "removing a name of a file linked twice gives it back");
        c.unlink(None, b"/t/d/b").unwrap();
        assert_eq!(st().ffree, 496, "the open file keeps its inode");
        drop(f);
        assert_eq!(st().ffree, 497, "its last reference gives the inode's slot back");
        assert_eq!((tmpfs_at(&m.sb, Some(1)).files, tmpfs_at(&m.sb, Some(1)).ffree), (0, 0), "max_inodes 0: none");
        assert_eq!((tmpfs_at(&m.sb, Some(4)).files, tmpfs_at(&m.sb, Some(4)).ffree), (2, 0), "saturates at 0");
        // Fixer FS-15: the paths where f_ffree could drift.
        let g = c.open(None, b"/t/g", O_CREAT | O_RDWR, 0o600).unwrap();
        drop(g);
        c.link(None, b"/t/g", None, b"/t/d/h", false).unwrap();
        assert_eq!(st().ffree, 495, "a file linked twice: two slots");
        assert_eq!(c.link(None, b"/t/g", None, b"/t/s", false), Err(uapi::errno::EEXIST));
        assert_eq!(c.link(None, b"/t/d", None, b"/t/d2", false), Err(uapi::errno::EPERM));
        assert_eq!(st().ffree, 495, "a failed link takes nothing");
        c.rename(None, b"/t/s", None, b"/t/d/h", 0).unwrap();
        assert_eq!(st().ffree, 496, "rename over a name of a file linked twice gives its link slot back");
        c.rename(None, b"/t/d/h", None, b"/t/g", 0).unwrap();
        assert_eq!(st().ffree, 497, "rename over the last name of a file frees the file");
        c.unlink(None, b"/t/g").unwrap();
        assert_eq!(st().ffree, 498, "the symlink is gone");
        c.rmdir(None, b"/t/d").unwrap();
        assert_eq!(st().ffree, 499, "rmdir gives the directory's slot back: only the root is left");
    }
}
