//! Linux x86-64 file ABI: flag values, limits and the byte layouts of `struct stat`, `struct statx` and
//! `struct linux_dirent64` (include/uapi/asm-generic/fcntl.h, include/uapi/linux/{fcntl,stat}.h,
//! arch/x86/include/uapi/asm/stat.h). Kept in the FS crate; the desk may move them to `uapi::fs` later.

pub const O_ACCMODE: u64 = 0o3;
pub const O_RDONLY: u64 = 0;
pub const O_WRONLY: u64 = 1;
pub const O_RDWR: u64 = 2;
pub const O_CREAT: u64 = 0o100;
pub const O_EXCL: u64 = 0o200;
pub const O_NOCTTY: u64 = 0o400;
pub const O_TRUNC: u64 = 0o1000;
pub const O_APPEND: u64 = 0o2000;
pub const O_NONBLOCK: u64 = 0o4000;
pub const O_DSYNC: u64 = 0o10000;
pub const FASYNC: u64 = 0o20000;
pub const O_DIRECT: u64 = 0o40000;
pub const O_LARGEFILE: u64 = 0o100000;
pub const O_DIRECTORY: u64 = 0o200000;
pub const O_NOFOLLOW: u64 = 0o400000;
pub const O_NOATIME: u64 = 0o1000000;
pub const O_CLOEXEC: u64 = 0o2000000;
pub const O_SYNC_BIT: u64 = 0o4000000;
pub const O_PATH: u64 = 0o10000000;
pub const O_TMPFILE_BIT: u64 = 0o20000000;
/// `VALID_OPEN_FLAGS` (fs/fcntl.c): bits outside are ignored by open/openat (openat2 would refuse them).
pub const VALID_OPEN_FLAGS: u64 = O_ACCMODE
    | O_CREAT
    | O_EXCL
    | O_NOCTTY
    | O_TRUNC
    | O_APPEND
    | O_NONBLOCK
    | O_DSYNC
    | FASYNC
    | O_DIRECT
    | O_LARGEFILE
    | O_DIRECTORY
    | O_NOFOLLOW
    | O_NOATIME
    | O_CLOEXEC
    | O_SYNC_BIT
    | O_PATH
    | O_TMPFILE_BIT;
/// What `fcntl(F_SETFL)` may change (`SETFL_MASK`).
pub const SETFL_MASK: u64 = O_APPEND | O_NONBLOCK | FASYNC | O_DIRECT | O_NOATIME;
/// Flags that only matter at open time and are not kept in the file (`do_dentry_open`).
pub const OPEN_ONLY: u64 = O_CREAT | O_EXCL | O_NOCTTY | O_TRUNC | O_CLOEXEC;

pub const AT_FDCWD: i32 = -100;
pub const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
pub const AT_REMOVEDIR: u64 = 0x200;
pub const AT_EACCESS: u64 = 0x200;
pub const AT_SYMLINK_FOLLOW: u64 = 0x400;
pub const AT_NO_AUTOMOUNT: u64 = 0x800;
pub const AT_EMPTY_PATH: u64 = 0x1000;
pub const AT_STATX_SYNC_TYPE: u64 = 0x6000;

pub const F_DUPFD: u64 = 0;
pub const F_GETFD: u64 = 1;
pub const F_SETFD: u64 = 2;
pub const F_GETFL: u64 = 3;
pub const F_SETFL: u64 = 4;
pub const F_DUPFD_CLOEXEC: u64 = 1030;
pub const FD_CLOEXEC: u64 = 1;

pub const SEEK_SET: u32 = 0;
pub const SEEK_CUR: u32 = 1;
pub const SEEK_END: u32 = 2;
pub const SEEK_DATA: u32 = 3;
pub const SEEK_HOLE: u32 = 4;

pub const RENAME_NOREPLACE: u64 = 1;
pub const RENAME_EXCHANGE: u64 = 2;
pub const RENAME_WHITEOUT: u64 = 4;

pub const R_OK: u32 = 4;
pub const W_OK: u32 = 2;
pub const X_OK: u32 = 1;

pub const S_ISUID: u32 = 0o4000;
pub const S_ISGID: u32 = 0o2000;
pub const S_ISVTX: u32 = 0o1000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFSOCK: u32 = 0o140000;

pub const DT_UNKNOWN: u8 = 0;
pub const DT_FIFO: u8 = 1;
pub const DT_CHR: u8 = 2;
pub const DT_DIR: u8 = 4;
pub const DT_BLK: u8 = 6;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;
pub const DT_SOCK: u8 = 12;

/// Longest read/write one call does (`MAX_RW_COUNT` = INT_MAX rounded down to a page).
pub const MAX_RW_COUNT: u64 = 0x7fff_f000;
/// A path argument including its NUL (`PATH_MAX`).
pub const PATH_MAX: usize = 4096;
/// One name component (`NAME_MAX`).
pub const NAME_MAX: usize = 255;
/// Symlinks followed in one path walk (`MAXSYMLINKS`): a bound against hostile loops, as Linux.
pub const MAXSYMLINKS: u32 = 40;
/// Entries of one readv/writev (`UIO_MAXIOV`).
pub const UIO_MAXIOV: u64 = 1024;
/// Largest file offset (`MAX_LFS_FILESIZE` on 64-bit).
pub const MAX_FILE_SIZE: u64 = i64::MAX as u64;
/// fallocate modes (linux-6.6/include/uapi/linux/falloc.h; FALLOC_FL_WRITE_ZEROES 0x80 is newer than 6.6 and
/// known to CachyOS's kernel, measured).
pub const FALLOC_FL_KEEP_SIZE: u32 = 0x01;
pub const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;
pub const FALLOC_FL_COLLAPSE_RANGE: u32 = 0x08;
pub const FALLOC_FL_ZERO_RANGE: u32 = 0x10;
pub const FALLOC_FL_INSERT_RANGE: u32 = 0x20;
pub const FALLOC_FL_UNSHARE_RANGE: u32 = 0x40;
pub const FALLOC_FL_WRITE_ZEROES: u32 = 0x80;
/// Default `RLIMIT_NOFILE` soft limit and the hard ceiling (`sysctl fs.nr_open`), as Linux.
pub const NOFILE_DEFAULT: usize = 1024;
pub const NR_OPEN: usize = 1 << 20;

pub const STATX_BASIC_STATS: u32 = 0x7ff;
pub const STATX_BTIME: u32 = 0x800;
pub const STATX_MNT_ID: u32 = 0x1000;
pub const STATX_RESERVED: u32 = 0x8000_0000;
pub const STATX_ATTR_IMMUTABLE: u64 = 0x10;
pub const STATX_ATTR_APPEND: u64 = 0x20;
pub const STATX_ATTR_NODUMP: u64 = 0x40;
pub const STATX_ATTR_MOUNT_ROOT: u64 = 0x2000;

/// `d_type` of a mode.
pub fn dtype(mode: u32) -> u8 {
    match mode & uapi::S_IFMT {
        uapi::S_IFREG => DT_REG,
        uapi::S_IFDIR => DT_DIR,
        uapi::S_IFLNK => DT_LNK,
        uapi::S_IFCHR => DT_CHR,
        uapi::S_IFIFO => DT_FIFO,
        S_IFBLK => DT_BLK,
        S_IFSOCK => DT_SOCK,
        _ => DT_UNKNOWN,
    }
}

/// A point in time (seconds, nanoseconds).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Time {
    pub sec: i64,
    pub nsec: u32,
}

impl Time {
    pub fn from_ns(ns: u64) -> Time {
        Time { sec: (ns / 1_000_000_000) as i64, nsec: (ns % 1_000_000_000) as u32 }
    }
}

/// What `stat`/`statx` report (`struct kstat`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attr {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: u64,
    pub blksize: u32,
    /// 512-byte blocks allocated.
    pub blocks: u64,
    pub atime: Time,
    pub mtime: Time,
    pub ctime: Time,
    pub btime: Time,
    pub mnt_id: u64,
    pub mount_root: bool,
}

/// `new_encode_dev` (major 12 bits, minor 20 bits split around it).
pub fn encode_dev(major: u32, minor: u32) -> u64 {
    ((minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)) as u64
}

pub fn dev_major(dev: u64) -> u32 {
    ((dev >> 8) & 0xfff) as u32
}

pub fn dev_minor(dev: u64) -> u32 {
    ((dev & 0xff) | ((dev >> 12) & 0xfff00)) as u32
}

/// `struct stat` (144 bytes, x86-64).
pub fn stat_bytes(a: &Attr) -> [u8; 144] {
    uapi::Stat {
        dev: a.dev,
        ino: a.ino,
        nlink: a.nlink as u64,
        mode: a.mode,
        uid: a.uid,
        gid: a.gid,
        rdev: a.rdev,
        size: a.size as i64,
        blksize: a.blksize as i64,
        blocks: a.blocks as i64,
        atime: uapi::Timespec { sec: a.atime.sec, nsec: a.atime.nsec as i64 },
        mtime: uapi::Timespec { sec: a.mtime.sec, nsec: a.mtime.nsec as i64 },
        ctime: uapi::Timespec { sec: a.ctime.sec, nsec: a.ctime.nsec as i64 },
    }
    .to_bytes()
}

/// `struct statx` (256 bytes). `want` is the caller's mask: btime is reported only when asked, as tmpfs does.
pub fn statx_bytes(a: &Attr, want: u32) -> [u8; 256] {
    let mut b = [0u8; 256];
    let mut mask = STATX_BASIC_STATS | STATX_MNT_ID;
    if want & STATX_BTIME != 0 {
        mask |= STATX_BTIME;
    }
    let w32 = |b: &mut [u8; 256], o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    let w64 = |b: &mut [u8; 256], o: usize, v: u64| b[o..o + 8].copy_from_slice(&v.to_le_bytes());
    let ts = |b: &mut [u8; 256], o: usize, t: Time| {
        b[o..o + 8].copy_from_slice(&t.sec.to_le_bytes());
        b[o + 8..o + 12].copy_from_slice(&t.nsec.to_le_bytes());
    };
    w32(&mut b, 0, mask);
    w32(&mut b, 4, a.blksize);
    w64(&mut b, 8, if a.mount_root { STATX_ATTR_MOUNT_ROOT } else { 0 });
    w32(&mut b, 16, a.nlink);
    w32(&mut b, 20, a.uid);
    w32(&mut b, 24, a.gid);
    b[28..30].copy_from_slice(&(a.mode as u16).to_le_bytes());
    w64(&mut b, 32, a.ino);
    w64(&mut b, 40, a.size);
    w64(&mut b, 48, a.blocks);
    w64(&mut b, 56, STATX_ATTR_APPEND | STATX_ATTR_IMMUTABLE | STATX_ATTR_NODUMP | STATX_ATTR_MOUNT_ROOT);
    ts(&mut b, 64, a.atime);
    if mask & STATX_BTIME != 0 {
        ts(&mut b, 80, a.btime);
    }
    ts(&mut b, 96, a.ctime);
    ts(&mut b, 112, a.mtime);
    w32(&mut b, 128, dev_major(a.rdev));
    w32(&mut b, 132, dev_minor(a.rdev));
    w32(&mut b, 136, dev_major(a.dev));
    w32(&mut b, 140, dev_minor(a.dev));
    w64(&mut b, 144, a.mnt_id);
    b
}

/// One `struct linux_dirent64` record (8-byte aligned).
pub fn dirent64(out: &mut alloc::vec::Vec<u8>, ino: u64, off: i64, dtype: u8, name: &[u8]) {
    let reclen = (19 + name.len() + 1).next_multiple_of(8);
    let start = out.len();
    out.extend_from_slice(&ino.to_le_bytes());
    out.extend_from_slice(&off.to_le_bytes());
    out.extend_from_slice(&(reclen as u16).to_le_bytes());
    out.push(dtype);
    out.extend_from_slice(name);
    out.resize(start + reclen, 0);
}

/// Size `dirent64` would take for `name`.
pub fn dirent64_len(name: &[u8]) -> usize {
    (19 + name.len() + 1).next_multiple_of(8)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layouts() {
        let a = Attr { dev: encode_dev(0, 0x123), ino: 7, mode: 0o100644, nlink: 1, size: 9, blksize: 4096, ..Default::default() };
        let s = stat_bytes(&a);
        assert_eq!(u64::from_le_bytes(s[8..16].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(s[24..28].try_into().unwrap()), 0o100644);
        let x = statx_bytes(&a, STATX_BASIC_STATS);
        assert_eq!(u32::from_le_bytes(x[0..4].try_into().unwrap()), 0x17ff);
        assert_eq!(u32::from_le_bytes(x[140..144].try_into().unwrap()), 0x123);
        assert_eq!(dev_minor(encode_dev(3, 0x12345)), 0x12345);
        assert_eq!(dev_major(encode_dev(3, 0x12345)), 3);
        let mut d = alloc::vec::Vec::new();
        dirent64(&mut d, 1, 2, DT_DIR, b".");
        assert_eq!(d.len(), 24);
        assert_eq!(d[16..18], [24, 0]);
        assert_eq!(d[19], b'.');
    }
}
