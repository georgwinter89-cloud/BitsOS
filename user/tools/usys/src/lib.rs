//! usys: raw Linux x86-64 system calls for UL's programs (`init`, `stress`, the `abi-*` tests).
//!
//! The programs themselves are `#![forbid(unsafe_code)]`; every `unsafe` block they need is here, each with a
//! `SAFETY:` comment. The rule this crate keeps: the kernel only ever writes into memory that a wrapper passed as
//! a Rust buffer it owns for the duration of the call (a `&mut` slice or a local), and no wrapper can make the
//! kernel unmap, remap or re-protect memory Rust still uses (mappings are owned by [`Mapping`]; the "probe"
//! calls that take raw addresses only run when the arguments cannot touch a usable page, see [`harmless`]).
//! Results are `Result<_, Errno>` with Linux's raw errno, so a transcript shows exactly what the kernel said.
//!
//! The calls go straight to `syscall` (no musl wrapper in between), so musl's own emulations and fallbacks never
//! hide a kernel difference (linux-6.6/arch/x86/entry/syscalls/syscall_64.tbl for the numbers).

use std::ffi::CString;
use std::fmt;
use std::sync::atomic::AtomicU32;

pub mod dev;
pub mod session;

pub mod nr {
    pub const READ: usize = 0;
    pub const WRITE: usize = 1;
    pub const OPEN: usize = 2;
    pub const CLOSE: usize = 3;
    pub const STAT: usize = 4;
    pub const FSTAT: usize = 5;
    pub const LSTAT: usize = 6;
    pub const POLL: usize = 7;
    pub const LSEEK: usize = 8;
    pub const MMAP: usize = 9;
    pub const MPROTECT: usize = 10;
    pub const MUNMAP: usize = 11;
    pub const BRK: usize = 12;
    pub const RT_SIGACTION: usize = 13;
    pub const RT_SIGPROCMASK: usize = 14;
    pub const IOCTL: usize = 16;
    pub const PREAD64: usize = 17;
    pub const PWRITE64: usize = 18;
    pub const READV: usize = 19;
    pub const WRITEV: usize = 20;
    pub const ACCESS: usize = 21;
    pub const PIPE: usize = 22;
    pub const SCHED_YIELD: usize = 24;
    pub const MREMAP: usize = 25;
    pub const MSYNC: usize = 26;
    pub const MINCORE: usize = 27;
    pub const MADVISE: usize = 28;
    pub const DUP: usize = 32;
    pub const DUP2: usize = 33;
    pub const NANOSLEEP: usize = 35;
    pub const GETPID: usize = 39;
    pub const SOCKET: usize = 41;
    pub const CONNECT: usize = 42;
    pub const SENDTO: usize = 44;
    pub const RECVFROM: usize = 45;
    pub const SENDMSG: usize = 46;
    pub const RECVMSG: usize = 47;
    pub const SHUTDOWN: usize = 48;
    pub const BIND: usize = 49;
    pub const LISTEN: usize = 50;
    pub const GETSOCKNAME: usize = 51;
    pub const GETPEERNAME: usize = 52;
    pub const SOCKETPAIR: usize = 53;
    pub const SETSOCKOPT: usize = 54;
    pub const GETSOCKOPT: usize = 55;
    pub const CLONE: usize = 56;
    pub const EXIT: usize = 60;
    pub const WAIT4: usize = 61;
    pub const KILL: usize = 62;
    pub const UNAME: usize = 63;
    pub const FCNTL: usize = 72;
    pub const FSYNC: usize = 74;
    pub const FDATASYNC: usize = 75;
    pub const TRUNCATE: usize = 76;
    pub const FTRUNCATE: usize = 77;
    pub const GETCWD: usize = 79;
    pub const CHDIR: usize = 80;
    pub const FCHDIR: usize = 81;
    pub const RENAME: usize = 82;
    pub const MKDIR: usize = 83;
    pub const RMDIR: usize = 84;
    pub const LINK: usize = 86;
    pub const UNLINK: usize = 87;
    pub const SYMLINK: usize = 88;
    pub const READLINK: usize = 89;
    pub const CHMOD: usize = 90;
    pub const FCHMOD: usize = 91;
    pub const FCHOWN: usize = 93;
    pub const UMASK: usize = 95;
    pub const GETTIMEOFDAY: usize = 96;
    pub const SYSINFO: usize = 99;
    pub const GETUID: usize = 102;
    pub const GETGID: usize = 104;
    pub const GETEUID: usize = 107;
    pub const GETEGID: usize = 108;
    pub const GETPPID: usize = 110;
    pub const GETPGRP: usize = 111;
    pub const GETPGID: usize = 121;
    pub const SIGALTSTACK: usize = 131;
    pub const STATFS: usize = 137;
    pub const FSTATFS: usize = 138;
    pub const SCHED_GETPARAM: usize = 143;
    pub const SCHED_GETSCHEDULER: usize = 145;
    pub const SCHED_GET_PRIORITY_MAX: usize = 146;
    pub const SCHED_GET_PRIORITY_MIN: usize = 147;
    pub const PRCTL: usize = 157;
    pub const ARCH_PRCTL: usize = 158;
    pub const MOUNT: usize = 165;
    pub const SETHOSTNAME: usize = 170;
    pub const GETTID: usize = 186;
    pub const TKILL: usize = 200;
    pub const FUTEX: usize = 202;
    pub const SCHED_GETAFFINITY: usize = 204;
    pub const GETDENTS64: usize = 217;
    pub const SET_TID_ADDRESS: usize = 218;
    pub const CLOCK_GETTIME: usize = 228;
    pub const CLOCK_GETRES: usize = 229;
    pub const CLOCK_NANOSLEEP: usize = 230;
    pub const EXIT_GROUP: usize = 231;
    pub const EPOLL_WAIT: usize = 232;
    pub const EPOLL_CTL: usize = 233;
    pub const TGKILL: usize = 234;
    pub const OPENAT: usize = 257;
    pub const MKDIRAT: usize = 258;
    pub const NEWFSTATAT: usize = 262;
    pub const UNLINKAT: usize = 263;
    pub const RENAMEAT: usize = 264;
    pub const FACCESSAT: usize = 269;
    pub const PPOLL: usize = 271;
    pub const UTIMENSAT: usize = 280;
    pub const EPOLL_PWAIT: usize = 281;
    pub const ACCEPT4: usize = 288;
    pub const EVENTFD2: usize = 290;
    pub const EPOLL_CREATE1: usize = 291;
    pub const DUP3: usize = 292;
    pub const PIPE2: usize = 293;
    pub const PRLIMIT64: usize = 302;
    pub const GETCPU: usize = 309;
    pub const RENAMEAT2: usize = 316;
    pub const GETRANDOM: usize = 318;
    pub const MEMFD_CREATE: usize = 319;
}

/// A Linux errno as the kernel returned it (`-4095..-1` in rax). `Errno(0)` is never a kernel answer: the
/// probes return it when they refused to make a call that could have touched usable memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Errno(pub i32);

pub const EPERM: Errno = Errno(1);
pub const ENOENT: Errno = Errno(2);
pub const EINTR: Errno = Errno(4);
pub const EBADF: Errno = Errno(9);
pub const ECHILD: Errno = Errno(10);
pub const EAGAIN: Errno = Errno(11);
pub const EACCES: Errno = Errno(13);
pub const EEXIST: Errno = Errno(17);
pub const EINVAL: Errno = Errno(22);
pub const ENOSYS: Errno = Errno(38);
pub const REFUSED: Errno = Errno(0);

impl Errno {
    pub fn name(self) -> String {
        let n = match self.0 {
            0 => "REFUSED_LOCALLY",
            1 => "EPERM",
            2 => "ENOENT",
            3 => "ESRCH",
            4 => "EINTR",
            5 => "EIO",
            6 => "ENXIO",
            7 => "E2BIG",
            8 => "ENOEXEC",
            9 => "EBADF",
            10 => "ECHILD",
            11 => "EAGAIN",
            12 => "ENOMEM",
            13 => "EACCES",
            14 => "EFAULT",
            15 => "ENOTBLK",
            16 => "EBUSY",
            17 => "EEXIST",
            18 => "EXDEV",
            19 => "ENODEV",
            20 => "ENOTDIR",
            21 => "EISDIR",
            22 => "EINVAL",
            23 => "ENFILE",
            24 => "EMFILE",
            25 => "ENOTTY",
            26 => "ETXTBSY",
            27 => "EFBIG",
            28 => "ENOSPC",
            29 => "ESPIPE",
            30 => "EROFS",
            31 => "EMLINK",
            32 => "EPIPE",
            33 => "EDOM",
            34 => "ERANGE",
            35 => "EDEADLK",
            36 => "ENAMETOOLONG",
            37 => "ENOLCK",
            38 => "ENOSYS",
            39 => "ENOTEMPTY",
            40 => "ELOOP",
            42 => "ENOMSG",
            61 => "ENODATA",
            75 => "EOVERFLOW",
            88 => "ENOTSOCK",
            89 => "EDESTADDRREQ",
            90 => "EMSGSIZE",
            91 => "EPROTOTYPE",
            92 => "ENOPROTOOPT",
            93 => "EPROTONOSUPPORT",
            94 => "ESOCKTNOSUPPORT",
            95 => "EOPNOTSUPP",
            96 => "EPFNOSUPPORT",
            97 => "EAFNOSUPPORT",
            98 => "EADDRINUSE",
            99 => "EADDRNOTAVAIL",
            100 => "ENETDOWN",
            101 => "ENETUNREACH",
            103 => "ECONNABORTED",
            104 => "ECONNRESET",
            105 => "ENOBUFS",
            106 => "EISCONN",
            107 => "ENOTCONN",
            110 => "ETIMEDOUT",
            111 => "ECONNREFUSED",
            113 => "EHOSTUNREACH",
            114 => "EALREADY",
            115 => "EINPROGRESS",
            _ => return format!("E{}", self.0),
        };
        n.to_string()
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "-{}", self.name())
    }
}

pub type SysResult<T = usize> = Result<T, Errno>;

/// The kernel's answer as a transcript string: `= n` or `-ENAME`.
pub fn show(r: SysResult) -> String {
    match r {
        Ok(v) => format!("= {v}"),
        Err(e) => e.to_string(),
    }
}

/// Only the outcome kind: `ok` or `-ENAME` (for results whose value differs between machines: fds, pids,
/// addresses).
pub fn kind<T>(r: &SysResult<T>) -> String {
    match r {
        Ok(_) => "ok".to_string(),
        Err(e) => e.to_string(),
    }
}

#[inline]
fn ret(r: isize) -> SysResult {
    if (-4095..0).contains(&r) { Err(Errno(-r as i32)) } else { Ok(r as usize) }
}

/// The `syscall` instruction with six arguments (Linux x86-64 convention: rax, rdi, rsi, rdx, r10, r8, r9;
/// rcx and r11 are clobbered).
///
/// # Safety
/// The caller guarantees that every argument the kernel may treat as a pointer refers to memory the call may
/// read or write as the syscall's documentation says, and that the call does not change memory Rust code still
/// uses in a way Rust cannot see (unmap, remap, protect, set a new stack).
#[inline]
unsafe fn raw(n: usize, a: [usize; 6]) -> isize {
    let r: isize;
    // SAFETY: the instruction itself only switches to the kernel and back; what the kernel does with the
    // arguments is the caller's contract above. `nostack`: the syscall instruction does not use the user stack.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") n as isize => r,
            in("rdi") a[0], in("rsi") a[1], in("rdx") a[2], in("r10") a[3], in("r8") a[4], in("r9") a[5],
            lateout("rcx") _, lateout("r11") _,
            options(nostack)
        );
    }
    r
}

/// A syscall whose arguments are all plain integers for the kernel (no pointer is read or written).
fn ints(n: usize, args: &[usize]) -> SysResult {
    let mut a = [0usize; 6];
    for (d, s) in a.iter_mut().zip(args) {
        *d = *s;
    }
    // SAFETY: callers pass only syscalls whose arguments are not pointers (checked per wrapper below), so the
    // kernel touches no user memory; none of them unmaps or re-protects memory.
    ret(unsafe { raw(n, a) })
}

/// A path as a C string; an interior NUL (never in our tests) gives an empty path, which the kernel answers
/// with ENOENT. Never panics.
pub fn cpath(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

/// True when a raw address range cannot touch any page this program uses: unaligned (the kernel refuses before
/// looking at memory), empty, entirely below `mmap_min_addr` (64 KiB, never mappable for us), or at or above
/// the user-space end.
pub fn harmless(addr: usize, len: usize) -> bool {
    const USER_END: usize = 0x0000_7fff_ffff_f000;
    addr % 4096 != 0 || len == 0 || addr.checked_add(len).is_none_or(|e| e <= 0x10000) || addr >= USER_END
}

// ---------------------------------------------------------------------------------------------------------
// Integer-only calls

pub fn getpid() -> SysResult {
    ints(nr::GETPID, &[])
}
pub fn getppid() -> SysResult {
    ints(nr::GETPPID, &[])
}
pub fn gettid() -> SysResult {
    ints(nr::GETTID, &[])
}
pub fn getuid() -> SysResult {
    ints(nr::GETUID, &[])
}
pub fn geteuid() -> SysResult {
    ints(nr::GETEUID, &[])
}
pub fn getgid() -> SysResult {
    ints(nr::GETGID, &[])
}
pub fn getegid() -> SysResult {
    ints(nr::GETEGID, &[])
}
pub fn getpgrp() -> SysResult {
    ints(nr::GETPGRP, &[])
}
pub fn getpgid(pid: usize) -> SysResult {
    ints(nr::GETPGID, &[pid])
}
pub fn sched_yield() -> SysResult {
    ints(nr::SCHED_YIELD, &[])
}
pub fn kill(pid: isize, sig: usize) -> SysResult {
    ints(nr::KILL, &[pid as usize, sig])
}
pub fn tkill(tid: usize, sig: usize) -> SysResult {
    ints(nr::TKILL, &[tid, sig])
}
pub fn tgkill(tgid: usize, tid: usize, sig: usize) -> SysResult {
    ints(nr::TGKILL, &[tgid, tid, sig])
}
pub fn umask(mask: usize) -> SysResult {
    ints(nr::UMASK, &[mask])
}
pub fn close(fd: usize) -> SysResult {
    ints(nr::CLOSE, &[fd])
}
pub fn dup(fd: usize) -> SysResult {
    ints(nr::DUP, &[fd])
}
pub fn dup2(fd: usize, to: usize) -> SysResult {
    ints(nr::DUP2, &[fd, to])
}
pub fn dup3(fd: usize, to: usize, flags: usize) -> SysResult {
    ints(nr::DUP3, &[fd, to, flags])
}
pub fn lseek(fd: usize, off: i64, whence: usize) -> SysResult {
    ints(nr::LSEEK, &[fd, off as usize, whence])
}
pub fn fsync(fd: usize) -> SysResult {
    ints(nr::FSYNC, &[fd])
}
pub fn fdatasync(fd: usize) -> SysResult {
    ints(nr::FDATASYNC, &[fd])
}
pub fn ftruncate(fd: usize, len: i64) -> SysResult {
    ints(nr::FTRUNCATE, &[fd, len as usize])
}
/// memfd_create(name, flags) (linux-6.6/mm/memfd.c): a new fd.
pub fn memfd_create(name: &str, flags: usize) -> SysResult {
    let p = cpath(name);
    // SAFETY: p is a NUL-terminated string the kernel only reads.
    ret(unsafe { raw(nr::MEMFD_CREATE, [p.as_ptr() as usize, flags, 0, 0, 0, 0]) })
}
pub fn fchdir(fd: usize) -> SysResult {
    ints(nr::FCHDIR, &[fd])
}
pub fn fchmod(fd: usize, mode: usize) -> SysResult {
    ints(nr::FCHMOD, &[fd, mode])
}
pub fn fchown(fd: usize, uid: u32, gid: u32) -> SysResult {
    ints(nr::FCHOWN, &[fd, uid as usize, gid as usize])
}
pub fn listen(fd: usize, backlog: usize) -> SysResult {
    ints(nr::LISTEN, &[fd, backlog])
}
pub fn shutdown(fd: usize, how: usize) -> SysResult {
    ints(nr::SHUTDOWN, &[fd, how])
}
pub fn socket(domain: usize, ty: usize, proto: usize) -> SysResult {
    ints(nr::SOCKET, &[domain, ty, proto])
}
pub fn eventfd2(init: u32, flags: usize) -> SysResult {
    ints(nr::EVENTFD2, &[init as usize, flags])
}
pub fn epoll_create1(flags: usize) -> SysResult {
    ints(nr::EPOLL_CREATE1, &[flags])
}
pub fn sched_getscheduler(pid: usize) -> SysResult {
    ints(nr::SCHED_GETSCHEDULER, &[pid])
}
pub fn sched_get_priority_max(policy: usize) -> SysResult {
    ints(nr::SCHED_GET_PRIORITY_MAX, &[policy])
}
pub fn sched_get_priority_min(policy: usize) -> SysResult {
    ints(nr::SCHED_GET_PRIORITY_MIN, &[policy])
}

/// fcntl commands whose third argument is an integer (F_DUPFD 0, F_GETFD 1, F_SETFD 2, F_GETFL 3, F_SETFL 4,
/// F_DUPFD_CLOEXEC 1030, F_ADD_SEALS 1033, F_GET_SEALS 1034). Any other command is refused locally (the lock
/// commands take pointers).
pub fn fcntl(fd: usize, cmd: usize, arg: usize) -> SysResult {
    match cmd {
        0..=4 | 1030 | 1033 | 1034 => ints(nr::FCNTL, &[fd, cmd, arg]),
        _ => Err(REFUSED),
    }
}

/// prctl options that take no pointer: PR_GET_DUMPABLE 3, PR_SET_DUMPABLE 4, PR_GET_NO_NEW_PRIVS 39, and an
/// option number Linux does not know (0x5a5a_5a5a, for EINVAL).
pub fn prctl_int(option: usize, arg2: usize) -> SysResult {
    match option {
        3 | 4 | 39 | 0x5a5a_5a5a => ints(nr::PRCTL, &[option, arg2, 0, 0, 0]),
        _ => Err(REFUSED),
    }
}

pub fn exit_group(code: i32) -> ! {
    let _ = ints(nr::EXIT_GROUP, &[code as usize]);
    // exit_group does not return; if a kernel ever let it, end through the libc path.
    std::process::exit(code)
}

// ---------------------------------------------------------------------------------------------------------
// Calls with buffers

pub fn read(fd: usize, buf: &mut [u8]) -> SysResult {
    // SAFETY: the kernel writes at most buf.len() bytes into buf, which we borrow mutably for the call.
    ret(unsafe { raw(nr::READ, [fd, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0]) })
}
pub fn write(fd: usize, buf: &[u8]) -> SysResult {
    // SAFETY: the kernel reads at most buf.len() bytes from buf.
    ret(unsafe { raw(nr::WRITE, [fd, buf.as_ptr() as usize, buf.len(), 0, 0, 0]) })
}
pub fn pread64(fd: usize, buf: &mut [u8], off: i64) -> SysResult {
    // SAFETY: as read.
    ret(unsafe { raw(nr::PREAD64, [fd, buf.as_mut_ptr() as usize, buf.len(), off as usize, 0, 0]) })
}
pub fn pwrite64(fd: usize, buf: &[u8], off: i64) -> SysResult {
    // SAFETY: as write.
    ret(unsafe { raw(nr::PWRITE64, [fd, buf.as_ptr() as usize, buf.len(), off as usize, 0, 0]) })
}

/// read() into an address that can never be usable memory (see [`harmless`]): for EFAULT checks.
pub fn read_to_bad(fd: usize, addr: usize, len: usize) -> SysResult {
    if !(addr < 0x10000 || addr >= 0x0000_8000_0000_0000) {
        return Err(REFUSED);
    }
    // SAFETY: addr is below mmap_min_addr or in the kernel half: no page of ours can be there.
    ret(unsafe { raw(nr::READ, [fd, addr, len, 0, 0, 0]) })
}
/// write() from an address that can never be usable memory.
pub fn write_from_bad(fd: usize, addr: usize, len: usize) -> SysResult {
    if !(addr < 0x10000 || addr >= 0x0000_8000_0000_0000) {
        return Err(REFUSED);
    }
    // SAFETY: the kernel only reads; the address holds nothing of ours.
    ret(unsafe { raw(nr::WRITE, [fd, addr, len, 0, 0, 0]) })
}

#[repr(C)]
struct IoVec {
    base: usize,
    len: usize,
}

pub fn writev(fd: usize, parts: &[&[u8]]) -> SysResult {
    let v: Vec<IoVec> = parts.iter().map(|p| IoVec { base: p.as_ptr() as usize, len: p.len() }).collect();
    // SAFETY: each iovec describes a borrowed slice; the kernel only reads them.
    ret(unsafe { raw(nr::WRITEV, [fd, v.as_ptr() as usize, v.len(), 0, 0, 0]) })
}
pub fn readv(fd: usize, parts: &mut [&mut [u8]]) -> SysResult {
    let v: Vec<IoVec> = parts.iter_mut().map(|p| IoVec { base: p.as_mut_ptr() as usize, len: p.len() }).collect();
    // SAFETY: each iovec describes a mutably borrowed slice; the kernel writes at most its length.
    ret(unsafe { raw(nr::READV, [fd, v.as_ptr() as usize, v.len(), 0, 0, 0]) })
}

pub fn open(path: &str, flags: usize, mode: usize) -> SysResult {
    let p = cpath(path);
    // SAFETY: p is a NUL-terminated string the kernel only reads.
    ret(unsafe { raw(nr::OPEN, [p.as_ptr() as usize, flags, mode, 0, 0, 0]) })
}
pub fn openat(dirfd: isize, path: &str, flags: usize, mode: usize) -> SysResult {
    let p = cpath(path);
    // SAFETY: as open.
    ret(unsafe { raw(nr::OPENAT, [dirfd as usize, p.as_ptr() as usize, flags, mode, 0, 0]) })
}
/// open() with the path at an address that is never usable memory (EFAULT check).
pub fn open_bad_path(addr: usize) -> SysResult {
    if !(addr < 0x10000 || addr >= 0x0000_8000_0000_0000) {
        return Err(REFUSED);
    }
    // SAFETY: the kernel only reads the path; nothing of ours is there.
    ret(unsafe { raw(nr::OPEN, [addr, 0, 0, 0, 0, 0]) })
}

fn path1(n: usize, path: &str, a1: usize, a2: usize) -> SysResult {
    let p = cpath(path);
    // SAFETY: the only pointer is p, a NUL-terminated string the kernel only reads (callers pass integer
    // a1/a2 only).
    ret(unsafe { raw(n, [p.as_ptr() as usize, a1, a2, 0, 0, 0]) })
}
fn path2(n: usize, a: &str, b: &str) -> SysResult {
    let (pa, pb) = (cpath(a), cpath(b));
    // SAFETY: two NUL-terminated strings, read only.
    ret(unsafe { raw(n, [pa.as_ptr() as usize, pb.as_ptr() as usize, 0, 0, 0, 0]) })
}
fn at1(n: usize, dirfd: isize, path: &str, a2: usize, a3: usize) -> SysResult {
    let p = cpath(path);
    // SAFETY: one NUL-terminated string, read only; a2/a3 are integers for these calls.
    ret(unsafe { raw(n, [dirfd as usize, p.as_ptr() as usize, a2, a3, 0, 0]) })
}

pub fn access(path: &str, mode: usize) -> SysResult {
    path1(nr::ACCESS, path, mode, 0)
}
pub fn faccessat(dirfd: isize, path: &str, mode: usize) -> SysResult {
    at1(nr::FACCESSAT, dirfd, path, mode, 0)
}
pub fn chdir(path: &str) -> SysResult {
    path1(nr::CHDIR, path, 0, 0)
}
pub fn mkdir(path: &str, mode: usize) -> SysResult {
    path1(nr::MKDIR, path, mode, 0)
}
pub fn mkdirat(dirfd: isize, path: &str, mode: usize) -> SysResult {
    at1(nr::MKDIRAT, dirfd, path, mode, 0)
}
pub fn rmdir(path: &str) -> SysResult {
    path1(nr::RMDIR, path, 0, 0)
}
pub fn unlink(path: &str) -> SysResult {
    path1(nr::UNLINK, path, 0, 0)
}
pub fn unlinkat(dirfd: isize, path: &str, flags: usize) -> SysResult {
    at1(nr::UNLINKAT, dirfd, path, flags, 0)
}
pub fn chmod(path: &str, mode: usize) -> SysResult {
    path1(nr::CHMOD, path, mode, 0)
}
pub fn truncate(path: &str, len: i64) -> SysResult {
    path1(nr::TRUNCATE, path, len as usize, 0)
}
pub fn rename(a: &str, b: &str) -> SysResult {
    path2(nr::RENAME, a, b)
}
pub fn link(a: &str, b: &str) -> SysResult {
    path2(nr::LINK, a, b)
}
pub fn symlink(target: &str, at: &str) -> SysResult {
    path2(nr::SYMLINK, target, at)
}
pub fn renameat(olddir: isize, a: &str, newdir: isize, b: &str) -> SysResult {
    renameat2(olddir, a, newdir, b, usize::MAX)
}
/// renameat2; `flags == usize::MAX` means plain renameat (syscall 264).
pub fn renameat2(olddir: isize, a: &str, newdir: isize, b: &str, flags: usize) -> SysResult {
    let (pa, pb) = (cpath(a), cpath(b));
    let (n, f) = if flags == usize::MAX { (nr::RENAMEAT, 0) } else { (nr::RENAMEAT2, flags) };
    // SAFETY: two NUL-terminated strings, read only.
    ret(unsafe { raw(n, [olddir as usize, pa.as_ptr() as usize, newdir as usize, pb.as_ptr() as usize, f, 0]) })
}
pub fn readlink(path: &str, buf: &mut [u8]) -> SysResult {
    let p = cpath(path);
    // SAFETY: the kernel reads p and writes at most buf.len() bytes into buf.
    ret(unsafe { raw(nr::READLINK, [p.as_ptr() as usize, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0]) })
}
pub fn getcwd(buf: &mut [u8]) -> SysResult {
    // SAFETY: the kernel writes at most buf.len() bytes into buf.
    ret(unsafe { raw(nr::GETCWD, [buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0, 0]) })
}
pub fn getdents64(fd: usize, buf: &mut [u8]) -> SysResult {
    // SAFETY: the kernel writes at most buf.len() bytes into buf.
    ret(unsafe { raw(nr::GETDENTS64, [fd, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0]) })
}

/// Names and d_type of the records in a getdents64 buffer (linux_dirent64: ino u64, off i64, reclen u16,
/// type u8, name). Stops at a malformed record.
pub fn parse_dirents(buf: &[u8]) -> Vec<(Vec<u8>, u8)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 19 <= buf.len() {
        let reclen = u16::from_le_bytes([buf[i + 16], buf[i + 17]]) as usize;
        if reclen < 19 || i + reclen > buf.len() {
            break;
        }
        let ty = buf[i + 18];
        let name = &buf[i + 19..i + reclen];
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        out.push((name[..end].to_vec(), ty));
        i += reclen;
    }
    out
}

/// The fields of `struct stat` (x86-64 layout, 144 bytes) the tests look at.
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
    pub mtime: (i64, i64),
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[o..o + 4]);
    u32::from_le_bytes(a)
}

impl Stat {
    fn from(b: &[u8; 144]) -> Stat {
        Stat {
            dev: u64_at(b, 0),
            ino: u64_at(b, 8),
            nlink: u64_at(b, 16),
            mode: u32_at(b, 24),
            uid: u32_at(b, 28),
            gid: u32_at(b, 32),
            rdev: u64_at(b, 40),
            size: u64_at(b, 48) as i64,
            blksize: u64_at(b, 56) as i64,
            blocks: u64_at(b, 64) as i64,
            mtime: (u64_at(b, 88) as i64, u64_at(b, 96) as i64),
        }
    }
    pub fn file_type(&self) -> &'static str {
        match self.mode & 0o170000 {
            0o140000 => "socket",
            0o120000 => "symlink",
            0o100000 => "regular",
            0o060000 => "block",
            0o040000 => "directory",
            0o020000 => "char",
            0o010000 => "fifo",
            _ => "unknown",
        }
    }
}

pub fn fstat(fd: usize) -> SysResult<Stat> {
    let mut b = [0u8; 144];
    // SAFETY: the kernel writes one struct stat (144 bytes) into b.
    ret(unsafe { raw(nr::FSTAT, [fd, b.as_mut_ptr() as usize, 0, 0, 0, 0]) }).map(|_| Stat::from(&b))
}
pub fn stat(path: &str) -> SysResult<Stat> {
    let p = cpath(path);
    let mut b = [0u8; 144];
    // SAFETY: the kernel reads p and writes one struct stat into b.
    ret(unsafe { raw(nr::STAT, [p.as_ptr() as usize, b.as_mut_ptr() as usize, 0, 0, 0, 0]) }).map(|_| Stat::from(&b))
}
pub fn lstat(path: &str) -> SysResult<Stat> {
    let p = cpath(path);
    let mut b = [0u8; 144];
    // SAFETY: as stat.
    ret(unsafe { raw(nr::LSTAT, [p.as_ptr() as usize, b.as_mut_ptr() as usize, 0, 0, 0, 0]) }).map(|_| Stat::from(&b))
}
pub fn newfstatat(dirfd: isize, path: &str, flags: usize) -> SysResult<Stat> {
    let p = cpath(path);
    let mut b = [0u8; 144];
    // SAFETY: as stat.
    ret(unsafe { raw(nr::NEWFSTATAT, [dirfd as usize, p.as_ptr() as usize, b.as_mut_ptr() as usize, flags, 0, 0]) })
        .map(|_| Stat::from(&b))
}
/// fstat into an address that is never usable memory (EFAULT check).
pub fn fstat_bad(fd: usize, addr: usize) -> SysResult {
    if !(addr < 0x10000 || addr >= 0x0000_8000_0000_0000) {
        return Err(REFUSED);
    }
    // SAFETY: nothing of ours lives at addr.
    ret(unsafe { raw(nr::FSTAT, [fd, addr, 0, 0, 0, 0]) })
}

/// `struct statfs` fields (x86-64: f_type, f_bsize, ... 120 bytes).
pub fn statfs(path: &str) -> SysResult<(u64, u64)> {
    let p = cpath(path);
    let mut b = [0u8; 120];
    // SAFETY: the kernel reads p and writes one struct statfs (120 bytes) into b.
    ret(unsafe { raw(nr::STATFS, [p.as_ptr() as usize, b.as_mut_ptr() as usize, 0, 0, 0, 0]) })
        .map(|_| (u64_at(&b, 0), u64_at(&b, 8)))
}
pub fn fstatfs(fd: usize) -> SysResult<(u64, u64)> {
    let mut b = [0u8; 120];
    // SAFETY: the kernel writes one struct statfs into b.
    ret(unsafe { raw(nr::FSTATFS, [fd, b.as_mut_ptr() as usize, 0, 0, 0, 0]) }).map(|_| (u64_at(&b, 0), u64_at(&b, 8)))
}

/// utimensat(dirfd, path or NULL, times or NULL, flags).
pub fn utimensat(dirfd: isize, path: Option<&str>, times: Option<[(i64, i64); 2]>, flags: usize) -> SysResult {
    let p = path.map(cpath);
    let t: Option<[i64; 4]> = times.map(|t| [t[0].0, t[0].1, t[1].0, t[1].1]);
    let pp = p.as_ref().map(|c| c.as_ptr() as usize).unwrap_or(0);
    let tp = t.as_ref().map(|a| a.as_ptr() as usize).unwrap_or(0);
    // SAFETY: the kernel reads the path string and the two timespecs, both alive for the call.
    ret(unsafe { raw(nr::UTIMENSAT, [dirfd as usize, pp, tp, flags, 0, 0]) })
}

pub fn pipe() -> SysResult<(usize, usize)> {
    let mut fds = [0i32; 2];
    // SAFETY: the kernel writes two ints into fds.
    ret(unsafe { raw(nr::PIPE, [fds.as_mut_ptr() as usize, 0, 0, 0, 0, 0]) }).map(|_| (fds[0] as usize, fds[1] as usize))
}
pub fn pipe2(flags: usize) -> SysResult<(usize, usize)> {
    let mut fds = [0i32; 2];
    // SAFETY: as pipe.
    ret(unsafe { raw(nr::PIPE2, [fds.as_mut_ptr() as usize, flags, 0, 0, 0, 0]) }).map(|_| (fds[0] as usize, fds[1] as usize))
}

/// ioctl(fd, TIOCGWINSZ): rows, cols, xpixel, ypixel.
pub fn ioctl_winsize(fd: usize) -> SysResult<[u16; 4]> {
    let mut w = [0u16; 4];
    // SAFETY: TIOCGWINSZ writes one struct winsize (8 bytes) into w.
    ret(unsafe { raw(nr::IOCTL, [fd, 0x5413, w.as_mut_ptr() as usize, 0, 0, 0]) }).map(|_| w)
}
/// ioctl(fd, FIONREAD): bytes ready to read.
pub fn ioctl_fionread(fd: usize) -> SysResult<i32> {
    let mut n = 0i32;
    // SAFETY: FIONREAD writes one int into n.
    ret(unsafe { raw(nr::IOCTL, [fd, 0x541B, &mut n as *mut i32 as usize, 0, 0, 0]) }).map(|_| n)
}
/// ioctl(fd, TCGETS) into a 60-byte termios buffer.
pub fn ioctl_tcgets(fd: usize) -> SysResult<[u8; 60]> {
    let mut t = [0u8; 60];
    // SAFETY: TCGETS writes one struct termios (60 bytes on x86-64) into t.
    ret(unsafe { raw(nr::IOCTL, [fd, 0x5401, t.as_mut_ptr() as usize, 0, 0, 0]) }).map(|_| t)
}
/// ioctl with a request number no driver knows (0x5a5a), argument 0.
pub fn ioctl_unknown(fd: usize) -> SysResult {
    ints(nr::IOCTL, &[fd, 0x5a5a, 0])
}

// ---------------------------------------------------------------------------------------------------------
// Memory

pub const PROT_NONE: usize = 0;
pub const PROT_READ: usize = 1;
pub const PROT_WRITE: usize = 2;
pub const PROT_EXEC: usize = 4;
pub const MAP_SHARED: usize = 0x01;
pub const MAP_PRIVATE: usize = 0x02;
pub const MAP_FIXED: usize = 0x10;
pub const MAP_ANONYMOUS: usize = 0x20;
pub const MAP_NORESERVE: usize = 0x4000;
pub const MAP_FIXED_NOREPLACE: usize = 0x100000;
pub const MREMAP_MAYMOVE: usize = 1;

/// An anonymous private mapping this program owns; unmapped on drop. Only its own range is ever passed to
/// munmap/mprotect/mremap/madvise/msync/mincore.
pub struct Mapping {
    addr: usize,
    len: usize,
}

impl Mapping {
    /// mmap(NULL, len, prot, MAP_PRIVATE|MAP_ANONYMOUS|extra); MAP_FIXED/MAP_FIXED_NOREPLACE in `extra` are
    /// dropped (the kernel picks the place).
    pub fn anon(len: usize, prot: usize, extra: usize) -> SysResult<Mapping> {
        let flags = MAP_PRIVATE | MAP_ANONYMOUS | (extra & !(MAP_FIXED | MAP_FIXED_NOREPLACE | MAP_SHARED));
        // SAFETY: no address hint and no MAP_FIXED: the kernel places a new mapping where nothing is mapped.
        let a = ret(unsafe { raw(nr::MMAP, [0, len, prot, flags, usize::MAX, 0]) })?;
        Ok(Mapping { addr: a, len })
    }
    pub fn addr(&self) -> usize {
        self.addr
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    fn inside(&self, off: usize, len: usize) -> bool {
        off.checked_add(len).is_some_and(|e| e <= self.len)
    }
    /// Reads one byte (None outside the range). A page without PROT_READ ends the program with SIGSEGV.
    pub fn get(&self, off: usize) -> Option<u8> {
        if off >= self.len {
            return None;
        }
        // SAFETY: inside our own mapping; volatile because the kernel may replace the page (madvise, mremap).
        Some(unsafe { core::ptr::read_volatile((self.addr + off) as *const u8) })
    }
    /// Writes one byte (false outside the range). A page without PROT_WRITE ends the program with SIGSEGV.
    pub fn set(&self, off: usize, v: u8) -> bool {
        if off >= self.len {
            return false;
        }
        // SAFETY: inside our own mapping, which no Rust reference points into.
        unsafe { core::ptr::write_volatile((self.addr + off) as *mut u8, v) };
        true
    }
    pub fn protect(&self, off: usize, len: usize, prot: usize) -> SysResult {
        if !self.inside(off, len) {
            return Err(REFUSED);
        }
        // SAFETY: the range lies inside our own mapping, which Rust code only reaches through get/set.
        ret(unsafe { raw(nr::MPROTECT, [self.addr + off, len, prot, 0, 0, 0]) })
    }
    pub fn advise(&self, off: usize, len: usize, advice: usize) -> SysResult {
        if !self.inside(off, len) {
            return Err(REFUSED);
        }
        // SAFETY: inside our own mapping (MADV_DONTNEED zeroes it, which get() reads volatile).
        ret(unsafe { raw(nr::MADVISE, [self.addr + off, len, advice, 0, 0, 0]) })
    }
    pub fn mincore(&self, off: usize, len: usize) -> SysResult<Vec<u8>> {
        if !self.inside(off, len) {
            return Err(REFUSED);
        }
        let mut v = vec![0u8; len.div_ceil(4096)];
        // SAFETY: the kernel writes one byte per page of the range into v (sized for it).
        ret(unsafe { raw(nr::MINCORE, [self.addr + off, len, v.as_mut_ptr() as usize, 0, 0, 0]) }).map(|_| v)
    }
    pub fn sync(&self, off: usize, len: usize, flags: usize) -> SysResult {
        if !self.inside(off, len) {
            return Err(REFUSED);
        }
        // SAFETY: msync only writes back / checks the range; inside our mapping.
        ret(unsafe { raw(nr::MSYNC, [self.addr + off, len, flags, 0, 0, 0]) })
    }
    /// mremap of the whole mapping to `new_len` (with MREMAP_MAYMOVE when `may_move`); the mapping follows.
    pub fn remap(&mut self, new_len: usize, may_move: bool) -> SysResult {
        let flags = if may_move { MREMAP_MAYMOVE } else { 0 };
        // SAFETY: our own whole mapping; without MREMAP_FIXED the kernel grows in place or moves it to free space.
        let a = ret(unsafe { raw(nr::MREMAP, [self.addr, self.len, new_len, flags, 0, 0]) })?;
        self.addr = a;
        self.len = new_len;
        Ok(a)
    }
    /// munmap of a part (the rest stays ours; drop unmaps the whole range again, which is allowed on holes).
    pub fn unmap_part(&self, off: usize, len: usize) -> SysResult {
        if !self.inside(off, len) {
            return Err(REFUSED);
        }
        // SAFETY: inside our own mapping; get/set on the hole would fault, never touch foreign memory.
        ret(unsafe { raw(nr::MUNMAP, [self.addr + off, len, 0, 0, 0, 0]) })
    }
    /// mmap(MAP_FIXED) of a fresh anonymous range over a part of this mapping.
    pub fn map_fixed_part(&self, off: usize, len: usize, prot: usize, noreplace: bool) -> SysResult {
        if !self.inside(off, len) {
            return Err(REFUSED);
        }
        let fixed = if noreplace { MAP_FIXED_NOREPLACE } else { MAP_FIXED };
        // SAFETY: the target range lies inside our own mapping, so only our pages are replaced.
        ret(unsafe { raw(nr::MMAP, [self.addr + off, len, prot, MAP_PRIVATE | MAP_ANONYMOUS | fixed, usize::MAX, 0]) })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: our own range (after remap: the new one).
        let _ = unsafe { raw(nr::MUNMAP, [self.addr, self.len, 0, 0, 0, 0]) };
    }
}

/// mmap with raw arguments for refusal checks. Refused locally when it could replace a usable page
/// (MAP_FIXED/MAP_FIXED_NOREPLACE at an aligned address that is not [`harmless`]). A mapping the kernel makes
/// is unmapped at once; the address is returned.
pub fn mmap_probe(addr: usize, len: usize, prot: usize, flags: usize, fd: usize, off: usize) -> SysResult {
    if flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0 && !harmless(addr, len) {
        return Err(REFUSED);
    }
    // SAFETY: without MAP_FIXED the kernel only uses free space; with it the range is harmless (above).
    let a = ret(unsafe { raw(nr::MMAP, [addr, len, prot, flags, fd, off]) })?;
    // SAFETY: the mapping just made is ours alone.
    let _ = unsafe { raw(nr::MUNMAP, [a, len, 0, 0, 0, 0]) };
    Ok(a)
}
/// munmap with raw arguments; only when [`harmless`].
pub fn munmap_probe(addr: usize, len: usize) -> SysResult {
    if !harmless(addr, len) {
        return Err(REFUSED);
    }
    // SAFETY: the range cannot hold a usable page (harmless).
    ret(unsafe { raw(nr::MUNMAP, [addr, len, 0, 0, 0, 0]) })
}
/// mprotect with raw arguments; only when [`harmless`].
pub fn mprotect_probe(addr: usize, len: usize, prot: usize) -> SysResult {
    if !harmless(addr, len) {
        return Err(REFUSED);
    }
    // SAFETY: as munmap_probe.
    ret(unsafe { raw(nr::MPROTECT, [addr, len, prot, 0, 0, 0]) })
}
/// madvise with raw arguments; only when [`harmless`].
pub fn madvise_probe(addr: usize, len: usize, advice: usize) -> SysResult {
    if !harmless(addr, len) {
        return Err(REFUSED);
    }
    // SAFETY: as munmap_probe.
    ret(unsafe { raw(nr::MADVISE, [addr, len, advice, 0, 0, 0]) })
}

/// The current program break (brk(0)).
pub fn brk_query() -> usize {
    // SAFETY: brk(0) only reports the break.
    unsafe { raw(nr::BRK, [0; 6]) as usize }
}

/// What brk does when the break is raised by `extra` bytes, the new memory touched, and the break put back:
/// (before, after raising, written byte read back, after restoring). Nothing allocates in between, so the
/// heap's own use of the break cannot interleave.
pub fn brk_grow_restore(extra: usize) -> (usize, usize, Option<u8>, usize) {
    let before = brk_query();
    // SAFETY: raising the break only adds memory above the old break, which nothing uses yet.
    let after = unsafe { raw(nr::BRK, [before + extra, 0, 0, 0, 0, 0]) as usize };
    let mut seen = None;
    if after >= before + extra && extra > 0 {
        let p = (before + extra - 1) as *mut u8;
        // SAFETY: the byte lies in the range the kernel just added to our break.
        unsafe {
            core::ptr::write_volatile(p, 0x5a);
            seen = Some(core::ptr::read_volatile(p));
        }
    }
    // SAFETY: lowering the break back to `before` releases only the range added above, which nothing else got.
    let restored = unsafe { raw(nr::BRK, [before, 0, 0, 0, 0, 0]) as usize };
    (before, after, seen, restored)
}

// ---------------------------------------------------------------------------------------------------------
// Time

pub const CLOCK_REALTIME: usize = 0;
pub const CLOCK_MONOTONIC: usize = 1;
pub const CLOCK_PROCESS_CPUTIME_ID: usize = 2;
pub const CLOCK_THREAD_CPUTIME_ID: usize = 3;
pub const CLOCK_MONOTONIC_RAW: usize = 4;
pub const CLOCK_REALTIME_COARSE: usize = 5;
pub const CLOCK_MONOTONIC_COARSE: usize = 6;
pub const CLOCK_BOOTTIME: usize = 7;

pub fn clock_gettime(clk: usize) -> SysResult<(i64, i64)> {
    let mut t = [0i64; 2];
    // SAFETY: the kernel writes one struct timespec (16 bytes) into t.
    ret(unsafe { raw(nr::CLOCK_GETTIME, [clk, t.as_mut_ptr() as usize, 0, 0, 0, 0]) }).map(|_| (t[0], t[1]))
}
pub fn clock_getres(clk: usize) -> SysResult<(i64, i64)> {
    let mut t = [0i64; 2];
    // SAFETY: as clock_gettime.
    ret(unsafe { raw(nr::CLOCK_GETRES, [clk, t.as_mut_ptr() as usize, 0, 0, 0, 0]) }).map(|_| (t[0], t[1]))
}
pub fn clock_gettime_bad(clk: usize, addr: usize) -> SysResult {
    if !(addr < 0x10000 || addr >= 0x0000_8000_0000_0000) {
        return Err(REFUSED);
    }
    // SAFETY: nothing of ours lives at addr.
    ret(unsafe { raw(nr::CLOCK_GETTIME, [clk, addr, 0, 0, 0, 0]) })
}
pub fn nanosleep(sec: i64, nsec: i64) -> SysResult {
    let req = [sec, nsec];
    let mut rem = [0i64; 2];
    // SAFETY: the kernel reads req and may write rem (both 16-byte locals).
    ret(unsafe { raw(nr::NANOSLEEP, [req.as_ptr() as usize, rem.as_mut_ptr() as usize, 0, 0, 0, 0]) })
}
pub fn clock_nanosleep(clk: usize, flags: usize, sec: i64, nsec: i64) -> SysResult {
    let req = [sec, nsec];
    let mut rem = [0i64; 2];
    // SAFETY: as nanosleep.
    ret(unsafe { raw(nr::CLOCK_NANOSLEEP, [clk, flags, req.as_ptr() as usize, rem.as_mut_ptr() as usize, 0, 0]) })
}
pub fn gettimeofday() -> SysResult<(i64, i64)> {
    let mut t = [0i64; 2];
    // SAFETY: the kernel writes one struct timeval into t; the timezone pointer is NULL.
    ret(unsafe { raw(nr::GETTIMEOFDAY, [t.as_mut_ptr() as usize, 0, 0, 0, 0, 0]) }).map(|_| (t[0], t[1]))
}

/// Monotonic nanoseconds (clock_gettime syscall, never the vDSO), 0 on error.
pub fn mono_ns() -> u64 {
    clock_gettime(CLOCK_MONOTONIC).map(|(s, n)| (s as u64).saturating_mul(1_000_000_000).saturating_add(n as u64)).unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------------------
// Info

pub struct Utsname {
    pub sysname: String,
    pub nodename: String,
    pub release: String,
    pub version: String,
    pub machine: String,
    pub domainname: String,
}

pub fn uname() -> SysResult<Utsname> {
    let mut b = [0u8; 390];
    // SAFETY: the kernel writes one struct utsname (6 x 65 bytes) into b.
    ret(unsafe { raw(nr::UNAME, [b.as_mut_ptr() as usize, 0, 0, 0, 0, 0]) })?;
    let f = |i: usize| {
        let s = &b[i * 65..(i + 1) * 65];
        let e = s.iter().position(|&c| c == 0).unwrap_or(65);
        String::from_utf8_lossy(&s[..e]).into_owned()
    };
    Ok(Utsname { sysname: f(0), nodename: f(1), release: f(2), version: f(3), machine: f(4), domainname: f(5) })
}

pub struct SysInfo {
    pub uptime: i64,
    pub totalram: u64,
    pub freeram: u64,
    pub procs: u16,
    pub mem_unit: u32,
    /// 1, 5 and 15 minute load averages, scaled by 65536 (SI_LOAD_SHIFT).
    pub loads: [u64; 3],
}

pub fn sysinfo() -> SysResult<SysInfo> {
    let mut b = [0u8; 112];
    // SAFETY: the kernel writes one struct sysinfo (112 bytes on x86-64) into b.
    ret(unsafe { raw(nr::SYSINFO, [b.as_mut_ptr() as usize, 0, 0, 0, 0, 0]) })?;
    Ok(SysInfo {
        uptime: u64_at(&b, 0) as i64,
        totalram: u64_at(&b, 32),
        freeram: u64_at(&b, 40),
        procs: u16::from_le_bytes([b[80], b[81]]),
        mem_unit: u32_at(&b, 104),
        loads: [u64_at(&b, 8), u64_at(&b, 16), u64_at(&b, 24)],
    })
}

/// prlimit64(pid, resource, new, old): returns the old (cur, max).
pub fn prlimit64(pid: usize, res: usize, new: Option<(u64, u64)>) -> SysResult<(u64, u64)> {
    let n = new.map(|(a, b)| [a, b]);
    let mut old = [0u64; 2];
    let np = n.as_ref().map(|a| a.as_ptr() as usize).unwrap_or(0);
    // SAFETY: the kernel reads n (16 bytes) and writes old (16 bytes).
    ret(unsafe { raw(nr::PRLIMIT64, [pid, res, np, old.as_mut_ptr() as usize, 0, 0]) }).map(|_| (old[0], old[1]))
}

pub fn prctl_set_name(name: &str) -> SysResult {
    let p = cpath(name);
    // SAFETY: PR_SET_NAME (15) reads up to 16 bytes of the string p.
    ret(unsafe { raw(nr::PRCTL, [15, p.as_ptr() as usize, 0, 0, 0, 0]) })
}
pub fn prctl_get_name() -> SysResult<Vec<u8>> {
    let mut b = [0u8; 16];
    // SAFETY: PR_GET_NAME (16) writes 16 bytes into b.
    ret(unsafe { raw(nr::PRCTL, [16, b.as_mut_ptr() as usize, 0, 0, 0, 0]) })?;
    let e = b.iter().position(|&c| c == 0).unwrap_or(16);
    Ok(b[..e].to_vec())
}

pub fn getrandom(buf: &mut [u8], flags: usize) -> SysResult {
    // SAFETY: the kernel writes at most buf.len() bytes into buf.
    ret(unsafe { raw(nr::GETRANDOM, [buf.as_mut_ptr() as usize, buf.len(), flags, 0, 0, 0]) })
}

/// arch_prctl GET codes (ARCH_GET_FS 0x1003, ARCH_GET_GS 0x1004) into a local; other codes are passed with a
/// NULL address and must be ones that only read or are unknown (0x1001/0x1002 SET are refused locally).
pub fn arch_prctl_get(code: usize) -> SysResult<u64> {
    match code {
        0x1001 | 0x1002 => Err(REFUSED),
        0x1003 | 0x1004 => {
            let mut v = 0u64;
            // SAFETY: the GET codes write one u64 into v.
            ret(unsafe { raw(nr::ARCH_PRCTL, [code, &mut v as *mut u64 as usize, 0, 0, 0, 0]) }).map(|_| v)
        }
        _ => ints(nr::ARCH_PRCTL, &[code, 0]).map(|v| v as u64),
    }
}

/// set_tid_address(ptr): the kernel will write 0 there (and futex-wake it) when this thread ends; a static
/// lives that long. Returns the caller's tid.
pub fn set_tid_address(slot: &'static AtomicU32) -> SysResult {
    // SAFETY: slot is a 'static u32; the kernel only stores the address now and writes 0 at thread exit.
    ret(unsafe { raw(nr::SET_TID_ADDRESS, [slot.as_ptr() as usize, 0, 0, 0, 0, 0]) })
}

// ---------------------------------------------------------------------------------------------------------
// Signals

pub const SIG_BLOCK: usize = 0;
pub const SIG_UNBLOCK: usize = 1;
pub const SIG_SETMASK: usize = 2;

/// rt_sigprocmask(how, set, old, sigsetsize). `old_seed` is what the old-mask buffer holds before the call, so a
/// kernel that does not write it is visible. Returns the old buffer's content afterwards when asked.
pub fn rt_sigprocmask(how: usize, set: Option<u64>, want_old: bool, old_seed: u64, sigsetsize: usize) -> SysResult<Option<u64>> {
    // Buffers of 128 bytes: even a kernel that wrongly honoured a larger sigsetsize stays inside them.
    if sigsetsize > 128 {
        return Err(REFUSED);
    }
    let mut s = [0u64; 16];
    s[0] = set.unwrap_or(0);
    let mut old = [old_seed; 16];
    let sp = if set.is_some() { s.as_ptr() as usize } else { 0 };
    let op = if want_old { old.as_mut_ptr() as usize } else { 0 };
    // SAFETY: the kernel reads at most sigsetsize (<= 128) bytes from s and writes at most that many into old.
    ret(unsafe { raw(nr::RT_SIGPROCMASK, [how, sp, op, sigsetsize, 0, 0]) }).map(|_| want_old.then_some(old[0]))
}

/// rt_sigaction(sig, NULL, &old): (handler, flags, restorer, mask) of the current action.
pub fn rt_sigaction_query(sig: usize) -> SysResult<[u64; 4]> {
    let mut old = [0u64; 4];
    // SAFETY: the kernel writes one struct kernel_sigaction (32 bytes) into old.
    ret(unsafe { raw(nr::RT_SIGACTION, [sig, 0, old.as_mut_ptr() as usize, 8, 0, 0]) }).map(|_| old)
}
/// rt_sigaction(sig, {SIG_IGN or SIG_DFL}, NULL). No handler code is installed, so nothing can jump into us.
pub fn rt_sigaction_simple(sig: usize, ignore: bool) -> SysResult {
    let act: [u64; 4] = [if ignore { 1 } else { 0 }, 0, 0, 0];
    // SAFETY: the kernel reads 32 bytes from act; SIG_IGN/SIG_DFL are not code addresses.
    ret(unsafe { raw(nr::RT_SIGACTION, [sig, act.as_ptr() as usize, 0, 8, 0, 0]) })
}
/// rt_sigaction with a sigsetsize other than 8 (Linux: EINVAL before any copy).
pub fn rt_sigaction_bad_size(sig: usize, size: usize) -> SysResult {
    if size > 128 {
        return Err(REFUSED);
    }
    let mut old = [0u64; 32];
    // SAFETY: a kernel writes at most 24 + size bytes (<= 152 < 256) into old; Linux writes none (wrong size).
    ret(unsafe { raw(nr::RT_SIGACTION, [sig, 0, old.as_mut_ptr() as usize, size, 0, 0]) })
}

/// sigaltstack(NULL, &old): (sp, flags, size).
pub fn sigaltstack_query() -> SysResult<(u64, u32, u64)> {
    let mut b = [0u8; 24];
    // SAFETY: the kernel writes one stack_t (24 bytes) into b.
    ret(unsafe { raw(nr::SIGALTSTACK, [0, b.as_mut_ptr() as usize, 0, 0, 0, 0]) })?;
    Ok((u64_at(&b, 0), u32_at(&b, 8), u64_at(&b, 16)))
}
/// sigaltstack with a new stack_t {sp, flags, size}; the previous alternate stack is put back right after a
/// success, so a signal never runs on `sp` (which may be a test value).
pub fn sigaltstack_try(sp: u64, flags: u32, size: u64) -> SysResult {
    let (osp, oflags, osize) = sigaltstack_query()?;
    let mut new = [0u8; 24];
    new[0..8].copy_from_slice(&sp.to_le_bytes());
    new[8..12].copy_from_slice(&flags.to_le_bytes());
    new[16..24].copy_from_slice(&size.to_le_bytes());
    // SAFETY: the kernel reads 24 bytes from new; no signal handler of ours uses an alternate stack before the
    // old one is restored below (std's overflow handler only runs on a fault).
    let r = ret(unsafe { raw(nr::SIGALTSTACK, [new.as_ptr() as usize, 0, 0, 0, 0, 0]) });
    if r.is_ok() {
        let mut old = [0u8; 24];
        old[0..8].copy_from_slice(&osp.to_le_bytes());
        old[8..12].copy_from_slice(&(oflags & 2).to_le_bytes()); // SS_DISABLE kept, SS_ONSTACK is not settable
        old[16..24].copy_from_slice(&osize.to_le_bytes());
        // SAFETY: as above, restoring the stack std installed.
        let _ = unsafe { raw(nr::SIGALTSTACK, [old.as_ptr() as usize, 0, 0, 0, 0, 0]) };
    }
    r
}

// ---------------------------------------------------------------------------------------------------------
// Poll, epoll, eventfd

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}
pub const POLLIN: i16 = 1;
pub const POLLPRI: i16 = 2;
pub const POLLOUT: i16 = 4;
pub const POLLERR: i16 = 8;
pub const POLLHUP: i16 = 16;
pub const POLLNVAL: i16 = 32;

pub fn poll(fds: &mut [PollFd], timeout_ms: i32) -> SysResult {
    // SAFETY: the kernel reads and writes fds.len() pollfd records in fds.
    ret(unsafe { raw(nr::POLL, [fds.as_mut_ptr() as usize, fds.len(), timeout_ms as isize as usize, 0, 0, 0]) })
}
pub fn ppoll(fds: &mut [PollFd], timeout: Option<(i64, i64)>) -> SysResult {
    let t = timeout.map(|(s, n)| [s, n]);
    let tp = t.as_ref().map(|a| a.as_ptr() as usize).unwrap_or(0);
    // SAFETY: as poll, plus a timespec read (and possibly updated) by the kernel; no signal mask (NULL).
    ret(unsafe { raw(nr::PPOLL, [fds.as_mut_ptr() as usize, fds.len(), tp, 0, 8, 0]) })
}

pub const EPOLL_CTL_ADD: usize = 1;
pub const EPOLL_CTL_DEL: usize = 2;
pub const EPOLL_CTL_MOD: usize = 3;

pub fn epoll_ctl(epfd: usize, op: usize, fd: usize, events: u32, data: u64) -> SysResult {
    let mut ev = [0u8; 12];
    ev[0..4].copy_from_slice(&events.to_le_bytes());
    ev[4..12].copy_from_slice(&data.to_le_bytes());
    // SAFETY: the kernel reads one packed struct epoll_event (12 bytes) from ev.
    ret(unsafe { raw(nr::EPOLL_CTL, [epfd, op, fd, ev.as_ptr() as usize, 0, 0]) })
}
/// epoll_wait (or epoll_pwait with a NULL mask when `pwait`) for up to `max` events: (events, data) pairs.
pub fn epoll_wait(epfd: usize, max: usize, timeout_ms: i32, pwait: bool) -> SysResult<Vec<(u32, u64)>> {
    let mut b = vec![0u8; max.max(1) * 12];
    let n = if pwait { nr::EPOLL_PWAIT } else { nr::EPOLL_WAIT };
    // SAFETY: the kernel writes at most `max` 12-byte records into b (sized for them).
    let k = ret(unsafe { raw(n, [epfd, b.as_mut_ptr() as usize, max, timeout_ms as isize as usize, 0, 8]) })?;
    Ok((0..k.min(max)).map(|i| (u32_at(&b, i * 12), u64_at(&b, i * 12 + 4))).collect())
}

// ---------------------------------------------------------------------------------------------------------
// Threads, futex, scheduling, CPU

const FUTEX_WAIT: usize = 0;
const FUTEX_WAKE: usize = 1;
const FUTEX_PRIVATE: usize = 128;

pub fn futex_wait(word: &AtomicU32, expected: u32, timeout: Option<(i64, i64)>, private: bool) -> SysResult {
    let t = timeout.map(|(s, n)| [s, n]);
    let tp = t.as_ref().map(|a| a.as_ptr() as usize).unwrap_or(0);
    let op = FUTEX_WAIT | if private { FUTEX_PRIVATE } else { 0 };
    // SAFETY: the kernel reads the u32 behind the atomic (shared memory is what futexes are for) and the timespec.
    ret(unsafe { raw(nr::FUTEX, [word.as_ptr() as usize, op, expected as usize, tp, 0, 0]) })
}
pub fn futex_wake(word: &AtomicU32, n: u32, private: bool) -> SysResult {
    let op = FUTEX_WAKE | if private { FUTEX_PRIVATE } else { 0 };
    // SAFETY: FUTEX_WAKE only looks the address up; it reads/writes nothing.
    ret(unsafe { raw(nr::FUTEX, [word.as_ptr() as usize, op, n as usize, 0, 0, 0]) })
}
/// futex with an operation number Linux does not have (ENOSYS there).
pub fn futex_bad_op(word: &AtomicU32) -> SysResult {
    // SAFETY: an unknown op is refused before the kernel touches the word.
    ret(unsafe { raw(nr::FUTEX, [word.as_ptr() as usize, 99, 0, 0, 0, 0]) })
}

/// clone with a flag combination Linux refuses (EINVAL) and no stack. If a kernel wrongly makes a child, the
/// child (return 0, running on a copy) ends at once with exit_group(99).
pub fn clone_invalid(flags: usize) -> SysResult {
    // CLONE_SIGHAND without CLONE_VM, or CLONE_THREAD without CLONE_SIGHAND: refused by copy_process.
    const VM: usize = 0x100;
    const SIGHAND: usize = 0x800;
    const THREAD: usize = 0x10000;
    let invalid = (flags & SIGHAND != 0 && flags & VM == 0) || (flags & THREAD != 0 && flags & SIGHAND == 0);
    if !invalid || flags & VM != 0 {
        return Err(REFUSED);
    }
    // SAFETY: the flags are invalid and without CLONE_VM, so at worst a separate copy of us runs, which exits.
    let r = ret(unsafe { raw(nr::CLONE, [flags, 0, 0, 0, 0, 0]) });
    if r == Ok(0) {
        exit_group(99);
    }
    r
}

pub fn sched_getaffinity(pid: usize, bytes: usize) -> SysResult<Vec<u8>> {
    let mut b = vec![0u8; bytes];
    // SAFETY: the kernel writes at most `bytes` bytes of the mask into b.
    let n = ret(unsafe { raw(nr::SCHED_GETAFFINITY, [pid, bytes, b.as_mut_ptr() as usize, 0, 0, 0]) })?;
    b.truncate(n);
    Ok(b)
}
pub fn sched_getparam(pid: usize) -> SysResult<i32> {
    let mut p = 0i32;
    // SAFETY: the kernel writes one struct sched_param (an int) into p.
    ret(unsafe { raw(nr::SCHED_GETPARAM, [pid, &mut p as *mut i32 as usize, 0, 0, 0, 0]) }).map(|_| p)
}
/// getcpu(&cpu, &node, NULL).
pub fn getcpu() -> SysResult<(u32, u32)> {
    let (mut c, mut n) = (0u32, 0u32);
    // SAFETY: the kernel writes one u32 each into c and n.
    ret(unsafe { raw(nr::GETCPU, [&mut c as *mut u32 as usize, &mut n as *mut u32 as usize, 0, 0, 0, 0]) }).map(|_| (c, n))
}
/// The CPU number from IA32_TSC_AUX via rdtscp (Linux and V2 both store `node << 12 | cpu` there).
pub fn rdtscp_cpu() -> u32 {
    let mut aux = 0u32;
    // SAFETY: rdtscp only reads the TSC and TSC_AUX into registers (CPUID-guaranteed on x86-64-v3 machines).
    let _ = unsafe { core::arch::x86_64::__rdtscp(&mut aux) };
    aux & 0xfff
}

// ---------------------------------------------------------------------------------------------------------
// Processes

/// wait4(pid, &status, options, NULL): (pid, raw status). With WNOHANG and nothing ended: Ok((0, 0)).
pub fn wait4(pid: isize, options: usize) -> SysResult<(usize, i32)> {
    let mut st = 0i32;
    // SAFETY: the kernel writes one int into st; rusage is NULL.
    ret(unsafe { raw(nr::WAIT4, [pid as usize, &mut st as *mut i32 as usize, options, 0, 0, 0]) }).map(|p| (p, st))
}
pub const WNOHANG: usize = 1;

/// The wait status as text: `exit N` or `signal N` (or `stopped`/`status 0x..`).
pub fn status_text(st: i32) -> String {
    let sig = st & 0x7f;
    if sig == 0 {
        format!("exit {}", (st >> 8) & 0xff)
    } else if sig == 0x7f {
        "stopped".to_string()
    } else {
        format!("signal {sig}")
    }
}

/// mount(source, target, fstype, flags, data).
pub fn mount(source: &str, target: &str, fstype: &str, flags: usize, data: Option<&str>) -> SysResult {
    let (s, t, f) = (cpath(source), cpath(target), cpath(fstype));
    let d = data.map(cpath);
    let dp = d.as_ref().map(|c| c.as_ptr() as usize).unwrap_or(0);
    // SAFETY: four NUL-terminated strings (or NULL), read only.
    ret(unsafe { raw(nr::MOUNT, [s.as_ptr() as usize, t.as_ptr() as usize, f.as_ptr() as usize, flags, dp, 0]) })
}
pub fn sethostname(name: &str) -> SysResult {
    // SAFETY: the kernel reads name.len() bytes.
    ret(unsafe { raw(nr::SETHOSTNAME, [name.as_ptr() as usize, name.len(), 0, 0, 0, 0]) })
}

// ---------------------------------------------------------------------------------------------------------
// Sockets

pub const AF_UNIX: usize = 1;
pub const AF_INET: usize = 2;
pub const SOCK_STREAM: usize = 1;
pub const SOCK_DGRAM: usize = 2;
pub const SOCK_NONBLOCK: usize = 0o4000;
pub const SOCK_CLOEXEC: usize = 0o2000000;
pub const SOL_SOCKET: usize = 1;
pub const SO_REUSEADDR: usize = 2;
pub const SO_TYPE: usize = 3;
pub const SO_ERROR: usize = 4;
pub const SO_RCVBUF: usize = 8;
pub const IPPROTO_TCP: usize = 6;
pub const TCP_NODELAY: usize = 1;

fn sockaddr_in(ip: [u8; 4], port: u16) -> [u8; 16] {
    let mut a = [0u8; 16];
    a[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    a[2..4].copy_from_slice(&port.to_be_bytes());
    a[4..8].copy_from_slice(&ip);
    a
}
fn from_sockaddr_in(a: &[u8; 16]) -> ([u8; 4], u16) {
    ([a[4], a[5], a[6], a[7]], u16::from_be_bytes([a[2], a[3]]))
}

pub fn bind_inet(fd: usize, ip: [u8; 4], port: u16) -> SysResult {
    let a = sockaddr_in(ip, port);
    // SAFETY: the kernel reads 16 bytes of sockaddr_in from a.
    ret(unsafe { raw(nr::BIND, [fd, a.as_ptr() as usize, 16, 0, 0, 0]) })
}
pub fn connect_inet(fd: usize, ip: [u8; 4], port: u16) -> SysResult {
    let a = sockaddr_in(ip, port);
    // SAFETY: as bind_inet.
    ret(unsafe { raw(nr::CONNECT, [fd, a.as_ptr() as usize, 16, 0, 0, 0]) })
}
pub fn accept4(fd: usize, flags: usize) -> SysResult<(usize, [u8; 4], u16)> {
    let mut a = [0u8; 16];
    let mut len: u32 = 16;
    // SAFETY: the kernel writes at most `len` (16) bytes of address into a and the real length into len.
    let n = ret(unsafe { raw(nr::ACCEPT4, [fd, a.as_mut_ptr() as usize, &mut len as *mut u32 as usize, flags, 0, 0]) })?;
    let (ip, port) = from_sockaddr_in(&a);
    Ok((n, ip, port))
}
fn sockname(n: usize, fd: usize) -> SysResult<([u8; 4], u16)> {
    let mut a = [0u8; 16];
    let mut len: u32 = 16;
    // SAFETY: as accept4's address.
    ret(unsafe { raw(n, [fd, a.as_mut_ptr() as usize, &mut len as *mut u32 as usize, 0, 0, 0]) })?;
    Ok(from_sockaddr_in(&a))
}
pub fn getsockname_inet(fd: usize) -> SysResult<([u8; 4], u16)> {
    sockname(nr::GETSOCKNAME, fd)
}
pub fn getpeername_inet(fd: usize) -> SysResult<([u8; 4], u16)> {
    sockname(nr::GETPEERNAME, fd)
}
pub fn sendto(fd: usize, buf: &[u8], flags: usize, to: Option<([u8; 4], u16)>) -> SysResult {
    let a = to.map(|(ip, p)| sockaddr_in(ip, p));
    let (ap, al) = a.as_ref().map(|a| (a.as_ptr() as usize, 16)).unwrap_or((0, 0));
    // SAFETY: the kernel reads buf and the optional 16-byte address.
    ret(unsafe { raw(nr::SENDTO, [fd, buf.as_ptr() as usize, buf.len(), flags, ap, al]) })
}
pub fn recvfrom(fd: usize, buf: &mut [u8], flags: usize) -> SysResult<(usize, [u8; 4], u16)> {
    let mut a = [0u8; 16];
    let mut len: u32 = 16;
    // SAFETY: the kernel writes at most buf.len() bytes into buf and at most 16 bytes of address into a.
    let n = ret(unsafe {
        raw(nr::RECVFROM, [fd, buf.as_mut_ptr() as usize, buf.len(), flags, a.as_mut_ptr() as usize, &mut len as *mut u32 as usize])
    })?;
    let (ip, port) = from_sockaddr_in(&a);
    Ok((n, ip, port))
}
pub fn setsockopt_int(fd: usize, level: usize, opt: usize, val: i32) -> SysResult {
    // SAFETY: the kernel reads one int.
    ret(unsafe { raw(nr::SETSOCKOPT, [fd, level, opt, &val as *const i32 as usize, 4, 0]) })
}
pub fn getsockopt_int(fd: usize, level: usize, opt: usize) -> SysResult<i32> {
    let mut v = 0i32;
    let mut len: u32 = 4;
    // SAFETY: the kernel writes at most 4 bytes into v and the length into len.
    ret(unsafe { raw(nr::GETSOCKOPT, [fd, level, opt, &mut v as *mut i32 as usize, &mut len as *mut u32 as usize, 0]) }).map(|_| v)
}
pub fn socketpair(domain: usize, ty: usize) -> SysResult<(usize, usize)> {
    let mut fds = [0i32; 2];
    // SAFETY: the kernel writes two ints into fds.
    ret(unsafe { raw(nr::SOCKETPAIR, [domain, ty, 0, fds.as_mut_ptr() as usize, 0, 0]) }).map(|_| (fds[0] as usize, fds[1] as usize))
}

#[repr(C)]
struct MsgHdr {
    name: usize,
    namelen: u32,
    iov: usize,
    iovlen: usize,
    control: usize,
    controllen: usize,
    flags: i32,
}

/// sendmsg with one data buffer, no address, no control data.
pub fn sendmsg(fd: usize, buf: &[u8], flags: usize) -> SysResult {
    let iov = IoVec { base: buf.as_ptr() as usize, len: buf.len() };
    let m = MsgHdr { name: 0, namelen: 0, iov: &iov as *const IoVec as usize, iovlen: 1, control: 0, controllen: 0, flags: 0 };
    // SAFETY: the kernel reads the msghdr, the iovec and buf.
    ret(unsafe { raw(nr::SENDMSG, [fd, &m as *const MsgHdr as usize, flags, 0, 0, 0]) })
}
/// recvmsg into one buffer, no address, no control data.
pub fn recvmsg(fd: usize, buf: &mut [u8], flags: usize) -> SysResult {
    let iov = IoVec { base: buf.as_mut_ptr() as usize, len: buf.len() };
    let mut m = MsgHdr { name: 0, namelen: 0, iov: &iov as *const IoVec as usize, iovlen: 1, control: 0, controllen: 0, flags: 0 };
    // SAFETY: the kernel reads the msghdr and iovec, writes at most buf.len() bytes into buf and updates m.
    ret(unsafe { raw(nr::RECVMSG, [fd, &mut m as *mut MsgHdr as usize, flags, 0, 0, 0]) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harmless_ranges() {
        assert!(harmless(0x1001, 4096));
        assert!(harmless(0x1000, 0));
        assert!(harmless(0x1000, 0x1000));
        assert!(harmless(0x0000_7fff_ffff_f000, 4096));
        assert!(!harmless(0x40_0000, 4096));
        assert!(!harmless(0, 0x20000));
    }

    #[test]
    fn dirents_parse_and_stop_on_garbage() {
        let mut b = vec![0u8; 24];
        b[16] = 24;
        b[18] = 4;
        b[19] = b'a';
        assert_eq!(parse_dirents(&b), vec![(b"a".to_vec(), 4)]);
        b[16] = 200;
        assert!(parse_dirents(&b).is_empty());
    }

    #[test]
    fn status_texts() {
        assert_eq!(status_text(7 << 8), "exit 7");
        assert_eq!(status_text(11), "signal 11");
    }
}
