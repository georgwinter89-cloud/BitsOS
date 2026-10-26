//! Host test `sys_cases`: runs `host/tests/fs/cases.txt` (checked against Linux tmpfs by
//! `host/tests/fs/10-cases-linux.sh`) through the file syscalls (`sys::call`) on a fake user memory, as the C
//! runner `host/tests/fs/progs/fs-cases.c` does on CachyOS and on BitsOS. Same tokens, same rendering.

extern crate std;

use crate::fd::FdTable;
use crate::inode::Cred;
use crate::mount_ns::MountNs;
use crate::sys::{self, Sys, UserMem};
use crate::Errno;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use std::format;
use uapi::errno::*;

const CASES: &str = include_str!("../../../host/tests/fs/cases.txt");
pub(crate) const ARENA: u64 = 0x1000_0000;

/// User memory: one growing arena at `ARENA`; everything else faults.
pub(crate) struct Mem {
    bytes: RefCell<Vec<u8>>,
}

impl Mem {
    pub(crate) fn new() -> Mem {
        Mem { bytes: RefCell::new(Vec::new()) }
    }
    fn reset(&self) {
        self.bytes.borrow_mut().clear();
    }
    pub(crate) fn alloc(&self, data: &[u8]) -> u64 {
        let mut b = self.bytes.borrow_mut();
        let at = b.len().next_multiple_of(16);
        b.resize(at, 0);
        b.extend_from_slice(data);
        ARENA + at as u64
    }
    pub(crate) fn alloc_zero(&self, n: usize) -> u64 {
        self.alloc(&vec![0u8; n])
    }
    pub(crate) fn cstr(&self, s: &[u8]) -> u64 {
        let mut v = s.to_vec();
        v.push(0);
        self.alloc(&v)
    }
    pub(crate) fn get(&self, addr: u64, n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        assert!(self.read(addr, &mut v));
        v
    }
    fn range(&self, addr: u64, n: usize) -> Option<core::ops::Range<usize>> {
        let off = addr.checked_sub(ARENA)? as usize;
        let end = off.checked_add(n)?;
        (end <= self.bytes.borrow().len()).then_some(off..end)
    }
}

impl UserMem for Mem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        match self.range(addr, buf.len()) {
            Some(r) => {
                buf.copy_from_slice(&self.bytes.borrow()[r]);
                true
            }
            None => false,
        }
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        match self.range(addr, data.len()) {
            Some(r) => {
                self.bytes.borrow_mut()[r].copy_from_slice(data);
                true
            }
            None => false,
        }
    }
    fn read_cstr(&self, addr: u64, max: usize) -> Result<Vec<u8>, Errno> {
        let b = self.bytes.borrow();
        let off = addr.checked_sub(ARENA).ok_or(EFAULT)? as usize;
        if off >= b.len() {
            return Err(EFAULT);
        }
        let avail = &b[off..];
        match avail.iter().take(max).position(|&c| c == 0) {
            Some(n) => Ok(avail[..n].to_vec()),
            None if avail.len() >= max => Err(ENAMETOOLONG),
            None => Err(EFAULT),
        }
    }
}

const ERRS: &[(&str, i64)] = &[
    ("EPERM", 1), ("ENOENT", 2), ("EIO", 5), ("ENXIO", 6), ("EBADF", 9), ("EAGAIN", 11), ("ENOMEM", 12),
    ("EACCES", 13), ("EFAULT", 14), ("EBUSY", 16), ("EEXIST", 17), ("EXDEV", 18), ("ENOTDIR", 20),
    ("EISDIR", 21), ("EINVAL", 22), ("EMFILE", 24), ("ENOTTY", 25), ("EFBIG", 27), ("ENOSPC", 28),
    ("ESPIPE", 29), ("EROFS", 30), ("EMLINK", 31), ("ERANGE", 34), ("ENAMETOOLONG", 36), ("ENOSYS", 38),
    ("ENOTEMPTY", 39), ("ELOOP", 40), ("EOPNOTSUPP", 95), ("EINTR", 4), ("EOVERFLOW", 75), ("EPIPE", 32),
    ("ENODEV", 19),
];

const NAMES: &[(&str, i64)] = &[
    ("O_RDONLY", 0), ("O_WRONLY", 1), ("O_RDWR", 2), ("O_CREAT", 0o100), ("O_EXCL", 0o200), ("O_TRUNC", 0o1000),
    ("O_APPEND", 0o2000), ("O_NONBLOCK", 0o4000), ("O_DIRECTORY", 0o200000), ("O_NOFOLLOW", 0o400000),
    ("O_CLOEXEC", 0o2000000), ("O_PATH", 0o10000000), ("SEEK_SET", 0), ("SEEK_CUR", 1), ("SEEK_END", 2),
    ("SEEK_DATA", 3), ("SEEK_HOLE", 4), ("F_DUPFD", 0), ("F_GETFD", 1), ("F_SETFD", 2), ("F_GETFL", 3),
    ("F_SETFL", 4), ("F_DUPFD_CLOEXEC", 1030), ("F_OK", 0), ("R_OK", 4), ("W_OK", 2), ("X_OK", 1),
    ("AT_FDCWD", -100), ("AT_EMPTY_PATH", 0x1000), ("AT_SYMLINK_NOFOLLOW", 0x100), ("AT_REMOVEDIR", 0x200),
    ("AT_SYMLINK_FOLLOW", 0x400), ("O_TMPFILE", 0o20200000), ("F_GETLK", 5), ("F_SETLK", 6), ("F_SETLKW", 7),
    ("F_OFD_GETLK", 36), ("F_OFD_SETLK", 37), ("F_OFD_SETLKW", 38), ("F_RDLCK", 0), ("F_WRLCK", 1), ("F_UNLCK", 2),
    ("O_DIRECT", 0o40000), ("F_SETPIPE_SZ", 1031), ("F_GETPIPE_SZ", 1032), ("EFD_SEMAPHORE", 1),
    ("S_IFIFO", 0o10000), ("S_IFCHR", 0o20000), ("S_IFDIR", 0o40000), ("S_IFBLK", 0o60000), ("S_IFREG", 0o100000),
    ("S_IFSOCK", 0o140000), ("POLLIN", 1), ("POLLPRI", 2), ("POLLOUT", 4), ("POLLRDHUP", 0x2000),
    ("F_ADD_SEALS", 1033), ("F_GET_SEALS", 1034), ("F_SEAL_SEAL", 1), ("F_SEAL_SHRINK", 2), ("F_SEAL_GROW", 4),
    ("F_SEAL_WRITE", 8), ("F_SEAL_FUTURE_WRITE", 16), ("F_SEAL_EXEC", 32), ("MFD_CLOEXEC", 1),
    ("MFD_ALLOW_SEALING", 2), ("MFD_NOEXEC_SEAL", 8), ("MFD_EXEC", 16),
    ("FALLOC_FL_KEEP_SIZE", 1), ("FALLOC_FL_PUNCH_HOLE", 2), ("FALLOC_FL_COLLAPSE_RANGE", 8),
    ("FALLOC_FL_ZERO_RANGE", 16), ("FALLOC_FL_INSERT_RANGE", 32), ("FALLOC_FL_UNSHARE_RANGE", 64),
    ("UTIME_NOW", 0x3fffffff), ("UTIME_OMIT", 0x3ffffffe), ("LOCK_SH", 1), ("LOCK_EX", 2), ("LOCK_NB", 4),
    ("LOCK_UN", 8), ("RWF_HIPRI", 1), ("RWF_DSYNC", 2), ("RWF_SYNC", 4), ("RWF_NOWAIT", 8), ("RWF_APPEND", 16),
];

struct Tok {
    s: Vec<u8>,
    quoted: bool,
}

fn tokenize(line: &str) -> Vec<Tok> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
            i += 1;
        }
        if i >= b.len() || b[i] == b'#' {
            break;
        }
        if b[i] == b'"' {
            i += 1;
            let mut s = Vec::new();
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' && i + 1 < b.len() {
                    s.push(match b[i + 1] {
                        b'0' => 0,
                        b'n' => b'\n',
                        c => c,
                    });
                    i += 2;
                } else {
                    s.push(b[i]);
                    i += 1;
                }
            }
            i += 1;
            out.push(Tok { s, quoted: true });
        } else {
            let st = i;
            while i < b.len() && b[i] != b' ' && b[i] != b'\t' {
                i += 1;
            }
            out.push(Tok { s: b[st..i].to_vec(), quoted: false });
        }
    }
    out
}

/// C `strtol(s, 0, 0)`: sign, 0x hex, leading-0 octal, decimal; stops at the first bad digit.
fn strtol(s: &str) -> i64 {
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (radix, digits) = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        (16, h)
    } else if s.len() > 1 && s.starts_with('0') {
        (8, &s[1..])
    } else {
        (10, s)
    };
    let mut v: i64 = 0;
    for c in digits.chars() {
        match c.to_digit(radix) {
            Some(d) => v = v * radix as i64 + d as i64,
            None => break,
        }
    }
    if neg { -v } else { v }
}

struct Runner<'a> {
    s: Sys<'a>,
    mem: &'a Mem,
    base: String,
    vars: [i64; 26],
}

impl<'a> Runner<'a> {
    fn num(&self, t: &Tok) -> i64 {
        let s = core::str::from_utf8(&t.s).unwrap();
        if s.len() == 1 && s.as_bytes()[0].is_ascii_uppercase() {
            return self.vars[(s.as_bytes()[0] - b'A') as usize];
        }
        s.split('|').map(|p| NAMES.iter().find(|n| n.0 == p).map(|n| n.1).unwrap_or_else(|| strtol(p))).fold(0, |a, b| a | b)
    }
    fn path(&self, t: &Tok) -> Vec<u8> {
        if t.quoted {
            return t.s.clone();
        }
        let s = core::str::from_utf8(&t.s).unwrap();
        if let Some(r) = s.strip_prefix('$') {
            return format!("{}{}", self.base, r).into_bytes();
        }
        if let Some(n) = s.strip_prefix('%') {
            return vec![b'x'; n.parse().unwrap()];
        }
        if let Some(n) = s.strip_prefix('^') {
            let n: usize = n.parse().unwrap();
            let mut o = Vec::new();
            for _ in 0..(n - 1) / 2 {
                o.extend_from_slice(b"./");
            }
            o.resize(n, b'q');
            return o;
        }
        t.s.clone()
    }
    fn p(&self, t: &Tok) -> u64 {
        self.mem.cstr(&self.path(t))
    }
    fn sc(&self, nr: usize, a: [i64; 5]) -> i64 {
        sys::call(&self.s, nr, [a[0] as u64, a[1] as u64, a[2] as u64, a[3] as u64, a[4] as u64, 0])
    }
}

fn words(v: &[i64]) -> Vec<u8> {
    v.iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn put_long(r: i64) -> String {
    if r < 0
        && let Some(e) = ERRS.iter().find(|e| e.1 == -r)
    {
        return e.0.to_string();
    }
    r.to_string()
}

fn put_data(r: i64, data: &[u8]) -> String {
    if r < 0 {
        return put_long(r);
    }
    let mut o = String::from("\"");
    for &c in &data[..r as usize] {
        match c {
            0 => o.push_str("\\0"),
            b'\n' => o.push_str("\\n"),
            c => o.push(c as char),
        }
    }
    o.push('"');
    o
}

fn stat_fields(mode: u32, nlink: u64, size: i64) -> String {
    let t = match mode & 0o170000 {
        0o100000 => "reg",
        0o040000 => "dir",
        0o120000 => "lnk",
        0o020000 => "chr",
        0o010000 => "fifo",
        0o060000 => "blk",
        0o140000 => "sock",
        0 => "none",
        _ => "?",
    };
    format!("{t}:{:o}:{nlink}:{size}", mode & 0o7777)
}

fn put_stat(r: i64, st: &[u8]) -> String {
    if r < 0 {
        return put_long(r);
    }
    let nlink = u64::from_le_bytes(st[16..24].try_into().unwrap());
    let mode = u32::from_le_bytes(st[24..28].try_into().unwrap());
    let size = i64::from_le_bytes(st[48..56].try_into().unwrap());
    stat_fields(mode, nlink, size)
}

/// Records of one getdents64 answer: (name, reclen).
fn dirents(buf: &[u8]) -> Vec<Vec<u8>> {
    let mut v = Vec::new();
    let mut off = 0;
    while off < buf.len() {
        let reclen = u16::from_le_bytes([buf[off + 16], buf[off + 17]]) as usize;
        let name = &buf[off + 19..off + reclen];
        let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        v.push(name[..end].to_vec());
        off += reclen;
    }
    v
}

impl<'a> Runner<'a> {
    fn ls(&self, p: u64) -> String {
        let fd = self.sc(2, [p as i64, 0o200000, 0, 0, 0]);
        if fd < 0 {
            return put_long(fd);
        }
        let buf = self.mem.alloc_zero(65536);
        let mut names = Vec::new();
        let mut dots = 0;
        let r = loop {
            let r = self.sc(217, [fd, buf as i64, 65536, 0, 0]);
            if r <= 0 {
                break r;
            }
            for n in dirents(&self.mem.get(buf, r as usize)) {
                if n == b"." || n == b".." {
                    dots += 1;
                } else {
                    names.push(String::from_utf8(n).unwrap());
                }
            }
        };
        self.sc(3, [fd, 0, 0, 0, 0]);
        if r < 0 {
            return put_long(r);
        }
        if dots != 2 {
            return format!("dots={dots}");
        }
        names.sort();
        if names.is_empty() { "-".to_string() } else { names.join(",") }
    }

    fn fill(&self, p: u64, n: usize) -> String {
        let pat: Vec<u8> = (0..n).map(|i| (i * 7 % 251) as u8).collect();
        let b = self.mem.alloc(&pat);
        let c = self.mem.alloc_zero(n);
        let fd = self.sc(2, [p as i64, 0o102, 0o644, 0, 0]);
        if fd < 0 {
            return put_long(fd);
        }
        let w = self.sc(1, [fd, b as i64, n as i64, 0, 0]);
        let s = self.sc(8, [fd, 0, 0, 0, 0]);
        let r = self.sc(0, [fd, c as i64, n as i64, 0, 0]);
        self.sc(3, [fd, 0, 0, 0, 0]);
        if w != n as i64 || s != 0 || r != n as i64 || self.mem.get(c, n) != pat {
            return format!("w={w} s={s} r={r}");
        }
        "ok".to_string()
    }

    /// One case line; `Some(true)` passed, `Some(false)` failed with `got`, `None` a bad line.
    fn run(&mut self, line: &str) -> (Option<bool>, String) {
        self.mem.reset();
        let Some((call, expect)) = line.split_once("=>") else { return (None, String::new()) };
        let expect = expect.trim();
        let expect = if expect.starts_with('"') { &expect[..=expect.rfind('"').unwrap()] } else { expect.split_whitespace().next().unwrap_or("") };
        let t = tokenize(call);
        if t.is_empty() {
            return (None, String::new());
        }
        let c = core::str::from_utf8(&t[0].s).unwrap().to_string();
        let n = |i: usize| t.get(i).map(|x| self.num(x)).unwrap_or(0);
        let st = self.mem.alloc_zero(256);
        let buf = self.mem.alloc_zero(65536);
        let got = match c.as_str() {
            "open" => put_long(self.sc(2, [self.p(&t[1]) as i64, n(2), n(3), 0, 0])),
            "openat" => put_long(self.sc(257, [n(1), self.p(&t[2]) as i64, n(3), n(4), 0])),
            "creat" => put_long(self.sc(85, [self.p(&t[1]) as i64, n(2), 0, 0, 0])),
            "close" => put_long(self.sc(3, [n(1), 0, 0, 0, 0])),
            "read" => {
                let r = self.sc(0, [n(1), buf as i64, n(2), 0, 0]);
                put_data(r, &self.mem.get(buf, 65536))
            }
            "pread" => {
                let r = self.sc(17, [n(1), buf as i64, n(2), n(3), 0]);
                put_data(r, &self.mem.get(buf, 65536))
            }
            "write" => {
                let d = self.mem.alloc(&t[2].s);
                put_long(self.sc(1, [n(1), d as i64, t[2].s.len() as i64, 0, 0]))
            }
            "pwrite" => {
                let d = self.mem.alloc(&t[2].s);
                put_long(self.sc(18, [n(1), d as i64, t[2].s.len() as i64, n(3), 0]))
            }
            "readv" => {
                let (l1, l2) = (n(2), n(3));
                let mut iov = Vec::new();
                for v in [buf as i64, l1, buf as i64 + 32768, l2] {
                    iov.extend_from_slice(&v.to_le_bytes());
                }
                let iv = self.mem.alloc(&iov);
                let r = self.sc(19, [n(1), iv as i64, 2, 0, 0]);
                let mut data = self.mem.get(buf, 65536);
                if r >= 0 {
                    let a = r.min(l1) as usize;
                    let rest = r as usize - a;
                    data.copy_within(32768..32768 + rest, a);
                }
                put_data(r, &data)
            }
            "writev" => {
                let (a, b) = (self.mem.alloc(&t[2].s), self.mem.alloc(&t[3].s));
                let mut iov = Vec::new();
                for v in [a as i64, t[2].s.len() as i64, b as i64, t[3].s.len() as i64] {
                    iov.extend_from_slice(&v.to_le_bytes());
                }
                let iv = self.mem.alloc(&iov);
                put_long(self.sc(20, [n(1), iv as i64, 2, 0, 0]))
            }
            "lseek" => put_long(self.sc(8, [n(1), n(2), n(3), 0, 0])),
            "dup" => put_long(self.sc(32, [n(1), 0, 0, 0, 0])),
            "dup2" => put_long(self.sc(33, [n(1), n(2), 0, 0, 0])),
            "dup3" => put_long(self.sc(292, [n(1), n(2), n(3), 0, 0])),
            "fcntl" => {
                let r = self.sc(72, [n(1), n(2), n(3), 0, 0]);
                if r >= 0 && n(2) == 3 { format!("0{r:o}") } else { put_long(r) }
            }
            "lock" => {
                let mut fl = Vec::new();
                fl.extend_from_slice(&(n(3) as i16).to_le_bytes());
                fl.extend_from_slice(&(n(4) as i16).to_le_bytes());
                fl.extend_from_slice(&[0; 4]);
                fl.extend_from_slice(&n(5).to_le_bytes());
                fl.extend_from_slice(&n(6).to_le_bytes());
                fl.extend_from_slice(&(n(7) as i32).to_le_bytes());
                fl.extend_from_slice(&[0; 4]);
                let at = self.mem.alloc(&fl);
                let r = self.sc(72, [n(1), n(2), at as i64, 0, 0]);
                if r >= 0 && (n(2) == 5 || n(2) == 36) {
                    put_long(i16::from_le_bytes(self.mem.get(at, 2).try_into().unwrap()) as i64)
                } else {
                    put_long(r)
                }
            }
            "fsync" => put_long(self.sc(74, [n(1), 0, 0, 0, 0])),
            "stat" | "lstat" => {
                let r = self.sc(if c == "stat" { 4 } else { 6 }, [self.p(&t[1]) as i64, st as i64, 0, 0, 0]);
                put_stat(r, &self.mem.get(st, 256))
            }
            "fstat" => {
                let r = self.sc(5, [n(1), st as i64, 0, 0, 0]);
                put_stat(r, &self.mem.get(st, 256))
            }
            "fstatat" => {
                let r = self.sc(262, [n(1), self.p(&t[2]) as i64, st as i64, n(3), 0]);
                put_stat(r, &self.mem.get(st, 256))
            }
            "blocks" => {
                let r = self.sc(4, [self.p(&t[1]) as i64, st as i64, 0, 0, 0]);
                if r < 0 { put_long(r) } else { put_long(i64::from_le_bytes(self.mem.get(st + 64, 8).try_into().unwrap())) }
            }
            "statx" | "statxmask" => {
                let r = self.sc(332, [n(1), self.p(&t[2]) as i64, n(3), n(4), st as i64]);
                let x = self.mem.get(st, 256);
                if r < 0 {
                    put_long(r)
                } else if c == "statxmask" {
                    format!("0x{:x}", u32::from_le_bytes(x[0..4].try_into().unwrap()) & 0x1fff)
                } else {
                    let nlink = u32::from_le_bytes(x[16..20].try_into().unwrap());
                    let mode = u16::from_le_bytes(x[28..30].try_into().unwrap());
                    let size = u64::from_le_bytes(x[40..48].try_into().unwrap());
                    stat_fields(mode as u32, nlink as u64, size as i64)
                }
            }
            "access" => put_long(self.sc(21, [self.p(&t[1]) as i64, n(2), 0, 0, 0])),
            "faccessat2" => put_long(self.sc(439, [n(1), self.p(&t[2]) as i64, n(3), n(4), 0])),
            "ls" => self.ls(self.p(&t[1])),
            "getdents" => {
                let r = self.sc(217, [n(1), buf as i64, n(2), 0, 0]);
                if r < 0 { put_long(r) } else { put_long(dirents(&self.mem.get(buf, r as usize)).len() as i64) }
            }
            "mkdir" => put_long(self.sc(83, [self.p(&t[1]) as i64, n(2), 0, 0, 0])),
            "mkdirat" => put_long(self.sc(258, [n(1), self.p(&t[2]) as i64, n(3), 0, 0])),
            "unlink" => put_long(self.sc(87, [self.p(&t[1]) as i64, 0, 0, 0, 0])),
            "unlinkat" => put_long(self.sc(263, [n(1), self.p(&t[2]) as i64, n(3), 0, 0])),
            "rmdir" => put_long(self.sc(84, [self.p(&t[1]) as i64, 0, 0, 0, 0])),
            "rename" => put_long(self.sc(82, [self.p(&t[1]) as i64, self.p(&t[2]) as i64, 0, 0, 0])),
            "renameat2" => put_long(self.sc(316, [n(1), self.p(&t[2]) as i64, n(3), self.p(&t[4]) as i64, n(5)])),
            "link" => put_long(self.sc(86, [self.p(&t[1]) as i64, self.p(&t[2]) as i64, 0, 0, 0])),
            "linkat" => put_long(self.sc(265, [n(1), self.p(&t[2]) as i64, n(3), self.p(&t[4]) as i64, n(5)])),
            "symlink" => put_long(self.sc(88, [self.p(&t[1]) as i64, self.p(&t[2]) as i64, 0, 0, 0])),
            "symchain" => {
                let mut r = 0;
                for i in 0..n(1) {
                    if r < 0 {
                        break;
                    }
                    let a = self.mem.cstr(format!("c{i}").as_bytes());
                    let b = self.mem.cstr(if i == 0 { "f".to_string() } else { format!("c{}", i - 1) }.as_bytes());
                    r = self.sc(88, [b as i64, a as i64, 0, 0, 0]);
                }
                put_long(r)
            }
            "readlink" => {
                let r = self.sc(89, [self.p(&t[1]) as i64, buf as i64, n(2), 0, 0]);
                put_data(r, &self.mem.get(buf, 65536))
            }
            "readlinkat" => {
                let r = self.sc(267, [n(1), self.p(&t[2]) as i64, buf as i64, n(3), 0]);
                put_data(r, &self.mem.get(buf, 65536))
            }
            "getcwd" => {
                let r = self.sc(79, [buf as i64, n(1), 0, 0, 0]);
                if r < 0 {
                    put_long(r)
                } else {
                    let d = self.mem.get(buf, r as usize - 1);
                    let s = String::from_utf8(d).unwrap();
                    match s.strip_prefix(self.base.as_str()) {
                        Some(rest) => format!("\"${rest}\""),
                        None => format!("\"{s}\""),
                    }
                }
            }
            "chdir" => put_long(self.sc(80, [self.p(&t[1]) as i64, 0, 0, 0, 0])),
            "fchdir" => put_long(self.sc(81, [n(1), 0, 0, 0, 0])),
            "truncate" => put_long(self.sc(76, [self.p(&t[1]) as i64, n(2), 0, 0, 0])),
            "ftruncate" => put_long(self.sc(77, [n(1), n(2), 0, 0, 0])),
            "umask" => format!("0{:o}", self.sc(95, [n(1), 0, 0, 0, 0])),
            "fill" => self.fill(self.p(&t[1]), n(2) as usize),
            other => match self.fs3(other, &t) {
                Some((g, stores)) => {
                    for (v, x) in stores {
                        self.vars[(v - b'A') as usize] = x;
                    }
                    g
                }
                None => match self.fs2(other, &t) {
                    Some(g) => g,
                    None => return (None, String::new()),
                },
            },
        };
        if let Some(v) = expect.strip_prefix("fd:") {
            if !got.starts_with(|c: char| c.is_ascii_digit()) {
                return (Some(false), got);
            }
            self.vars[(v.as_bytes()[0] - b'A') as usize] = got.parse().unwrap();
            return (Some(true), got);
        }
        if let Some(v) = expect.strip_prefix("var:") {
            let want = self.vars[(v.as_bytes()[0] - b'A') as usize].to_string();
            return (Some(got == want), got);
        }
        if expect == "ok" {
            return (Some(got == "ok" || got.starts_with(|c: char| c.is_ascii_digit())), got);
        }
        (Some(got == expect), got)
    }
}

impl<'a> Runner<'a> {
    /// The FS-3 commands (pipes, FIFOs, mknod, eventfd) with the output of `fs3()` in fs-cases.c; returns the
    /// answer and the variables to store. ioctl is AB's syscall, so on the host FIONREAD goes through
    /// `File::ioctl`; poll is FS-4's own.
    fn fs3(&self, c: &str, t: &[Tok]) -> Option<(String, Vec<(u8, i64)>)> {
        let n = |i: usize| t.get(i).map(|x| self.num(x)).unwrap_or(0);
        let var = |i: usize| t.get(i).map(|x| x.s[0]).unwrap_or(b'Z');
        let mut stores = Vec::new();
        // FS-1b's commands use a stat/statfs buffer and a data buffer like `run`.
        let st = self.mem.alloc_zero(256);
        let buf = self.mem.alloc_zero(65536);
        let got = match c {
            "pipe2" | "pipe1" => {
                let b = self.mem.alloc_zero(8);
                let r = if c == "pipe1" { self.sc(22, [b as i64, 0, 0, 0, 0]) } else { self.sc(293, [b as i64, n(3), 0, 0, 0]) };
                if r == 0 {
                    let fds = self.mem.get(b, 8);
                    stores.push((var(1), i32::from_le_bytes(fds[0..4].try_into().unwrap()) as i64));
                    stores.push((var(2), i32::from_le_bytes(fds[4..8].try_into().unwrap()) as i64));
                }
                put_long(r)
            }
            "pipe2null" => put_long(self.sc(293, [0, n(1), 0, 0, 0])),
            "mknod" => put_long(self.sc(133, [self.p(&t[1]) as i64, n(2), n(3), 0, 0])),
            "mknodat" => put_long(self.sc(259, [n(1), self.p(&t[2]) as i64, n(3), n(4), 0])),
            "eventfd" => put_long(self.sc(290, [n(1), n(2), 0, 0, 0])),
            "memfd" => put_long(self.sc(319, [self.p(&t[1]) as i64, n(2), 0, 0, 0])),
            "memfdnull" => put_long(self.sc(319, [0, n(1), 0, 0, 0])),
            "eventfd1" => put_long(self.sc(284, [n(1), 0, 0, 0, 0])),
            "fallocate" => put_long(self.sc(285, [n(1), n(2), n(3), n(4), 0])),
            // FS-1b (the same ops as fs-cases.c)
            "chmod" => put_long(self.sc(90, [self.p(&t[1]) as i64, n(2), 0, 0, 0])),
            "fchmod" => put_long(self.sc(91, [n(1), n(2), 0, 0, 0])),
            "fchmodat" => put_long(self.sc(268, [n(1), self.p(&t[2]) as i64, n(3), 0, 0])),
            "fchmodat2" => put_long(self.sc(452, [n(1), self.p(&t[2]) as i64, n(3), n(4), 0])),
            "chown" => put_long(self.sc(92, [self.p(&t[1]) as i64, n(2), n(3), 0, 0])),
            "lchown" => put_long(self.sc(94, [self.p(&t[1]) as i64, n(2), n(3), 0, 0])),
            "fchown" => put_long(self.sc(93, [n(1), n(2), n(3), 0, 0])),
            "fchownat" => put_long(self.sc(260, [n(1), self.p(&t[2]) as i64, n(3), n(4), n(5)])),
            "utimens" => {
                let p = if t[2].s == b"NULL" { 0 } else { self.p(&t[2]) as i64 };
                let ts = if t[3].s == b"-" { 0 } else { self.mem.alloc(&words(&[n(3), n(4), n(5), n(6)])) as i64 };
                put_long(self.sc(280, [n(1), p, ts, n(7), 0]))
            }
            "utimes" => {
                let tv = self.mem.alloc(&words(&[n(2), n(3), n(4), n(5)]));
                put_long(self.sc(235, [self.p(&t[1]) as i64, tv as i64, 0, 0, 0]))
            }
            "futimesat" => {
                let p = if t[2].s == b"NULL" { 0 } else { self.p(&t[2]) as i64 };
                let tv = if t[3].s == b"-" { 0 } else { self.mem.alloc(&words(&[n(3), n(4), n(5), n(6)])) as i64 };
                put_long(self.sc(261, [n(1), p, tv, 0, 0]))
            }
            "utime" => {
                let ub = self.mem.alloc(&words(&[n(2), n(3)]));
                put_long(self.sc(132, [self.p(&t[1]) as i64, ub as i64, 0, 0, 0]))
            }
            "times" | "ctimenew" => {
                let r = self.sc(6, [self.p(&t[1]) as i64, st as i64, 0, 0, 0]);
                let b = self.mem.get(st, 144);
                let w = |o: usize| i64::from_le_bytes(b[o..o + 8].try_into().unwrap());
                match (r, c) {
                    (r, _) if r < 0 => put_long(r),
                    (_, "times") => format!("{}:{}", w(72), w(88)),
                    _ => put_long((w(104) >= w(88) && w(104) >= w(72)) as i64),
                }
            }
            "statfs" | "fstatfs" => {
                let r = if c == "statfs" { self.sc(137, [self.p(&t[1]) as i64, st as i64, 0, 0, 0]) } else { self.sc(138, [n(1), st as i64, 0, 0, 0]) };
                let b = self.mem.get(st, 120);
                let w = |i: usize| i64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
                if r < 0 { put_long(r) } else { format!("0x{:x}:{}:{}:{}", w(0), w(8), w(10) & 1, (w(9) == w(1)) as i64) }
            }
            "sync" => put_long(self.sc(162, [0; 5])),
            "syncfs" => put_long(self.sc(306, [n(1), 0, 0, 0, 0])),
            "fadvise" => put_long(self.sc(221, [n(1), n(2), n(3), n(4), 0])),
            "preadv" | "preadv2" => {
                let (l1, l2) = (n(2), n(3));
                let iv = self.mem.alloc(&words(&[buf as i64, l1, buf as i64 + 32768, l2]));
                let a = [n(1) as u64, iv, 2, n(4) as u64, 0, n(5) as u64];
                let r = sys::call(&self.s, if c == "preadv" { 295 } else { 327 }, a);
                let mut data = self.mem.get(buf, 65536);
                if r >= 0 {
                    let a = r.min(l1) as usize;
                    data.copy_within(32768..32768 + (r as usize - a), a);
                }
                put_data(r, &data)
            }
            "pwritev" | "pwritev2" => {
                let (a, b) = (self.mem.alloc(&t[2].s), self.mem.alloc(&t[3].s));
                let iv = self.mem.alloc(&words(&[a as i64, t[2].s.len() as i64, b as i64, t[3].s.len() as i64]));
                let args = [n(1) as u64, iv, 2, n(4) as u64, 0, n(5) as u64];
                put_long(sys::call(&self.s, if c == "pwritev" { 296 } else { 328 }, args))
            }
            "sendfile" => {
                let off = if t[3].s == b"-" { 0 } else { self.mem.alloc(&n(3).to_le_bytes()) };
                let r = self.sc(40, [n(1), n(2), off as i64, n(4), 0]);
                if r >= 0 && off != 0 { format!("{r},{}", i64::from_le_bytes(self.mem.get(off, 8).try_into().unwrap())) } else { put_long(r) }
            }
            "copyrange" => {
                let oi = if t[2].s == b"-" { 0 } else { self.mem.alloc(&n(2).to_le_bytes()) };
                let oo = if t[4].s == b"-" { 0 } else { self.mem.alloc(&n(4).to_le_bytes()) };
                put_long(sys::call(&self.s, 326, [n(1) as u64, oi, n(3) as u64, oo, n(5) as u64, n(6) as u64]))
            }
            "fionread" => match self.s.fds.get(n(1) as usize) {
                Ok(f) if !f.is_path_only() => match f.file.ioctl(uapi::ioctl::FIONREAD, 0) {
                    Ok(b) => put_long(i32::from_le_bytes(b[..4].try_into().unwrap()) as i64),
                    Err(e) => put_long(-e),
                },
                _ => put_long(-EBADF),
            },
            "poll" => {
                // FS-4: the real poll(2) of `pollsys` (one entry, no waiting), answering the entry's revents.
                let mut e = [0u8; 8];
                e[0..4].copy_from_slice(&(n(1) as i32).to_le_bytes());
                e[4..6].copy_from_slice(&(n(2) as u16).to_le_bytes());
                let b = self.mem.alloc(&e);
                let r = self.sc(7, [b as i64, 1, 0, 0, 0]);
                if r < 0 {
                    put_long(r)
                } else {
                    let got = self.mem.get(b, 8);
                    put_long(u16::from_le_bytes([got[6], got[7]]) as i64)
                }
            }
            "writen" => {
                let d = self.mem.alloc(&vec![b'x'; n(2) as usize]);
                put_long(self.sc(1, [n(1), d as i64, n(2), 0, 0]))
            }
            "readn" => {
                let d = self.mem.alloc_zero(n(2) as usize);
                put_long(self.sc(0, [n(1), d as i64, n(2), 0, 0]))
            }
            "fillnb" => {
                let k = n(2);
                let d = self.mem.alloc(&vec![b'y'; k as usize]);
                let mut total = 0i64;
                let r = loop {
                    let r = self.sc(1, [n(1), d as i64, k, 0, 0]);
                    if r > 0 {
                        total += r;
                    }
                    if r != k {
                        break r;
                    }
                };
                format!("{total},{}", put_long(r))
            }
            "efdw" => {
                let d = self.mem.alloc(&(n(2) as u64).to_le_bytes());
                self.mem.alloc_zero(64);
                put_long(self.sc(1, [n(1), d as i64, n(3), 0, 0]))
            }
            "efdr" => {
                let d = self.mem.alloc_zero(n(2).clamp(8, 4096) as usize);
                let r = self.sc(0, [n(1), d as i64, n(2), 0, 0]);
                if r == 8 { u64::from_le_bytes(self.mem.get(d, 8).try_into().unwrap()).to_string() } else { put_long(r) }
            }
            "efdrv" => {
                let a = self.mem.alloc_zero(4096);
                let b = self.mem.alloc_zero(4096);
                let mut iov = Vec::new();
                for v in [a as i64, n(2), b as i64, n(3)] {
                    iov.extend_from_slice(&v.to_le_bytes());
                }
                let iv = self.mem.alloc(&iov);
                let r = self.sc(19, [n(1), iv as i64, 2, 0, 0]);
                if r == 8 {
                    let k = n(2).min(8) as usize;
                    let mut v = self.mem.get(a, k);
                    v.extend_from_slice(&self.mem.get(b, 8 - k));
                    u64::from_le_bytes(v.try_into().unwrap()).to_string()
                } else {
                    put_long(r)
                }
            }
            "efdwv" => {
                let a = self.mem.alloc(&1u64.to_le_bytes());
                self.mem.alloc_zero(64);
                let b = self.mem.alloc(&1u64.to_le_bytes());
                self.mem.alloc_zero(64);
                let mut iov = Vec::new();
                for v in [a as i64, n(2), b as i64, n(3)] {
                    iov.extend_from_slice(&v.to_le_bytes());
                }
                let iv = self.mem.alloc(&iov);
                put_long(self.sc(20, [n(1), iv as i64, 2, 0, 0]))
            }
            "readfault" => put_long(self.sc(0, [n(1), 0x10, n(2), 0, 0])),
            "writefault" => put_long(self.sc(1, [n(1), 0x10, n(2), 0, 0])),
            _ => return None,
        };
        Some((got, stores))
    }
}

fn ftype(mode: u32) -> &'static str {
    match mode & 0o170000 {
        0o100000 => "reg",
        0o040000 => "dir",
        0o120000 => "lnk",
        0o020000 => "chr",
        0o060000 => "blk",
        0o010000 => "fifo",
        _ => "?",
    }
}

fn yes(b: bool) -> String {
    (if b { "yes" } else { "no" }).to_string()
}

impl<'a> Runner<'a> {
    /// The FS-2 commands of dev-cases.txt, with the output of `fs2()` in fs-cases.c. The commands about the
    /// runner's own process (`self`, `cmdline`, `grep`, `field`, the token `pid`) are not run on the host.
    fn fs2(&self, c: &str, t: &[Tok]) -> Option<String> {
        let arg = |i: usize| t.get(i).map(|x| x.s.clone()).unwrap_or_default();
        Some(match c {
            "has" => self.has(self.p(&t[1]), &String::from_utf8(arg(2)).ok()?),
            "rdev" | "mode" => {
                let st = self.mem.alloc_zero(256);
                let r = self.sc(if c == "mode" { 6 } else { 4 }, [self.p(&t[1]) as i64, st as i64, 0, 0, 0]);
                if r < 0 {
                    return Some(put_long(r));
                }
                let b = self.mem.get(st, 256);
                if c == "rdev" {
                    let d = u64::from_le_bytes(b[40..48].try_into().unwrap());
                    format!("{}:{}", (d >> 8) & 0xfff, (d & 0xff) | ((d >> 12) & !0xff))
                } else {
                    let mode = u32::from_le_bytes(b[24..28].try_into().unwrap());
                    format!("{}:{:o}", ftype(mode), mode & 0o7777)
                }
            }
            "nonzero" => {
                let n = self.num(&t[2]);
                let b = self.mem.alloc_zero(65536);
                let r = self.sc(0, [self.num(&t[1]), b as i64, n, 0, 0]);
                if r > 8 && self.mem.get(b, r as usize).iter().all(|&x| x == 0) { "zeros".to_string() } else { put_long(r) }
            }
            "key" | "text" | "lines" | "fields" => {
                let (r, file) = self.slurp(self.p(&t[1]));
                if r < 0 {
                    return Some(put_long(r));
                }
                let s = String::from_utf8_lossy(&file).into_owned();
                let a2 = String::from_utf8_lossy(&arg(2)).into_owned();
                match c {
                    "text" => yes(s.contains(&a2)),
                    "lines" => s.matches('\n').count().to_string(),
                    "fields" => s.split('\n').next().unwrap_or("").split([' ', '\t']).filter(|w| !w.is_empty()).count().to_string(),
                    _ => yes(s.split('\n').any(|l| l.starts_with(&a2))),
                }
            }
            _ => return None,
        })
    }

    /// The whole file read until end of file (proc files have no size): (result, bytes).
    fn slurp(&self, p: u64) -> (i64, Vec<u8>) {
        let fd = self.sc(2, [p as i64, 0, 0, 0, 0]);
        if fd < 0 {
            return (fd, Vec::new());
        }
        let b = self.mem.alloc_zero(65536);
        let mut out = Vec::new();
        let r = loop {
            let r = self.sc(0, [fd, b as i64, 65536, 0, 0]);
            if r <= 0 {
                break r;
            }
            out.extend_from_slice(&self.mem.get(b, r as usize));
        };
        self.sc(3, [fd, 0, 0, 0, 0]);
        if r < 0 && out.is_empty() { (r, out) } else { (out.len() as i64, out) }
    }

    /// `has DIR a,b,c`: every name is listed by getdents64 ("yes", else "missing:NAME").
    fn has(&self, p: u64, list: &str) -> String {
        let fd = self.sc(2, [p as i64, 0o200000, 0, 0, 0]);
        if fd < 0 {
            return put_long(fd);
        }
        // A small buffer, so the listing takes several calls and positions are exercised.
        let buf = self.mem.alloc_zero(200);
        let mut names: Vec<Vec<u8>> = Vec::new();
        let r = loop {
            let r = self.sc(217, [fd, buf as i64, 200, 0, 0]);
            if r <= 0 {
                break r;
            }
            names.extend(dirents(&self.mem.get(buf, r as usize)));
        };
        self.sc(3, [fd, 0, 0, 0, 0]);
        if r < 0 {
            return put_long(r);
        }
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        if sorted.len() != names.len() {
            return "listed twice".to_string();
        }
        for want in list.split(',') {
            if !names.iter().any(|n| n == want.as_bytes()) {
                return format!("missing:{want}");
            }
        }
        "yes".to_string()
    }
}

const DEV_CASES: &str = include_str!("../../../host/tests/fs/dev-cases.txt");

/// Lines about the runner's own process: only on Linux and on V2 (ktest `fs.calls.dev_cases`).
fn needs_process(l: &str) -> bool {
    let w: Vec<&str> = l.split_whitespace().collect();
    l.contains("/proc/self") || matches!(w.first(), Some(&"self" | &"cmdline" | &"grep" | &"field")) || w.iter().any(|x| *x == "pid")
}

/// FS-2: host/tests/fs/dev-cases.txt through the file syscalls on a view populated like the boot view, with
/// stand-ins for the content providers that live above `vfs` (meminfo, cpuinfo, uptime, PCI's view).
#[test]
fn dev_cases() {
    let (ns, fds) = program_view();
    crate::fs2_tests::fake_system(&ns);
    let mem = Mem { bytes: RefCell::new(Vec::new()) };
    let s = Sys { mem: &mem, fds: &fds, ns: &ns, cred: USER, subject: SUBJECT };
    let mut r = Runner { s, mem: &mem, base: "/tmp/dev-cases-100".to_string(), vars: [0; 26] };
    r.sc(95, [0o22, 0, 0, 0, 0]);
    let b = mem.cstr(r.base.as_bytes());
    assert_eq!(r.sc(83, [b as i64, 0o755, 0, 0, 0]), 0);
    let b = mem.cstr(r.base.as_bytes());
    assert_eq!(r.sc(80, [b as i64, 0, 0, 0, 0]), 0);
    let (mut pass, mut skipped, mut fails) = (0, 0, Vec::new());
    for (i, line) in DEV_CASES.lines().enumerate() {
        let l = line.trim_start();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if needs_process(l) {
            skipped += 1;
            continue;
        }
        match r.run(line) {
            (Some(true), _) => pass += 1,
            (Some(false), got) => fails.push(format!("FAIL {}: {} => got {}", i + 1, line, got)),
            (None, _) => fails.push(format!("FAIL {}: {} (bad case line)", i + 1, line)),
        }
    }
    for f in &fails {
        std::println!("{f}");
    }
    std::println!("dev_cases: {pass} passed, {} failed, {skipped} need a process", fails.len());
    assert!(fails.is_empty(), "{} of {} cases differ from Linux", fails.len(), pass + fails.len());
    assert!(pass > 100);
}

/// A fresh view with `/tmp` (1777) like the boot view, and a caller with uid 1000 like a program.
pub(crate) fn program_view() -> (Arc<MountNs>, FdTable) {
    let ns = Arc::new(MountNs::new_empty());
    ns.mkdir_p("/tmp").unwrap().inode.meta.set_perm(0o1777);
    (ns, FdTable::new())
}

pub(crate) const USER: Cred = Cred { uid: 1000, gid: 1000 };
pub(crate) const SUBJECT: policy::Subject = policy::Subject { program: policy::ProgramId(7), pid: 100, uid: 1000 };

#[test]
fn sys_cases() {
    let (ns, fds) = program_view();
    // A second file system, as on Linux and in the boot view (copy_file_range's EXDEV case).
    crate::tmpfs::mount_new(&ns, "/dev/shm", 0o1777, None).unwrap();
    let mem = Mem { bytes: RefCell::new(Vec::new()) };
    let s = Sys { mem: &mem, fds: &fds, ns: &ns, cred: USER, subject: SUBJECT };
    let mut r = Runner { s, mem: &mem, base: "/tmp/fs-cases-100".to_string(), vars: [0; 26] };
    // The runner's start: umask 022, make the base, enter it.
    r.sc(95, [0o22, 0, 0, 0, 0]);
    let b = mem.cstr(r.base.as_bytes());
    assert_eq!(r.sc(83, [b as i64, 0o755, 0, 0, 0]), 0);
    let b = mem.cstr(r.base.as_bytes());
    assert_eq!(r.sc(80, [b as i64, 0, 0, 0, 0]), 0);
    let (mut pass, mut fails) = (0, Vec::new());
    for (i, line) in CASES.lines().enumerate() {
        let l = line.trim_start();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        match r.run(line) {
            (Some(true), _) => pass += 1,
            (Some(false), got) => fails.push(format!("FAIL {}: {} => got {}", i + 1, line, got)),
            (None, _) => fails.push(format!("FAIL {}: {} (bad case line)", i + 1, line)),
        }
    }
    for f in &fails {
        std::println!("{f}");
    }
    std::println!("sys_cases: {pass} passed, {} failed", fails.len());
    assert!(fails.is_empty(), "{} of {} cases differ from Linux", fails.len(), pass + fails.len());
    assert!(pass > 350);
}
