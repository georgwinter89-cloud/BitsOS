//! Linux x86-64 ABI: numbers, errno, structs (desk owns the structure; lanes add constants in their own files
//! `src/<lane>.rs`, e.g. `ab.rs`, `nw.rs`). Values from Linux uapi headers (MIT-compatible facts, no code).
#![no_std]
#![forbid(unsafe_code)]

pub mod errno;
pub mod nr;
pub mod ab;

/// auxv keys used at process start.
pub mod auxv {
    pub const AT_NULL: u64 = 0;
    pub const AT_PHDR: u64 = 3;
    pub const AT_PHENT: u64 = 4;
    pub const AT_PHNUM: u64 = 5;
    pub const AT_PAGESZ: u64 = 6;
    pub const AT_BASE: u64 = 7;
    pub const AT_FLAGS: u64 = 8;
    pub const AT_ENTRY: u64 = 9;
    pub const AT_UID: u64 = 11;
    pub const AT_EUID: u64 = 12;
    pub const AT_GID: u64 = 13;
    pub const AT_EGID: u64 = 14;
    pub const AT_PLATFORM: u64 = 15;
    pub const AT_HWCAP: u64 = 16;
    pub const AT_CLKTCK: u64 = 17;
    pub const AT_SECURE: u64 = 23;
    pub const AT_RANDOM: u64 = 25;
    pub const AT_HWCAP2: u64 = 26;
    pub const AT_EXECFN: u64 = 31;
    pub const AT_SYSINFO_EHDR: u64 = 33;
    pub const AT_MINSIGSTKSZ: u64 = 51;
}

pub mod mman {
    pub const PROT_READ: u64 = 1;
    pub const PROT_WRITE: u64 = 2;
    pub const PROT_EXEC: u64 = 4;
    pub const MAP_SHARED: u64 = 0x01;
    pub const MAP_PRIVATE: u64 = 0x02;
    pub const MAP_FIXED: u64 = 0x10;
    pub const MAP_ANONYMOUS: u64 = 0x20;
    pub const MAP_NORESERVE: u64 = 0x4000;
    pub const MAP_FIXED_NOREPLACE: u64 = 0x100000;
}

pub mod arch_prctl {
    pub const ARCH_SET_GS: u64 = 0x1001;
    pub const ARCH_SET_FS: u64 = 0x1002;
    pub const ARCH_GET_FS: u64 = 0x1003;
    pub const ARCH_GET_GS: u64 = 0x1004;
}

pub mod ioctl {
    pub const TCGETS: u64 = 0x5401;
    pub const TCSETS: u64 = 0x5402;
    pub const TIOCGWINSZ: u64 = 0x5413;
    pub const FIONREAD: u64 = 0x541B;
}

/// `struct winsize`
#[derive(Clone, Copy, Debug, Default)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

impl Winsize {
    pub fn to_bytes(&self) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[0..2].copy_from_slice(&self.rows.to_le_bytes());
        b[2..4].copy_from_slice(&self.cols.to_le_bytes());
        b[4..6].copy_from_slice(&self.xpixel.to_le_bytes());
        b[6..8].copy_from_slice(&self.ypixel.to_le_bytes());
        b
    }
}

/// `struct timespec`
#[derive(Clone, Copy, Debug, Default)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: i64,
}

impl Timespec {
    pub fn from_ns(ns: u64) -> Self {
        Timespec { sec: (ns / 1_000_000_000) as i64, nsec: (ns % 1_000_000_000) as i64 }
    }
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&self.sec.to_le_bytes());
        b[8..16].copy_from_slice(&self.nsec.to_le_bytes());
        b
    }
    pub fn from_bytes(b: &[u8; 16]) -> Self {
        let mut s = [0u8; 8];
        let mut n = [0u8; 8];
        s.copy_from_slice(&b[0..8]);
        n.copy_from_slice(&b[8..16]);
        Timespec { sec: i64::from_le_bytes(s), nsec: i64::from_le_bytes(n) }
    }
}

/// `struct stat` (x86-64 layout, 144 bytes).
#[derive(Clone, Copy, Debug, Default)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: i64,
    pub blksize: i64,
    pub blocks: i64,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
}

pub const S_IFMT: u32 = 0o170000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFIFO: u32 = 0o010000;
pub const S_IFLNK: u32 = 0o120000;

impl Stat {
    pub fn to_bytes(&self) -> [u8; 144] {
        let mut b = [0u8; 144];
        let w64 = |b: &mut [u8; 144], o: usize, v: u64| b[o..o + 8].copy_from_slice(&v.to_le_bytes());
        w64(&mut b, 0, self.dev);
        w64(&mut b, 8, self.ino);
        w64(&mut b, 16, self.nlink);
        b[24..28].copy_from_slice(&self.mode.to_le_bytes());
        b[28..32].copy_from_slice(&self.uid.to_le_bytes());
        b[32..36].copy_from_slice(&self.gid.to_le_bytes());
        w64(&mut b, 40, self.rdev);
        w64(&mut b, 48, self.size as u64);
        w64(&mut b, 56, self.blksize as u64);
        w64(&mut b, 64, self.blocks as u64);
        b[72..88].copy_from_slice(&self.atime.to_bytes());
        b[88..104].copy_from_slice(&self.mtime.to_bytes());
        b[104..120].copy_from_slice(&self.ctime.to_bytes());
        b
    }
}

/// `struct utsname`: six fields of 65 bytes.
pub fn utsname(sysname: &str, nodename: &str, release: &str, version: &str, machine: &str) -> [u8; 390] {
    let mut b = [0u8; 390];
    for (i, s) in [sysname, nodename, release, version, machine, "(none)"].iter().enumerate() {
        let n = s.len().min(64);
        b[i * 65..i * 65 + n].copy_from_slice(&s.as_bytes()[..n]);
    }
    b
}
