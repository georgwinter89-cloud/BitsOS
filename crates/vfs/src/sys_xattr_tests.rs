//! The answers `cargo xtask abi-diff` cannot compare, because the host's file systems have extended attributes.
use super::*;
use crate::abi::{O_CREAT, O_RDONLY, O_RDWR, O_WRONLY};
use crate::cases_test::{program_view, Mem, SUBJECT, USER};
use crate::sys;
use alloc::vec;

const OPENAT: usize = 257;
const AT: u64 = AT_FDCWD as u64;

fn open(s: &Sys<'_>, mem: &Mem, path: &[u8], flags: u64) -> u64 {
    let r = sys::call(s, OPENAT, [AT, mem.cstr(path), flags, 0o644, 0, 0]);
    assert!(r >= 0, "open: {r}");
    r as u64
}

#[test]
fn answers_for_an_inode_without_xattrs() {
    let (ns, fds) = program_view();
    let mem = Mem::new();
    let s = Sys { mem: &mem, fds: &fds, fs: &crate::fd::FsState::new(), ns: &ns, cred: USER, subject: SUBJECT };
    let f = mem.cstr(b"/tmp/x");
    let fd = open(&s, &mem, b"/tmp/x", O_CREAT | O_RDWR);
    let user = mem.cstr(b"user.a");
    let trusted = mem.cstr(b"trusted.a");
    let v = mem.alloc(b"value");

    assert_eq!(getxattr(&s, [f, user, v, 5, 0, 0]), -EOPNOTSUPP);
    assert_eq!(fgetxattr(&s, [fd, user, v, 5, 0, 0]), -EOPNOTSUPP);
    assert_eq!(lgetxattr(&s, [f, mem.cstr(b"security.a"), 0, 0, 0, 0]), -EOPNOTSUPP);
    assert_eq!(setxattr(&s, [f, user, v, 5, 0, 0]), -EOPNOTSUPP);
    assert_eq!(fsetxattr(&s, [fd, user, v, 5, 0, 0]), -EOPNOTSUPP);
    assert_eq!(removexattr(&s, [f, user, 0, 0, 0, 0]), -EOPNOTSUPP);
    assert_eq!(listxattr(&s, [f, 0, 0, 0, 0, 0]), 0);
    assert_eq!(flistxattr(&s, [fd, 0, 0, 0, 0, 0]), 0);

    // trusted.* without the capability; root gets to the end.
    assert_eq!(getxattr(&s, [f, trusted, 0, 0, 0, 0]), -ENODATA);
    assert_eq!(setxattr(&s, [f, trusted, 0, 0, 0, 0]), -EPERM);
    assert_eq!(removexattr(&s, [f, trusted, 0, 0, 0, 0]), -EPERM);
    let root = Sys { mem: &mem, fds: &fds, fs: &crate::fd::FsState::new(), ns: &ns, cred: Cred::ROOT, subject: SUBJECT };
    assert_eq!(getxattr(&root, [f, trusted, 0, 0, 0, 0]), -EOPNOTSUPP);

    // user.* only on regular files and directories.
    let ra = mem.alloc_zero(8);
    assert_eq!(sys::call(&s, 22, [ra, 0, 0, 0, 0, 0]), 0, "pipe");
    let rd = u32::from_le_bytes(mem.get(ra, 4).try_into().unwrap()) as u64;
    assert_eq!(fgetxattr(&s, [rd, user, 0, 0, 0, 0]), -ENODATA);
    assert_eq!(fsetxattr(&s, [rd, user, 0, 0, 0, 0]), -EPERM);

    // The file's own bits: not readable or not writable for this user.
    assert_eq!(sys::call(&s, 90, [f, 0o000, 0, 0, 0, 0]), 0, "chmod");
    assert_eq!(getxattr(&s, [f, user, 0, 0, 0, 0]), -EACCES);
    assert_eq!(setxattr(&s, [f, user, 0, 0, 0, 0]), -EACCES);

    // The errors before them, in CachyOS's order: what comes from the program first, then the path.
    let none = mem.cstr(b"/tmp/none");
    assert_eq!(setxattr(&s, [none, 0, 0, 0, 4, 0]), -EINVAL, "the flags");
    assert_eq!(setxattr(&s, [none, 0, 0, 0, 0, 0]), -EFAULT, "then the name");
    assert_eq!(setxattr(&s, [none, mem.cstr(b""), 0, 0, 0, 0]), -ERANGE, "an empty name");
    assert_eq!(setxattr(&s, [none, mem.cstr(&vec![b'a'; 256]), 0, 0, 0, 0]), -ERANGE, "a name of 256");
    assert_eq!(setxattr(&s, [none, user, v, 65537, 0, 0]), -E2BIG, "then the size");
    assert_eq!(setxattr(&s, [none, user, 0, 1, 0, 0]), -EFAULT, "then the value");
    assert_eq!(setxattr(&s, [none, user, 0, 0, 0, 0]), -ENOENT, "then the path");
    assert_eq!(getxattr(&s, [none, 0, 0, 0, 0, 0]), -EFAULT, "the name before the path");
    assert_eq!(removexattr(&s, [none, 0, 0, 0, 0, 0]), -EFAULT, "the name before the path");
    assert_eq!(fgetxattr(&s, [99, 0, 0, 0, 0, 0]), -EFAULT, "the name before the fd");
    assert_eq!(fgetxattr(&s, [99, user, 0, 0, 0, 0]), -EBADF);
    assert_eq!(flistxattr(&s, [99, 0, 0, 0, 0, 0]), -EBADF);
    assert_eq!(fremovexattr(&s, [99, user, 0, 0, 0, 0]), -EBADF);
}

#[test]
fn readahead_and_sync_file_range() {
    let (ns, fds) = program_view();
    let mem = Mem::new();
    let s = Sys { mem: &mem, fds: &fds, fs: &crate::fd::FsState::new(), ns: &ns, cred: USER, subject: SUBJECT };
    let fd = open(&s, &mem, b"/tmp/r", O_CREAT | O_RDWR);
    let wr = open(&s, &mem, b"/tmp/r", O_WRONLY);
    let dir = open(&s, &mem, b"/tmp", O_RDONLY);
    let ra = mem.alloc_zero(8);
    assert_eq!(sys::call(&s, 22, [ra, 0, 0, 0, 0, 0]), 0, "pipe");
    let rd = u32::from_le_bytes(mem.get(ra, 4).try_into().unwrap()) as u64;

    assert_eq!(readahead(&s, [fd, 0, 4096, 0, 0, 0]), 0);
    assert_eq!(readahead(&s, [fd, u64::MAX, 0, 0, 0, 0]), -EINVAL, "a negative offset");
    assert_eq!(readahead(&s, [99, 0, 0, 0, 0, 0]), -EBADF);
    assert_eq!(readahead(&s, [wr, 0, 0, 0, 0, 0]), -EBADF, "not open for reading");
    assert_eq!(readahead(&s, [rd, 0, 0, 0, 0, 0]), -EINVAL, "a pipe");
    assert_eq!(readahead(&s, [dir, 0, 0, 0, 0, 0]), -EINVAL, "a directory");
    assert_eq!(readahead(&s, [fd, 0, u64::MAX, 0, 0, 0]), -EINVAL, "a count negative as loff_t");

    assert_eq!(sync_file_range(&s, [fd, 0, 0, 0, 0, 0]), 0);
    assert_eq!(sync_file_range(&s, [fd, 0, 4096, 7, 0, 0]), 0);
    assert_eq!(sync_file_range(&s, [dir, 0, 0, 2, 0, 0]), 0, "a directory");
    assert_eq!(sync_file_range(&s, [99, 0, 0, 0, 0, 0]), -EBADF);
    assert_eq!(sync_file_range(&s, [fd, 0, 0, 8, 0, 0]), -EINVAL, "flags");
    assert_eq!(sync_file_range(&s, [fd, u64::MAX, 0, 0, 0, 0]), -EINVAL, "a negative offset");
    assert_eq!(sync_file_range(&s, [fd, 1, u64::MAX - 1, 0, 0, 0]), -EINVAL, "a negative length past the offset");
    assert_eq!(sync_file_range(&s, [fd, i64::MAX as u64, 1, 0, 0, 0]), -EINVAL, "a wrap");
    assert_eq!(sync_file_range(&s, [rd, 0, 0, 0, 0, 0]), -ESPIPE, "a pipe");
}
