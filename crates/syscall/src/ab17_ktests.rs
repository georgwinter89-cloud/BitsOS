//! AB-17 ktests (Safe, rerunnable): process_vm_readv/writev (data across page edges and segments, the error
//! order, short counts, ptrace rights, no write through to a private file mapping's file), kcmp (equality,
//! antisymmetry, errors in Linux's order) and F_DUPFD_QUERY. Held processes only (`ab6_ktests::T`); no user
//! code runs.

use crate::ab6_ktests::T;
use alloc::format;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use uapi::ab::kcmp::*;
use uapi::ab::nr_more::{KCMP, PIDFD_GETFD, PIDFD_OPEN, PROCESS_VM_READV, PROCESS_VM_WRITEV};
use uapi::errno::*;
use uapi::nr;

const KERNEL_ADDR: u64 = 0xffff_8880_0000_0000;
const NO_PID: u64 = 0x3fff_fff0;
const AT_FDCWD: u64 = -100i64 as u64;
const O_RDWR: u64 = 2;
const O_CREAT: u64 = 0o100;
const O_PATH: u64 = 0o1000_0000;
const MAP_PRIVATE: u64 = 2;
const F_DUPFD_CLOEXEC: u64 = 1030;
const F_DUPFD_QUERY: u64 = 1027;

/// Writes iovecs at scratch offset `at` of `t` and answers their user address.
fn iov(t: &T, at: u64, v: &[(u64, u64)]) -> u64 {
    let mut b = Vec::new();
    for &(base, len) in v {
        b.extend_from_slice(&base.to_le_bytes());
        b.extend_from_slice(&len.to_le_bytes());
    }
    t.put(at, &b);
    t.a(at)
}

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

fn expect(what: &str, got: i64, want: i64) -> TestResult {
    kassert!(got == want, "{what}: {got}, want {want}");
    Ok(())
}

ktest!("ab.procvm.data", Safe, data);
fn data() -> TestResult {
    let mut a = T::new("ab17-reader")?;
    let b = T::new("ab17-target")?;
    let pid = b.p.pid as u64;
    // 12000 bytes from b at an odd offset into two local segments, both crossing page edges.
    let src = pattern(12000, 7);
    b.put(0x1064, &src);
    a.put(0x3000, &alloc::vec![0u8; 12000]);
    let l = iov(&a, 0x100, &[(a.a(0x3000 + 4001), 5000), (a.a(0x3000 + 9001 + 4), 7000)]);
    let r = iov(&a, 0x200, &[(b.a(0x1064), 3), (0, 0), (b.a(0x1067), 11997)]);
    expect("readv", a.call(PROCESS_VM_READV, &[pid, l, 2, r, 3, 0]), 12000)?;
    let got = [a.get(0x3000 + 4001, 5000), a.get(0x3000 + 9005, 7000)].concat();
    kassert!(got == src, "readv data differs");
    // And back: writev into another place of b.
    let w = iov(&a, 0x300, &[(b.a(0x6000 + 13), 12000)]);
    expect("writev", a.call(PROCESS_VM_WRITEV, &[pid, l, 2, w, 1, 0]), 12000)?;
    kassert!(b.get(0x6000 + 13, 12000) == src, "writev data differs");
    // The remote side asks for more than the local holds: the local size.
    let big = iov(&a, 0x400, &[(b.a(0x1064), 1 << 40)]);
    let l16 = iov(&a, 0x500, &[(a.a(0x3000), 16)]);
    expect("remote longer", a.call(PROCESS_VM_READV, &[pid, l16, 1, big, 1, 0]), 16)?;
    // A short count: the second remote segment faults after 8 bytes moved.
    let rs = iov(&a, 0x600, &[(b.a(0x1064), 8), (KERNEL_ADDR, 8)]);
    expect("short", a.call(PROCESS_VM_READV, &[pid, l16, 1, rs, 2, 0]), 8)?;
    // The caller's own memory (same address space: no permission check).
    let me = a.p.pid as u64;
    let own = iov(&a, 0x700, &[(a.a(0x1064), 16)]);
    a.put(0x1064, b"0123456789abcdef");
    let l2 = iov(&a, 0x780, &[(a.a(0x5000), 16)]);
    expect("own memory", a.call(PROCESS_VM_READV, &[me, l2, 1, own, 1, 0]), 16)?;
    kassert!(a.get(0x5000, 16) == b"0123456789abcdef", "own copy");
    Ok(())
}

ktest!("ab.procvm.errors", Safe, errors);
fn errors() -> TestResult {
    let mut a = T::new("ab17-err")?;
    let b = T::new("ab17-err-target")?;
    let pid = b.p.pid as u64;
    let l = iov(&a, 0x100, &[(a.a(0x3000), 16)]);
    let r = iov(&a, 0x200, &[(b.a(0x1000), 16)]);
    expect("flags", a.call(PROCESS_VM_READV, &[pid, l, 1, r, 1, 1]), -EINVAL)?;
    expect("flags before a bad pid", a.call(PROCESS_VM_READV, &[NO_PID, KERNEL_ADDR, 1, r, 1, 1]), -EINVAL)?;
    // Local errors come before the pid.
    expect("local array", a.call(PROCESS_VM_READV, &[NO_PID, KERNEL_ADDR, 1, r, 1, 0]), -EFAULT)?;
    expect("local count", a.call(PROCESS_VM_READV, &[NO_PID, l, 1025, r, 1, 0]), -EINVAL)?;
    let neg = iov(&a, 0x300, &[(a.a(0x3000), 1 << 63)]);
    expect("negative local length", a.call(PROCESS_VM_READV, &[NO_PID, neg, 1, r, 1, 0]), -EINVAL)?;
    let kseg = iov(&a, 0x380, &[(a.a(0x3000), 4), (KERNEL_ADDR, 4)]);
    expect("local segment access_ok", a.call(PROCESS_VM_READV, &[NO_PID, kseg, 2, r, 1, 0]), -EFAULT)?;
    // The count is an unsigned int: 2^32 is 0.
    expect("local count 2^32", a.call(PROCESS_VM_READV, &[NO_PID, l, 1 << 32, r, 1, 0]), 0)?;
    // Nothing to move: 0 before the remote iovecs and the pid are looked at.
    let empty = iov(&a, 0x400, &[(a.a(0x3000), 0), (a.a(0x3000), 0)]);
    expect("empty local", a.call(PROCESS_VM_READV, &[NO_PID, empty, 2, KERNEL_ADDR, 5000, 0]), 0)?;
    expect("no local", a.call(PROCESS_VM_READV, &[NO_PID, l, 0, KERNEL_ADDR, 1, 0]), 0)?;
    // Remote iovecs: EFAULT, EINVAL, and an empty remote side answers 0 before the pid.
    expect("remote array", a.call(PROCESS_VM_READV, &[NO_PID, l, 1, KERNEL_ADDR, 1, 0]), -EFAULT)?;
    expect("remote count", a.call(PROCESS_VM_READV, &[NO_PID, l, 1, r, 1025, 0]), -EINVAL)?;
    let rneg = iov(&a, 0x480, &[(b.a(0), 1 << 63)]);
    expect("negative remote length", a.call(PROCESS_VM_READV, &[NO_PID, l, 1, rneg, 1, 0]), -EINVAL)?;
    let rempty = iov(&a, 0x500, &[(KERNEL_ADDR, 0)]);
    expect("empty remote", a.call(PROCESS_VM_READV, &[NO_PID, l, 1, rempty, 1, 0]), 0)?;
    expect("no remote", a.call(PROCESS_VM_READV, &[NO_PID, l, 1, r, 0, 0]), 0)?;
    // Then the pid.
    expect("no such pid", a.call(PROCESS_VM_READV, &[NO_PID, l, 1, r, 1, 0]), -ESRCH)?;
    expect("pid 0", a.call(PROCESS_VM_READV, &[0, l, 1, r, 1, 0]), -ESRCH)?;
    expect("pid -1", a.call(PROCESS_VM_WRITEV, &[u64::MAX, l, 1, r, 1, 0]), -ESRCH)?;
    // A bad address on either side with nothing moved: EFAULT.
    let rk = iov(&a, 0x580, &[(KERNEL_ADDR, 16)]);
    expect("remote kernel address", a.call(PROCESS_VM_READV, &[pid, l, 1, rk, 1, 0]), -EFAULT)?;
    let lro = a.p.mm.sys_mmap(0, 4096, 1, 0x22, 0).map_err(|e| format!("mmap ro: {e:?}"))?;
    let lr = iov(&a, 0x600, &[(lro, 16)]);
    expect("read into a read-only local page", a.call(PROCESS_VM_READV, &[pid, lr, 1, r, 1, 0]), -EFAULT)?;
    // A read-only remote page: readable, but no write (no FOLL_FORCE).
    let rro = b.p.mm.sys_mmap(0, 4096, 1, 0x22, 0).map_err(|e| format!("mmap ro: {e:?}"))?;
    let rr = iov(&a, 0x680, &[(rro, 16)]);
    expect("read a read-only remote page", a.call(PROCESS_VM_READV, &[pid, l, 1, rr, 1, 0]), 16)?;
    expect("write a read-only remote page", a.call(PROCESS_VM_WRITEV, &[pid, l, 1, rr, 1, 0]), -EFAULT)?;
    Ok(())
}

// R1/B4: lengths near MAX_RW_COUNT over a few mapped pages. The page-sized bounce buffer moves exactly the
// mapped bytes and stops at the first hole; nothing in the kernel grows with the asked length.
ktest!("ab.procvm.huge", Safe, huge);
fn huge() -> TestResult {
    const HUGE: u64 = 0x7fff_f000;
    let mut a = T::new("ab17-huge")?;
    let b = T::new("ab17-huge-target")?;
    let pid = b.p.pid as u64;
    // Three local pages, the third unmapped again: a hole after 8192 bytes.
    let lm = a.p.mm.sys_mmap(0, 3 * 4096, 3, 0x22, 0).map_err(|e| format!("mmap: {e:?}"))?;
    a.p.mm.sys_munmap(lm + 2 * 4096, 4096).map_err(|e| format!("munmap: {e:?}"))?;
    let src = pattern(8192, 3);
    b.put(0, &src);
    let l = iov(&a, 0x100, &[(lm, HUGE)]);
    let r = iov(&a, 0x200, &[(b.a(0), HUGE)]);
    expect("readv, local hole", a.call(PROCESS_VM_READV, &[pid, l, 1, r, 1, 0]), 8192)?;
    let mut got = alloc::vec![0u8; 8192];
    a.p.mm.copy_from_user(&mut got, lm).map_err(|e| format!("read back: {e:?}"))?;
    kassert!(got == src, "readv data differs");
    let w = iov(&a, 0x280, &[(b.a(0x8000), HUGE)]);
    expect("writev, local hole", a.call(PROCESS_VM_WRITEV, &[pid, l, 1, w, 1, 0]), 8192)?;
    kassert!(b.get(0x8000, 8192) == src, "writev data differs");
    // The remote side ends first: one remote page, then a hole.
    let rm = b.p.mm.sys_mmap(0, 2 * 4096, 3, 0x22, 0).map_err(|e| format!("mmap: {e:?}"))?;
    b.p.mm.sys_munmap(rm + 4096, 4096).map_err(|e| format!("munmap: {e:?}"))?;
    let la = iov(&a, 0x300, &[(a.a(0x8000), HUGE)]);
    let rh = iov(&a, 0x380, &[(rm, HUGE)]);
    expect("readv, remote hole", a.call(PROCESS_VM_READV, &[pid, la, 1, rh, 1, 0]), 4096)?;
    expect("writev, remote hole", a.call(PROCESS_VM_WRITEV, &[pid, la, 1, rh, 1, 0]), 4096)?;
    // Many huge segments: the local total is clipped to MAX_RW_COUNT, the copy still stops at the hole.
    let many: Vec<(u64, u64)> = (0..64).map(|_| (lm, HUGE)).collect();
    let lmany = iov(&a, 0x400, &many);
    expect("64 huge local segments", a.call(PROCESS_VM_READV, &[pid, lmany, 64, r, 1, 0]), 8192)?;
    Ok(())
}

ktest!("ab.procvm.perm", Safe, perm);
fn perm() -> TestResult {
    let mut a = T::new("ab17-perm")?;
    let mut target = T::new("ab17-perm-target")?;
    let stranger = T::with("ab17-perm-stranger", 1001, false)?;
    let mut root = T::with("ab17-perm-root", 0, false)?;
    let l = iov(&a, 0x100, &[(a.a(0x3000), 16)]);
    let r = iov(&a, 0x200, &[(target.a(0x1000), 16)]);
    let rs = iov(&a, 0x280, &[(stranger.a(0x1000), 16)]);
    expect("same uid", a.call(PROCESS_VM_READV, &[target.p.pid as u64, l, 1, r, 1, 0]), 16)?;
    expect("another uid", a.call(PROCESS_VM_READV, &[stranger.p.pid as u64, l, 1, rs, 1, 0]), -EPERM)?;
    expect("another uid, writev", a.call(PROCESS_VM_WRITEV, &[stranger.p.pid as u64, l, 1, rs, 1, 0]), -EPERM)?;
    // EPERM before any copy is tried.
    let rk = iov(&a, 0x300, &[(KERNEL_ADDR, 16)]);
    expect("EPERM before EFAULT", a.call(PROCESS_VM_READV, &[stranger.p.pid as u64, l, 1, rk, 1, 0]), -EPERM)?;
    expect("PR_SET_DUMPABLE 0", target.call(nr::PRCTL, &[4, 0]), 0)?;
    expect("undumpable", a.call(PROCESS_VM_READV, &[target.p.pid as u64, l, 1, r, 1, 0]), -EPERM)?;
    let rl = iov(&root, 0x100, &[(root.a(0x3000), 16)]);
    let rr = iov(&root, 0x200, &[(stranger.a(0x1000), 16)]);
    expect("root (CAP_SYS_PTRACE)", root.call(PROCESS_VM_READV, &[stranger.p.pid as u64, rl, 1, rr, 1, 0]), 16)?;
    let rt = iov(&root, 0x280, &[(target.a(0x1000), 16)]);
    expect("root, undumpable", root.call(PROCESS_VM_READV, &[target.p.pid as u64, rl, 1, rt, 1, 0]), 16)?;
    // An ended process (no parent: reaped at once): ESRCH.
    let gone = stranger.p.pid as u64;
    drop(stranger);
    let mut ok = false;
    for _ in 0..200 {
        if a.call(PROCESS_VM_READV, &[gone, l, 1, rs, 1, 0]) == -ESRCH {
            ok = true;
            break;
        }
        sched::sleep_ms(10);
    }
    kassert!(ok, "after the end: not ESRCH");
    Ok(())
}

// AB-8 CR 4: a write into a private file mapping must never reach the file (Linux: COW page; no FOLL_FORCE).
ktest!("ab.procvm.private_file", Safe, private_file);
fn private_file() -> TestResult {
    static SEQ: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let path = format!("/tmp/ab17-pf-{}\0", SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed));
    let r = private_file_in(&path);
    let _ = vfs::root_ns().remove(path.trim_end_matches('\0'));
    r
}

fn private_file_in(path: &str) -> TestResult {
    let mut a = T::new("ab17-pf")?;
    let mut b = T::new("ab17-pf-target")?;
    b.put(0x100, path.as_bytes());
    let fd = b.call(nr::OPENAT, &[AT_FDCWD, b.a(0x100), O_RDWR | O_CREAT, 0o600]);
    kassert!(fd >= 0, "open {fd}");
    b.put(0x800, &[b'o'; 64]);
    expect("pwrite", b.call(nr::PWRITE64, &[fd as u64, b.a(0x800), 64, 0]), 64)?;
    // Not touched before the write: the first fault comes from process_vm_writev.
    let rw = b.call(nr::MMAP, &[0, 4096, 3, MAP_PRIVATE, fd as u64, 0]);
    let ro = b.call(nr::MMAP, &[0, 4096, 1, MAP_PRIVATE, fd as u64, 0]);
    kassert!(rw > 0 && ro > 0, "mmap {rw} {ro}");
    a.put(0x3000, b"NEWBYTES");
    let l = iov(&a, 0x100, &[(a.a(0x3000), 8)]);
    let r = iov(&a, 0x200, &[(rw as u64 + 4, 8)]);
    expect("writev into the private map", a.call(PROCESS_VM_WRITEV, &[b.p.pid as u64, l, 1, r, 1, 0]), 8)?;
    let mut m = [0u8; 12];
    kassert!(b.p.mm.copy_from_user(&mut m, rw as u64).is_ok() && &m == b"ooooNEWBYTES", "the mapping: {m:?}");
    let rr = iov(&a, 0x280, &[(ro as u64, 8)]);
    expect("writev into a read-only private map", a.call(PROCESS_VM_WRITEV, &[b.p.pid as u64, l, 1, rr, 1, 0]), -EFAULT)?;
    expect("pread", b.call(nr::PREAD64, &[fd as u64, b.a(0x900), 64, 0]), 64)?;
    kassert!(b.get(0x900, 64) == [b'o'; 64], "the file changed: {:?}", b.get(0x900, 16));
    let mut m2 = [0u8; 8];
    kassert!(b.p.mm.copy_from_user(&mut m2, ro as u64).is_ok() && m2 == [b'o'; 8], "the read-only map: {m2:?}");
    Ok(())
}

fn kc(t: &mut T, p1: u32, p2: u32, ty: u64, i1: u64, i2: u64) -> i64 {
    t.call(KCMP, &[p1 as u64, p2 as u64, ty, i1, i2])
}

ktest!("ab.kcmp.basic", Safe, kcmp_basic);
fn kcmp_basic() -> TestResult {
    let mut a = T::new("ab17-kcmp")?;
    let mut b = T::new("ab17-kcmp-b")?;
    let (pa, pb) = (a.p.pid, b.p.pid);
    let e1 = a.call(nr::EVENTFD2, &[0, 0]);
    let e2 = a.call(nr::EVENTFD2, &[0, 0]);
    let d1 = a.call(nr::DUP, &[e1 as u64]);
    kassert!(e1 >= 0 && e2 >= 0 && d1 >= 0, "fds {e1} {e2} {d1}");
    let (e1, e2, d1) = (e1 as u64, e2 as u64, d1 as u64);
    let f = KCMP_FILE as u64;
    expect("FILE dup", kc(&mut a, pa, pa, f, e1, d1), 0)?;
    let x = kc(&mut a, pa, pa, f, e1, e2);
    let y = kc(&mut a, pa, pa, f, e2, e1);
    kassert!((x == 1 && y == 2) || (x == 2 && y == 1), "FILE order {x} {y}");
    expect("FILE bad fd 1", kc(&mut a, pa, pa, f, 999, e1), -EBADF)?;
    expect("FILE bad fd 2", kc(&mut a, pa, pa, f, e1, 999), -EBADF)?;
    // The same open file in another process (pidfd_getfd).
    let pfd = b.call(PIDFD_OPEN, &[pa as u64, 0]);
    let got = b.call(PIDFD_GETFD, &[pfd as u64, e1, 0]);
    kassert!(pfd >= 0 && got >= 0, "pidfd {pfd} getfd {got}");
    expect("FILE across processes", kc(&mut a, pa, pb, f, e1, got as u64), 0)?;
    kassert!(kc(&mut a, pa, pb, f, e2, got as u64) > 0, "FILE across processes, another file");
    // An O_PATH fd counts (raw lookup).
    a.put(0x100, b"/\0");
    let op = a.call(nr::OPENAT, &[AT_FDCWD, a.a(0x100), O_PATH, 0]);
    kassert!(op >= 0, "O_PATH open {op}");
    let opd = a.call(nr::FCNTL, &[op as u64, F_DUPFD_CLOEXEC, 0]);
    expect("FILE O_PATH", kc(&mut a, pa, pa, f, op as u64, opd as u64), 0)?;
    for (ty, name) in [(KCMP_VM, "VM"), (KCMP_FILES, "FILES"), (KCMP_FS, "FS"), (KCMP_SIGHAND, "SIGHAND")] {
        expect(&format!("{name} self"), kc(&mut a, pa, pa, ty as u64, 0, 0), 0)?;
        let x = kc(&mut a, pa, pb, ty as u64, 0, 0);
        let y = kc(&mut a, pb, pa, ty as u64, 0, 0);
        kassert!(x + y == 3 && x * y == 2, "{name} two processes: {x} {y}");
    }
    expect("IO", kc(&mut a, pa, pb, KCMP_IO as u64, 0, 0), 0)?;
    expect("SYSVSEM", kc(&mut a, pa, pb, KCMP_SYSVSEM as u64, 0, 0), 0)?;
    // Errors in Linux's order: pids, permission, type.
    expect("type 8", kc(&mut a, pa, pb, KCMP_TYPES as u64, 0, 0), -EINVAL)?;
    expect("type -1", kc(&mut a, pa, pb, u32::MAX as u64, 0, 0), -EINVAL)?;
    expect("no pid 1", kc(&mut a, NO_PID as u32, pa, 99, 0, 0), -ESRCH)?;
    expect("no pid 2", kc(&mut a, pa, NO_PID as u32, KCMP_VM as u64, 0, 0), -ESRCH)?;
    expect("pid 0", kc(&mut a, 0, pa, KCMP_VM as u64, 0, 0), -ESRCH)?;
    // EPOLL_TFD up to the lookup.
    let ep = a.call(nr::EPOLL_CREATE1, &[0]);
    kassert!(ep >= 0, "epoll {ep}");
    let t = KCMP_EPOLL_TFD as u64;
    expect("EPOLL_TFD slot", kc(&mut a, pa, pa, t, e1, KERNEL_ADDR), -EFAULT)?;
    let slot = |a: &T, efd: u64| {
        let mut s = [0u8; 12];
        s[0..4].copy_from_slice(&(efd as u32).to_le_bytes());
        s[4..8].copy_from_slice(&(e1 as u32).to_le_bytes());
        a.put(0x200, &s);
        a.a(0x200)
    };
    let s = slot(&a, 999);
    expect("EPOLL_TFD idx1", kc(&mut a, pa, pa, t, 999, s), -EBADF)?;
    expect("EPOLL_TFD efd", kc(&mut a, pa, pa, t, e1, s), -EBADF)?;
    let s = slot(&a, e2);
    expect("EPOLL_TFD not an epoll", kc(&mut a, pa, pa, t, e1, s), -EINVAL)?;
    let s = slot(&a, op as u64);
    expect("EPOLL_TFD O_PATH efd (found, no epoll)", kc(&mut a, pa, pa, t, e1, s), -EINVAL)?;
    let s = slot(&a, ep as u64);
    expect("EPOLL_TFD not watched", kc(&mut a, pa, pa, t, e1, s), -ENOENT)?;
    // EPOLL_CTL_ADD e1 (epoll_event: EPOLLIN, data 0), then the lookup finds it (INT-W24, vfs Epoll::target).
    let mut ev = [0u8; 12];
    ev[0..4].copy_from_slice(&1u32.to_le_bytes());
    a.put(0x300, &ev);
    let r = a.call(nr::EPOLL_CTL, &[ep as u64, 1, e1, a.a(0x300)]);
    kassert!(r == 0, "epoll_ctl add {r}");
    expect("EPOLL_TFD target is idx1's file", kc(&mut a, pa, pa, t, e1, s), 0)?;
    let other = kc(&mut a, pa, pa, t, e2, s);
    kassert!(other == 1 || other == 2, "EPOLL_TFD another file {other}");
    let mut s2 = [0u8; 12];
    s2[0..4].copy_from_slice(&(ep as u32).to_le_bytes());
    s2[4..8].copy_from_slice(&(e1 as u32).to_le_bytes());
    s2[8..12].copy_from_slice(&1u32.to_le_bytes());
    a.put(0x220, &s2);
    let s2 = a.a(0x220);
    expect("EPOLL_TFD toff past the items", kc(&mut a, pa, pa, t, e1, s2), -ENOENT)?;
    for fd in [e1, e2, d1, op as u64, opd as u64, ep as u64] {
        a.call(nr::CLOSE, &[fd]);
    }
    Ok(())
}

ktest!("ab.kcmp.perm", Safe, kcmp_perm);
fn kcmp_perm() -> TestResult {
    let mut a = T::new("ab17-kcmp-perm")?;
    let mut target = T::new("ab17-kcmp-target")?;
    let stranger = T::with("ab17-kcmp-stranger", 1001, false)?;
    let mut root = T::with("ab17-kcmp-root", 0, false)?;
    let (pa, pt, ps) = (a.p.pid, target.p.pid, stranger.p.pid);
    kassert!(kc(&mut a, pa, pt, KCMP_VM as u64, 0, 0) > 0, "same uid");
    expect("another uid", kc(&mut a, pa, ps, KCMP_VM as u64, 0, 0), -EPERM)?;
    expect("another uid first", kc(&mut a, ps, pa, KCMP_VM as u64, 0, 0), -EPERM)?;
    expect("EPERM before the type", kc(&mut a, pa, ps, 99, 0, 0), -EPERM)?;
    expect("PR_SET_DUMPABLE 0", target.call(nr::PRCTL, &[4, 0]), 0)?;
    expect("undumpable", kc(&mut a, pa, pt, KCMP_VM as u64, 0, 0), -EPERM)?;
    kassert!(kc(&mut root, pt, ps, KCMP_VM as u64, 0, 0) > 0, "root may");
    Ok(())
}

ktest!("ab.fcntl.dupfd_query", Safe, dupfd_query);
fn dupfd_query() -> TestResult {
    let mut a = T::new("ab17-dupfd-query")?;
    let e1 = a.call(nr::EVENTFD2, &[0, 0]) as u64;
    let e2 = a.call(nr::EVENTFD2, &[0, 0]) as u64;
    let d = a.call(nr::FCNTL, &[e1, F_DUPFD_CLOEXEC, 0]) as u64;
    expect("its dup", a.call(nr::FCNTL, &[e1, F_DUPFD_QUERY, d]), 1)?;
    expect("itself", a.call(nr::FCNTL, &[e1, F_DUPFD_QUERY, e1]), 1)?;
    expect("another file", a.call(nr::FCNTL, &[e1, F_DUPFD_QUERY, e2]), 0)?;
    expect("closed", a.call(nr::FCNTL, &[e1, F_DUPFD_QUERY, 999]), -EBADF)?;
    expect("negative", a.call(nr::FCNTL, &[e1, F_DUPFD_QUERY, u64::MAX]), -EBADF)?;
    expect("bad first fd", a.call(nr::FCNTL, &[999, F_DUPFD_QUERY, e1]), -EBADF)?;
    a.put(0x100, b"/\0");
    let op = a.call(nr::OPENAT, &[AT_FDCWD, a.a(0x100), O_PATH, 0]) as u64;
    let opd = a.call(nr::FCNTL, &[op, F_DUPFD_CLOEXEC, 0]) as u64;
    expect("O_PATH on both sides", a.call(nr::FCNTL, &[op, F_DUPFD_QUERY, opd]), 1)?;
    expect("O_PATH and another", a.call(nr::FCNTL, &[op, F_DUPFD_QUERY, e1]), 0)?;
    // Agrees with kcmp(KCMP_FILE), Mesa's fallback.
    let p = a.p.pid as u64;
    expect("kcmp agrees", a.call(KCMP, &[p, p, KCMP_FILE as u64, e1, d]), 0)?;
    for fd in [e1, e2, d, op, opd] {
        a.call(nr::CLOSE, &[fd]);
    }
    Ok(())
}
