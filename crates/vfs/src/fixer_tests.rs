//! Host tests added by the FS-1 fixer round: hostile syscall arguments (no panic, Linux's errno), the fd limit
//! through the syscall path, close-on-exec on a forked table, link and the B3 policy, listing stability.

extern crate std;

use crate::abi::{O_CLOEXEC, O_CREAT, O_DIRECTORY, O_RDONLY, O_RDWR, O_WRONLY};
use crate::cases_test::{program_view, Mem, SUBJECT, USER};
use crate::fd::FdTable;
use crate::mount_ns::MountNs;
use crate::ops::{self, Ctx};
use crate::sys::{self, Sys};
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;

fn sys_for<'a>(mem: &'a Mem, fds: &'a FdTable, ns: &'a Arc<MountNs>) -> Sys<'a> {
    Sys { mem, fds, ns, cred: USER, subject: SUBJECT }
}

fn call(s: &Sys<'_>, nr: usize, a: [u64; 6]) -> i64 {
    sys::call(s, nr, a)
}

fn iov(mem: &Mem, segs: &[(u64, u64)]) -> u64 {
    let mut raw = Vec::new();
    for (b, l) in segs {
        raw.extend_from_slice(&b.to_le_bytes());
        raw.extend_from_slice(&l.to_le_bytes());
    }
    mem.alloc(&raw)
}

/// Malformed pointers, lengths and offsets: every call answers what Linux 6.6 answers and nothing panics
/// (dev builds have overflow checks on, so an unchecked `buf + done` would abort here).
#[test]
fn hostile_arguments() {
    let (ns, fds) = program_view();
    let mem = Mem::new();
    let s = sys_for(&mem, &fds, &ns);
    let path = mem.cstr(b"/tmp/h");
    let fd = call(&s, 2, [path, O_CREAT | O_RDWR, 0o644, 0, 0, 0]);
    assert!(fd >= 0);
    let fd = fd as u64;
    let data = mem.alloc(b"0123456789");
    assert_eq!(call(&s, 1, [fd, data, 10, 0, 0, 0]), 10);
    let buf = mem.alloc_zero(64);
    let near_top = u64::MAX - 0xfff;
    // read/write/pread/pwrite: a wrapping or upper-half range is EFAULT before anything is done (access_ok).
    for nr in [0usize, 1] {
        assert_eq!(call(&s, nr, [fd, near_top, 0x2000, 0, 0, 0]), -EFAULT, "nr {nr} wrapping range");
        assert_eq!(call(&s, nr, [fd, buf, u64::MAX, 0, 0, 0]), -EFAULT, "nr {nr} count u64::MAX");
        assert_eq!(call(&s, nr, [fd, 1 << 63, 1, 0, 0, 0]), -EFAULT, "nr {nr} kernel half");
        // A lower-half range that is not mapped faults in the copy itself (at offset 0, so there is data).
        assert_eq!(call(&s, 8, [fd, 0, 0, 0, 0, 0]), 0);
        assert_eq!(call(&s, nr, [fd, 0x7fff_ffff_f000, 0x8000_0000_0000, 0, 0, 0]), -EFAULT, "nr {nr} unmapped");
    }
    for nr in [17usize, 18] {
        assert_eq!(call(&s, nr, [fd, buf, 4, (-1i64) as u64, 0, 0]), -EINVAL, "nr {nr} negative offset");
        assert_eq!(call(&s, nr, [fd, near_top, 0x2000, 0, 0, 0]), -EFAULT, "nr {nr} wrapping range");
    }
    assert_eq!(call(&s, 18, [fd, data, 1, i64::MAX as u64, 0, 0]), -EFBIG);
    assert_eq!(call(&s, 17, [fd, buf, 4, 1 << 62, 0, 0]), 0);
    // A fault after some bytes keeps them: the arena ends a few bytes after `tail`.
    let tail = mem.alloc_zero(4);
    assert_eq!(call(&s, 17, [fd, tail, 10, 0, 0, 0]), -EFAULT, "fault in the first chunk copies nothing");
    // lseek: overflowing and negative results are EINVAL.
    assert_eq!(call(&s, 8, [fd, i64::MIN as u64, 1, 0, 0, 0]), -EINVAL);
    assert_eq!(call(&s, 8, [fd, 0, 0, 0, 0, 0]), 0);
    assert_eq!(call(&s, 8, [fd, i64::MAX as u64, 1, 0, 0, 0]), i64::MAX, "MAX_LFS_FILESIZE itself is a position");
    assert_eq!(call(&s, 8, [fd, 1, 1, 0, 0, 0]), -EINVAL);
    assert_eq!(call(&s, 8, [fd, i64::MAX as u64, 2, 0, 0, 0]), -EINVAL);
    assert_eq!(call(&s, 8, [fd, 0, 99, 0, 0, 0]), -EINVAL);
    // readv/writev: count above UIO_MAXIOV, a bad array, negative lengths before bad addresses, bad segments.
    assert_eq!(call(&s, 19, [fd, buf, 1025, 0, 0, 0]), -EINVAL);
    assert_eq!(call(&s, 19, [fd, 0xdead_0000, 2, 0, 0, 0]), -EFAULT);
    assert_eq!(call(&s, 19, [fd, buf, u64::MAX, 0, 0, 0]), -EINVAL);
    let v = iov(&mem, &[(near_top, 16), (buf, u64::MAX)]);
    assert_eq!(call(&s, 19, [fd, v, 2, 0, 0, 0]), -EINVAL, "negative length is found while copying the array");
    let v = iov(&mem, &[(buf, 4), (near_top, 16)]);
    assert_eq!(call(&s, 20, [fd, v, 2, 0, 0, 0]), -EFAULT);
    assert_eq!(call(&s, 19, [fd, v, 0, 0, 0, 0]), 0);
    // Paths: unterminated at the end of the mapped memory, PATH_MAX without a NUL, NULL.
    let unterminated = mem.alloc(b"/tmp/abc");
    assert_eq!(call(&s, 2, [unterminated, O_RDONLY, 0, 0, 0, 0]), -EFAULT);
    let long = mem.alloc(&[b'a'; 4096]);
    mem.alloc(&[b'a'; 64]);
    assert_eq!(call(&s, 2, [long, O_RDONLY, 0, 0, 0, 0]), -ENAMETOOLONG);
    assert_eq!(call(&s, 2, [0, O_RDONLY, 0, 0, 0, 0]), -EFAULT);
    assert_eq!(call(&s, 4, [path, 0, 0, 0, 0, 0]), -EFAULT, "stat into NULL");
    assert_eq!(call(&s, 332, [(-100i64) as u64, path, 0, 0x7ff, 0, 0]), -EFAULT, "statx into NULL");
    // getdents64: a bad buffer is EFAULT, a zero count EINVAL, a huge count is clipped.
    let tmp = mem.cstr(b"/tmp");
    let dfd = call(&s, 2, [tmp, O_RDONLY | O_DIRECTORY, 0, 0, 0, 0]) as u64;
    assert_eq!(call(&s, 217, [dfd, 0xdead_0000, 4096, 0, 0, 0]), -EFAULT);
    assert_eq!(call(&s, 217, [dfd, buf, 0, 0, 0, 0]), -EINVAL, "the fault above did not consume entries");
    let big = mem.alloc_zero(4096);
    // ".", "..", "h": 24 + 24 + 24 bytes.
    assert_eq!(call(&s, 217, [dfd, big, u64::MAX, 0, 0, 0]), 72);
    // fcntl locks with a bad struct pointer; dup2 to numbers far out.
    assert_eq!(call(&s, 72, [fd, 6, 0xdead_0000, 0, 0, 0]), -EFAULT);
    assert_eq!(call(&s, 33, [fd, 1 << 31, 0, 0, 0, 0]), -EBADF);
    assert_eq!(call(&s, 33, [fd, u32::MAX as u64, 0, 0, 0, 0]), -EBADF);
    assert_eq!(call(&s, 3, [u64::MAX, 0, 0, 0, 0, 0]), -EBADF);
}

/// RLIMIT_NOFILE through the syscalls: a lowered limit gives EMFILE for open, dup and F_DUPFD; dup2 at or
/// above it is EBADF; raising it again lets the table grow. Close-on-exec bits survive a fork copy and
/// `close_on_exec` closes exactly those.
#[test]
fn fd_limit_and_cloexec_through_the_calls() {
    let (ns, fds) = program_view();
    let mem = Mem::new();
    let s = sys_for(&mem, &fds, &ns);
    let path = mem.cstr(b"/tmp/l");
    let a = call(&s, 2, [path, O_CREAT | O_RDWR | O_CLOEXEC, 0o644, 0, 0, 0]);
    assert_eq!(a, 0);
    assert_eq!(call(&s, 32, [0, 0, 0, 0, 0, 0]), 1);
    fds.set_limit(2);
    assert_eq!(call(&s, 2, [path, O_RDONLY, 0, 0, 0, 0]), -EMFILE);
    assert_eq!(call(&s, 32, [0, 0, 0, 0, 0, 0]), -EMFILE);
    assert_eq!(call(&s, 72, [0, 0, 0, 0, 0, 0]), -EMFILE);
    assert_eq!(call(&s, 72, [0, 0, 2, 0, 0, 0]), -EINVAL);
    assert_eq!(call(&s, 33, [0, 2, 0, 0, 0, 0]), -EBADF);
    fds.set_limit(10_000);
    assert_eq!(call(&s, 33, [0, 9_999, 0, 0, 0, 0]), 9_999);
    assert_eq!(call(&s, 292, [0, 5_000, O_CLOEXEC, 0, 0, 0]), 5_000);
    let child = fds.duplicate().unwrap();
    assert_eq!(child.limit(), 10_000);
    assert_eq!(child.cloexec(0), Ok(true));
    assert_eq!(child.cloexec(1), Ok(false));
    assert_eq!(child.cloexec(5_000), Ok(true));
    child.close_on_exec();
    assert_eq!(child.get(0).err(), Some(EBADF));
    assert_eq!(child.get(5_000).err(), Some(EBADF));
    assert!(child.get(1).is_ok() && child.get(9_999).is_ok());
    assert_eq!(child.count(), 2);
    // The parent is untouched; positions are shared between the two tables (one open file).
    assert_eq!(fds.count(), 4);
    let data = mem.alloc(b"xyz");
    assert_eq!(call(&s, 1, [1, data, 3, 0, 0, 0]), 3);
    assert_eq!(child.get(1).unwrap().pos(), 3);
}

/// B3: linking an outside file the policy refuses into the program's own folder is EACCES (the later open
/// of the link would not be asked again).
#[test]
fn link_asks_the_policy_for_the_source() {
    crate::more_tests::install_policy();
    let ns = MountNs::new_empty();
    let k = Ctx::kernel(&ns);
    k.mkdir(None, b"/secret", 0o777).unwrap();
    k.mkdir(None, b"/home", 0o755).unwrap();
    k.mkdir(None, b"/home/p9", 0o777).unwrap();
    k.open(None, b"/secret/x", O_CREAT | O_WRONLY, 0o666).unwrap();
    k.open(None, b"/pub", O_CREAT | O_WRONLY, 0o666).unwrap();
    let c = crate::more_tests::p9_ctx(&ns);
    let e = |r: Result<(), i64>| r.err().unwrap_or(0);
    assert_eq!(e(c.link(None, b"/secret/x", None, b"/home/p9/x", false)), EACCES);
    assert!(c.lookup(None, b"/home/p9/x", true).is_err());
    assert_eq!(e(c.link(None, b"/pub", None, b"/home/p9/pub", false)), 0);
    assert_eq!(c.lookup(None, b"/pub", true).unwrap().attr().nlink, 2);
}

/// getdents64 while names come and go: a name that exists for the whole listing is listed exactly once
/// (listing numbers are never reused, a renamed or re-created name gets a new one).
#[test]
fn listing_keeps_stable_names() {
    let ns = Arc::new(MountNs::new_empty());
    let k = Ctx::kernel(&ns);
    k.mkdir(None, b"/d", 0o777).unwrap();
    for i in 0..50 {
        k.open(None, std::format!("/d/keep{i}").as_bytes(), O_CREAT | O_WRONLY, 0o644).unwrap();
    }
    let churn = {
        let ns = ns.clone();
        std::thread::spawn(move || {
            let c = Ctx::kernel(&ns);
            for i in 0..2000 {
                let n = std::format!("/d/tmp{}", i % 7);
                let _ = c.open(None, n.as_bytes(), O_CREAT | O_WRONLY, 0o644);
                let _ = c.rename(None, n.as_bytes(), None, b"/d/moved", 0);
                let _ = c.unlink(None, b"/d/moved");
            }
        })
    };
    for _ in 0..100 {
        let f = k.open(None, b"/d", O_RDONLY | O_DIRECTORY, 0).unwrap();
        let mut keep = 0;
        let mut seen = std::collections::BTreeSet::new();
        loop {
            // Small reads: many cursor steps while the other thread changes the directory.
            let b = ops::read_dir(&f, 100).unwrap();
            if b.is_empty() {
                break;
            }
            let mut off = 0;
            while off < b.len() {
                let reclen = u16::from_le_bytes([b[off + 16], b[off + 17]]) as usize;
                let name = b[off + 19..off + reclen].split(|&c| c == 0).next().unwrap().to_vec();
                if name.starts_with(b"keep") {
                    keep += 1;
                    // Only names nobody changes must appear once: a churned name (made again, or renamed onto)
                    // gets a new position and may be listed twice in one pass (POSIX: unspecified; Linux's
                    // dcache_readdir does the same).
                    assert!(seen.insert(name), "a stable name listed twice in one pass");
                }
                off += reclen;
            }
        }
        assert_eq!(keep, 50);
    }
    churn.join().unwrap();
}
