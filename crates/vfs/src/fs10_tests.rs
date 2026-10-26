//! FS-10 host tests: memfd_create's checks in Linux's order, the start seals of each flag set, F_ADD_SEALS /
//! F_GET_SEALS rules, the seals on write, truncate and shared mappings (EBUSY while one may write, EPERM for a
//! new writable one, read-only ones allowed as on CachyOS), W^X of F_SEAL_EXEC, the names memfds show, and the
//! proc magic link that opens a memfd again (the walk's `nd_jump_link`). The answers were checked on CachyOS
//! (host/tests/fs/memfd-linux.txt holds the program transcripts; host/tests/fs/cases.txt the line cases).
//! Fixer round: fallocate on memfds and sized tmpfs pools, the refusal counters, and two races (F_SEAL_WRITE
//! against threads making writable shared mappings, and against a writing thread).

extern crate std;

use crate::cases_test::{program_view, Mem, SUBJECT, USER};
use crate::fd::FdTable;
use crate::memfd::*;
use crate::mmap::{self, MapBacking, MAP_PRIVATE, MAP_SHARED};
use crate::mount_ns::{MountNs, Path};
use crate::synth::{DirSource, MagicLink, SynthEntry};
use crate::sys::{self, Sys};
use crate::Errno;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use std::format;
use uapi::errno::*;

const P: u64 = 4096;
const RW: u64 = 3;
const NR_MEMFD: usize = 319;
const NR_FCNTL: usize = 72;

fn call(s: &Sys<'_>, nr: usize, a: [u64; 6]) -> i64 {
    sys::call(s, nr, a)
}

fn memfd(s: &Sys<'_>, mem: &Mem, name: &[u8], flags: u32) -> i64 {
    let n = mem.cstr(name);
    call(s, NR_MEMFD, [n, flags as u64, 0, 0, 0, 0])
}

fn fcntl(s: &Sys<'_>, fd: i64, cmd: u64, arg: u64) -> i64 {
    call(s, NR_FCNTL, [fd as u64, cmd, arg, 0, 0, 0])
}

/// fstat's (mode, nlink, size).
fn fstat(s: &Sys<'_>, mem: &Mem, fd: i64) -> (u32, u64, i64) {
    let st = mem.alloc_zero(144);
    assert_eq!(call(s, 5, [fd as u64, st, 0, 0, 0, 0]), 0);
    let b = mem.get(st, 144);
    let nlink = u64::from_le_bytes(b[16..24].try_into().unwrap());
    let mode = u32::from_le_bytes(b[24..28].try_into().unwrap());
    let size = i64::from_le_bytes(b[48..56].try_into().unwrap());
    (mode, nlink, size)
}

fn pwrite(s: &Sys<'_>, mem: &Mem, fd: i64, data: &[u8], at: u64) -> i64 {
    let b = mem.alloc(data);
    call(s, 18, [fd as u64, b, data.len() as u64, at, 0, 0])
}

fn ftruncate(s: &Sys<'_>, fd: i64, len: u64) -> i64 {
    call(s, 77, [fd as u64, len, 0, 0, 0, 0])
}

macro_rules! env {
    ($ns:ident, $fds:ident, $mem:ident, $s:ident) => {
        let ($ns, $fds) = program_view();
        let $mem = Mem::new();
        let $s = Sys { mem: &$mem, fds: &$fds, ns: &$ns, cred: USER, subject: SUBJECT };
    };
}

#[test]
fn create_checks_in_linux_order() {
    env!(ns, fds, mem, s);
    // Unknown flags first, even with a bad name; huge page size bits only with MFD_HUGETLB.
    assert_eq!(call(&s, NR_MEMFD, [0, 0x40, 0, 0, 0, 0]), -EINVAL);
    assert_eq!(memfd(&s, &mem, b"x", 21 << 26), -EINVAL);
    assert_eq!(memfd(&s, &mem, b"x", MFD_EXEC | MFD_NOEXEC_SEAL), -EINVAL);
    // The name: a fault, more than 249 bytes; exactly 249 is fine.
    assert_eq!(call(&s, NR_MEMFD, [0, 0, 0, 0, 0, 0]), -EFAULT);
    assert_eq!(memfd(&s, &mem, &[b'n'; MFD_NAME_MAX_LEN + 1], 0), -EINVAL);
    let fd = memfd(&s, &mem, &[b'n'; MFD_NAME_MAX_LEN], 0);
    assert_eq!(fd, 0, "the lowest free fd");
    // No hugetlbfs in V2: valid MFD_HUGETLB flags get as far as the file and answer ENOSYS, without an fd left.
    assert_eq!(memfd(&s, &mem, b"huge", MFD_HUGETLB | (21 << 26)), -ENOSYS);
    assert_eq!(memfd(&s, &mem, b"next", 0), 1, "the ENOSYS call gave its reserved fd back");
    // EMFILE comes before the file is made.
    fds.set_limit(2);
    assert_eq!(memfd(&s, &mem, b"full", 0), -EMFILE);
    fds.set_limit(1024);
    // O_RDWR|O_LARGEFILE, close-on-exec only with MFD_CLOEXEC.
    assert_eq!(fcntl(&s, fd, crate::abi::F_GETFL, 0), 0o100002);
    assert_eq!(fcntl(&s, fd, crate::abi::F_GETFD, 0), 0);
    let c = memfd(&s, &mem, b"c", MFD_CLOEXEC);
    assert_eq!(fcntl(&s, c, crate::abi::F_GETFD, 0), 1);
    let _ = ns;
}

#[test]
fn start_modes_and_seals() {
    env!(ns, fds, mem, s);
    let cases: [(u32, u32, u32); 6] = [
        (0, 0o777, F_SEAL_SEAL),
        (MFD_EXEC, 0o777, F_SEAL_SEAL),
        (MFD_ALLOW_SEALING, 0o777, 0),
        (MFD_ALLOW_SEALING | MFD_EXEC, 0o777, 0),
        // MFD_NOEXEC_SEAL allows sealing by itself (6.6 and CachyOS 7.2 alike).
        (MFD_NOEXEC_SEAL, 0o666, F_SEAL_EXEC),
        (MFD_NOEXEC_SEAL | MFD_ALLOW_SEALING, 0o666, F_SEAL_EXEC),
    ];
    for (flags, perm, seals) in cases {
        let fd = memfd(&s, &mem, b"m", flags);
        assert!(fd >= 0);
        assert_eq!(fstat(&s, &mem, fd), (uapi::S_IFREG | perm, 0, 0), "flags {flags:#x}");
        assert_eq!(fcntl(&s, fd, F_GET_SEALS, 0), seals as i64, "flags {flags:#x}");
    }
    let _ = (ns, fds);
}

#[test]
fn add_seals_rules() {
    env!(ns, fds, mem, s);
    let fd = memfd(&s, &mem, b"s", MFD_ALLOW_SEALING);
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, 0x40), -EINVAL, "unknown seal bit");
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, 0), 0);
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, F_SEAL_SHRINK as u64), 0);
    // fcntl's argument is an unsigned int: the high half is not looked at.
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, (1 << 32) | F_SEAL_GROW as u64), 0);
    assert_eq!(fcntl(&s, fd, F_GET_SEALS, 0), (F_SEAL_SHRINK | F_SEAL_GROW) as i64);
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, F_SEAL_SEAL as u64), 0);
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, 0), -EPERM, "F_SEAL_SEAL: no more seals, not even none");
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, 0x40), -EINVAL, "the bits are checked before the file's seals");
    // A plain memfd starts sealed.
    let plain = memfd(&s, &mem, b"p", 0);
    assert_eq!(fcntl(&s, plain, F_ADD_SEALS, F_SEAL_GROW as u64), -EPERM);
    // Other files: a tmpfs file has seals (F_SEAL_SEAL); an fd not open for writing is EPERM before anything
    // else; a pipe has none (EINVAL); an O_PATH fd is EBADF.
    let path = mem.cstr(b"/tmp/sealtest");
    let w = call(&s, 2, [path, 0o102, 0o600, 0, 0, 0]);
    assert_eq!(fcntl(&s, w, F_GET_SEALS, 0), F_SEAL_SEAL as i64);
    assert_eq!(fcntl(&s, w, F_ADD_SEALS, F_SEAL_WRITE as u64), -EPERM);
    let r = call(&s, 2, [path, 0, 0, 0, 0, 0]);
    assert_eq!(fcntl(&s, r, F_ADD_SEALS, 0x40), -EPERM);
    let o = call(&s, 2, [path, crate::abi::O_PATH, 0, 0, 0, 0]);
    assert_eq!(fcntl(&s, o, F_GET_SEALS, 0), -EBADF);
    let pfd = mem.alloc_zero(8);
    assert_eq!(call(&s, 293, [pfd, 0, 0, 0, 0, 0]), 0);
    let ends = mem.get(pfd, 8);
    let (pr, pw) = (i32::from_le_bytes(ends[0..4].try_into().unwrap()) as i64, i32::from_le_bytes(ends[4..8].try_into().unwrap()) as i64);
    assert_eq!(fcntl(&s, pr, F_ADD_SEALS, 0), -EPERM);
    assert_eq!(fcntl(&s, pw, F_ADD_SEALS, 0), -EINVAL);
    assert_eq!(fcntl(&s, pw, F_GET_SEALS, 0), -EINVAL);
    assert_eq!(fcntl(&s, fd, 1035, 0), -EINVAL, "the next command number is not a seal command");
    let _ = (ns, fds);
}

#[test]
fn shrink_grow_and_write_seals() {
    env!(ns, fds, mem, s);
    let fd = memfd(&s, &mem, b"g", MFD_ALLOW_SEALING);
    assert_eq!(ftruncate(&s, fd, P), 0);
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, (F_SEAL_SHRINK | F_SEAL_GROW) as u64), 0);
    assert_eq!(ftruncate(&s, fd, P + 1), -EPERM);
    assert_eq!(ftruncate(&s, fd, P - 1), -EPERM);
    assert_eq!(ftruncate(&s, fd, P), 0, "the same size is no change");
    // A write that would grow the file is refused in the 2 MiB step it would grow in (CachyOS).
    let big = alloc::vec![7u8; 2 * P as usize];
    assert_eq!(pwrite(&s, &mem, fd, &big, 0), -EPERM);
    assert_eq!(pwrite(&s, &mem, fd, &[1u8; 10], P - 6), -EPERM);
    assert_eq!(pwrite(&s, &mem, fd, &[1u8; 10], P), -EPERM);
    assert_eq!(pwrite(&s, &mem, fd, &[1u8; 6], P - 6), 6, "up to the end is fine");
    assert_eq!(pwrite(&s, &mem, fd, &[], P + 10), 0, "nothing to write is never refused");
    assert_eq!(fstat(&s, &mem, fd).2, P as i64);
    // O_APPEND under F_SEAL_GROW: every append grows.
    assert_eq!(fcntl(&s, fd, crate::abi::F_SETFL, crate::abi::O_APPEND), 0);
    let one = mem.alloc(b"a");
    assert_eq!(call(&s, 1, [fd as u64, one, 1, 0, 0, 0]), -EPERM);
    assert_eq!(fcntl(&s, fd, crate::abi::F_SETFL, 0), 0);
    // F_SEAL_WRITE: any write is EPERM, truncating stays the business of SHRINK/GROW.
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, F_SEAL_WRITE as u64), 0);
    assert_eq!(pwrite(&s, &mem, fd, b"x", 0), -EPERM);
    assert_eq!(pwrite(&s, &mem, fd, &[], 0), 0);
    let fd2 = memfd(&s, &mem, b"w", MFD_ALLOW_SEALING);
    assert_eq!(fcntl(&s, fd2, F_ADD_SEALS, F_SEAL_FUTURE_WRITE as u64), 0);
    assert_eq!(pwrite(&s, &mem, fd2, b"x", 0), -EPERM);
    assert_eq!(ftruncate(&s, fd2, 3 * P), 0, "FUTURE_WRITE does not stop a resize");
    let mut b = [9u8; 4];
    let f = fds.get(fd2 as usize).unwrap();
    assert_eq!(f.pread(0, &mut b).unwrap(), 4);
    assert_eq!(b, [0; 4]);
    let _ = ns;
}

#[test]
fn grow_seal_steps_end_at_2mib_boundaries() {
    env!(ns, fds, mem, s);
    let mb2 = 2u64 << 20;
    let fd = memfd(&s, &mem, b"steps", MFD_ALLOW_SEALING);
    assert_eq!(ftruncate(&s, fd, mb2 + 5), 0);
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, F_SEAL_GROW as u64), 0);
    // CachyOS: 20 bytes at 2 MiB - 10 write 10 (the step up to the boundary), the next step is refused.
    assert_eq!(pwrite(&s, &mem, fd, &[3u8; 20], mb2 - 10), 10);
    assert_eq!(pwrite(&s, &mem, fd, &[3u8; 20], mb2 - 20), 20);
    let _ = (ns, fds);
}

#[test]
fn write_seals_and_shared_mappings() {
    env!(ns, fds, mem, s);
    let fd = memfd(&s, &mem, b"wl_shm", MFD_ALLOW_SEALING | MFD_CLOEXEC);
    assert_eq!(ftruncate(&s, fd, 2 * P), 0);
    let f = fds.get(fd as usize).unwrap();
    let cache = || f.inode().unwrap().page_cache().unwrap();
    // A writable shared mapping is counted: F_SEAL_WRITE is EBUSY while it exists.
    let a = mmap::check(&f, RW, MAP_SHARED, 0, 2 * P).unwrap();
    assert!(a.shared && a.may_write);
    assert_eq!(cache().writable_maps(), 1);
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, F_SEAL_WRITE as u64), -EBUSY);
    // So is a read-only one of this writable fd (VM_SHARED with VM_MAYWRITE: mprotect could make it writable).
    let ro = mmap::check(&f, 1, MAP_SHARED, 0, P).unwrap();
    assert!(ro.may_write);
    assert_eq!(cache().writable_maps(), 2);
    drop(ro);
    // Private mappings never count.
    let pv = mmap::check(&f, RW, MAP_PRIVATE, 0, P).unwrap();
    assert!(!pv.shared && pv.may_write);
    assert_eq!(cache().writable_maps(), 1);
    // FUTURE_WRITE is allowed with the mapping in place, which keeps writing; new writable ones are EPERM.
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, F_SEAL_FUTURE_WRITE as u64), 0);
    let phys = a.backing.get_page(0, true).unwrap();
    assert!(a.backing.put_page(phys));
    assert!(matches!(mmap::check(&f, RW, MAP_SHARED, 0, P), Err(EPERM)));
    let ro2 = mmap::check(&f, 1, MAP_SHARED, 0, P).unwrap();
    assert!(ro2.shared && !ro2.may_write, "read-only and not VM_MAYWRITE (mprotect PROT_WRITE: EACCES)");
    assert!(mmap::check(&f, RW, MAP_PRIVATE, 0, P).unwrap().may_write, "private copies stay writable");
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, F_SEAL_WRITE as u64), -EBUSY, "the first mapping still may write");
    // Its end (munmap of every part: the FileMap goes) lets the seal in.
    drop(a);
    assert_eq!(cache().writable_maps(), 0);
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, F_SEAL_WRITE as u64), 0);
    assert_eq!(fcntl(&s, fd, F_GET_SEALS, 0), (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) as i64);
    // Under F_SEAL_WRITE: the compositor's read-only shared mapping works (CachyOS; 6.6 refused it).
    let comp = mmap::check(&f, 1, MAP_SHARED, 0, 2 * P).unwrap();
    assert!(!comp.may_write);
    assert!(matches!(mmap::check(&f, RW, MAP_SHARED, 0, P), Err(EPERM)));
    assert_eq!(cache().writable_maps(), 0);
    let _ = (ns, ro2, pv);
}

#[test]
fn exec_seal_brings_write_seals() {
    env!(ns, fds, mem, s);
    let fd = memfd(&s, &mem, b"x", MFD_ALLOW_SEALING);
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, F_SEAL_EXEC as u64), 0);
    // CachyOS answered 0x3e for the same call: W^X from the start.
    assert_eq!(fcntl(&s, fd, F_GET_SEALS, 0), 0x3e);
    let i = fds.get(fd as usize).unwrap().inode().unwrap().clone();
    assert_eq!(check_mode_change(&i, 0o776), Err(EPERM));
    assert_eq!(check_mode_change(&i, 0o757), Ok(()), "only the exec bits are sealed");
    // MFD_NOEXEC_SEAL: 0666 with F_SEAL_EXEC; adding seals stays possible and brings nothing along.
    let n = memfd(&s, &mem, b"n", MFD_NOEXEC_SEAL);
    assert_eq!(fcntl(&s, n, F_ADD_SEALS, (F_SEAL_EXEC | F_SEAL_SHRINK) as u64), 0);
    assert_eq!(fcntl(&s, n, F_GET_SEALS, 0), (F_SEAL_EXEC | F_SEAL_SHRINK) as i64);
    let ni = fds.get(n as usize).unwrap().inode().unwrap().clone();
    assert_eq!(check_mode_change(&ni, 0o766), Err(EPERM));
    let plain = memfd(&s, &mem, b"p", MFD_EXEC);
    let pi = fds.get(plain as usize).unwrap().inode().unwrap().clone();
    assert_eq!(check_mode_change(&pi, 0o666), Ok(()));
    // FS-1b: fchmod goes through the same rule (shmem_setattr); the mode stays as it was.
    let chmod = |fd: i64, mode: u64| crate::sys_attr::fchmod(&s, [fd as u64, mode, 0, 0, 0, 0]);
    let before = i.meta.mode();
    assert_eq!(chmod(fd, 0o776), -EPERM);
    assert_eq!(i.meta.mode(), before);
    assert_eq!(chmod(n, 0o766), -EPERM);
    assert_eq!(chmod(n, 0o600), 0, "the other bits may change");
    assert_eq!(chmod(plain, 0o700), 0);
    let _ = ns;
}

#[test]
fn charged_like_tmpfs_and_freed_at_last_close() {
    env!(ns, fds, mem, s);
    let fd = memfd(&s, &mem, b"pool", 0);
    assert_eq!(ftruncate(&s, fd, 10 * P), 0);
    let f = fds.get(fd as usize).unwrap();
    let inode = Arc::downgrade(f.inode().unwrap());
    assert_eq!(f.inode().unwrap().page_cache().unwrap().charged(), 0, "FS-14: a growing ftruncate charges nothing");
    assert!(Arc::ptr_eq(&f.path.as_ref().unwrap().dentry.sb.pool, &shm_mount().sb.pool), "the internal tmpfs's pool");
    drop(f);
    assert_eq!(call(&s, 3, [fd as u64, 0, 0, 0, 0, 0]), 0);
    assert!(inode.upgrade().is_none(), "the last close ends a memfd (nlink 0)");
    let _ = ns;
}

#[test]
fn names_as_d_path_shows_them() {
    env!(ns, fds, mem, s);
    let fd = memfd(&s, &mem, b"wayland-shm", 0);
    let odd = memfd(&s, &mem, b"a/b c", 0);
    let f = fds.get(fd as usize).unwrap();
    let path = f.path.clone().unwrap();
    assert_eq!(ns.d_path(&ns.root_path(), &path), b"/memfd:wayland-shm (deleted)");
    let g = fds.get(odd as usize).unwrap();
    assert_eq!(ns.d_path(&ns.root_path(), g.path.as_ref().unwrap()), b"/memfd:a/b c (deleted)");
    // linkat cannot give it a name: its inode is not linkable (and it lives on another mount: EXDEV first).
    assert!(!f.inode().unwrap().linkable.load(core::sync::atomic::Ordering::Relaxed));
}

/// A stand-in for vfs-sys's `/proc/<pid>/fd` over one table (the walk and the magic link are vfs's).
struct FdDir(Arc<FdTable>, Arc<MountNs>);
struct FdLink(Arc<FdTable>, usize);

impl MagicLink for FdLink {
    fn target(&self) -> Result<Path, Errno> {
        let f = self.0.get(self.1).map_err(|_| ENOENT)?;
        f.path.clone().ok_or(ENXIO)
    }
}

impl DirSource for FdDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok(self
            .0
            .open_fds()
            .iter()
            .map(|(n, f)| {
                let text = match &f.path {
                    Some(p) => self.1.d_path(&self.1.root_path(), p),
                    None => f.file.link_name().unwrap_or_default().into_bytes(),
                };
                let perm = if f.can_read() { 0o500 } else { 0 } | if f.can_write() { 0o300 } else { 0 };
                SynthEntry::magic(&format!("{n}"), perm, text, Arc::new(FdLink(self.0.clone(), *n))).cookie(*n as u64 + 1)
            })
            .collect())
    }
}

#[test]
fn proc_fd_magic_link_opens_the_memfd_again() {
    let (ns, _) = program_view();
    crate::fs2_tests::fake_system(&ns);
    let fds = Arc::new(FdTable::new());
    crate::devfs::attach(&ns, "/proc/fdt", Arc::new(FdDir(fds.clone(), ns.clone()))).unwrap();
    let mem = Mem::new();
    let s = Sys { mem: &mem, fds: &fds, ns: &ns, cred: USER, subject: SUBJECT };
    let fd = memfd(&s, &mem, b"reopen", MFD_ALLOW_SEALING);
    assert_eq!(pwrite(&s, &mem, fd, b"pixels", 0), 6);
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, F_SEAL_SHRINK as u64), 0);
    let link = format!("/proc/fdt/{fd}");
    // readlink: the d_path text; lstat: a link with the open file's access bits and size 64.
    let lp = mem.cstr(link.as_bytes());
    let buf = mem.alloc_zero(256);
    let n = call(&s, 89, [lp, buf, 256, 0, 0, 0]);
    assert_eq!(String::from_utf8(mem.get(buf, n as usize)).unwrap(), "/memfd:reopen (deleted)");
    let st = mem.alloc_zero(144);
    assert_eq!(call(&s, 6, [lp, st, 0, 0, 0, 0]), 0);
    let b = mem.get(st, 144);
    assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), uapi::S_IFLNK | 0o700);
    assert_eq!(i64::from_le_bytes(b[48..56].try_into().unwrap()), 64);
    // Opening through it: a new open file of the same memfd (read-only here), seals and data shared.
    let r = call(&s, 2, [lp, 0, 0, 0, 0, 0]);
    assert!(r > fd, "opened: {r}");
    let (ma, mb) = (fstat(&s, &mem, fd), fstat(&s, &mem, r));
    assert_eq!(ma, mb);
    assert_eq!(fcntl(&s, r, F_GET_SEALS, 0), F_SEAL_SHRINK as i64);
    assert_eq!(fcntl(&s, r, F_ADD_SEALS, F_SEAL_GROW as u64), -EPERM, "not open for writing");
    assert_eq!(fcntl(&s, r, crate::abi::F_GETFL, 0), 0o100000);
    let rb = mem.alloc_zero(16);
    assert_eq!(call(&s, 0, [r as u64, rb, 16, 0, 0, 0]), 6);
    assert_eq!(mem.get(rb, 6), b"pixels");
    // The read-only reopen maps shared without counting (the compositor's side): F_SEAL_WRITE still fits.
    let rf = fds.get(r as usize).unwrap();
    let m = mmap::check(&rf, 1, MAP_SHARED, 0, P).unwrap();
    assert!(!m.may_write);
    assert_eq!(rf.inode().unwrap().page_cache().unwrap().writable_maps(), 0);
    // O_TRUNC through the link meets F_SEAL_SHRINK; O_NOFOLLOW meets a link; a trailing slash a non-directory.
    assert_eq!(call(&s, 2, [lp, 0o1002, 0, 0, 0, 0]), -EPERM);
    assert_eq!(call(&s, 2, [lp, 0o400000, 0, 0, 0, 0]), -ELOOP);
    let slash = mem.cstr(format!("{link}/").as_bytes());
    assert_eq!(call(&s, 2, [slash, 0, 0, 0, 0, 0]), -ENOTDIR);
    let below = mem.cstr(format!("{link}/x").as_bytes());
    assert_eq!(call(&s, 2, [below, 0, 0, 0, 0, 0]), -ENOTDIR);
    // A link to an fd that was closed meanwhile: the lookup finds no entry.
    assert_eq!(call(&s, 3, [fd as u64, 0, 0, 0, 0, 0]), 0);
    assert_eq!(call(&s, 2, [lp, 0, 0, 0, 0, 0]), -ENOENT);
    // The reopened file keeps the memfd alive with its data.
    assert_eq!(call(&s, 17, [r as u64, rb, 16, 0, 0, 0]), 6);
    drop(m);
}

// ---- fixer: fallocate, refusal logging, races, hostile sizes ------------------------------------------------

const NR_FALLOCATE: usize = 285;
const KEEP: u64 = crate::abi::FALLOC_FL_KEEP_SIZE as u64;
const PUNCH: u64 = crate::abi::FALLOC_FL_PUNCH_HOLE as u64;

fn fallocate(s: &Sys<'_>, fd: i64, mode: u64, off: u64, len: u64) -> i64 {
    call(s, NR_FALLOCATE, [fd as u64, mode, off, len, 0, 0])
}

#[test]
fn fallocate_on_a_memfd_charges_punches_and_obeys_the_seals() {
    env!(ns, fds, mem, s);
    let fd = memfd(&s, &mem, b"pool", MFD_ALLOW_SEALING);
    let f = fds.get(fd as usize).unwrap();
    let cache = || f.inode().unwrap().page_cache().unwrap();
    // FS-14: mode 0 allocates and charges the range's pages and grows the size; KEEP_SIZE past the end
    // allocates them too and keeps the size (Linux holds them beyond i_size).
    assert_eq!(fallocate(&s, fd, 0, 0, 4 * P), 0);
    assert_eq!((fstat(&s, &mem, fd).2, cache().charged()), (4 * P as i64, 4));
    assert_eq!(fallocate(&s, fd, KEEP, 0, 8 * P), 0);
    assert_eq!((fstat(&s, &mem, fd).2, cache().charged()), (4 * P as i64, 8), "KEEP_SIZE past the end holds its pages");
    assert_eq!(fallocate(&s, fd, 0, P, P), 0, "inside the file: allocated already");
    assert_eq!(cache().charged(), 8);
    // Punch: an unmapped page wholly inside leaves the cache, a partly covered one is zeroed in its range.
    assert_eq!(pwrite(&s, &mem, fd, &[b'p'; 4 * P as usize], 0), 4 * P as i64);
    assert_eq!(cache().pages(), 8);
    assert_eq!(fallocate(&s, fd, PUNCH | KEEP, 0, P + 10), 0);
    assert!(cache().cached(0).is_none(), "the whole page is a hole again");
    let mut b = [9u8; 12];
    assert_eq!(f.pread(P, &mut b).unwrap(), 12);
    assert_eq!(&b, b"\0\0\0\0\0\0\0\0\0\0pp");
    // A page a shared mapping pins stays (its frame is mapped) and is zeroed in place: the mapping sees the hole.
    let m = mmap::check(&f, RW, MAP_SHARED, 0, 4 * P).unwrap();
    let pinned = m.backing.get_page(2, false).unwrap();
    let before = cache().cached(2).unwrap().ident();
    assert_eq!(fallocate(&s, fd, PUNCH | KEEP, 2 * P, P), 0);
    let page = cache().cached(2).expect("a pinned page is not dropped");
    assert_eq!(page.ident(), before, "the same frame the mapping maps");
    let mut z = [9u8; 8];
    page.read(0, &mut z);
    assert_eq!(z, [0; 8]);
    assert!(m.backing.put_page(pinned));
    assert_eq!((fstat(&s, &mem, fd).2, cache().charged()), (4 * P as i64, 7), "a punch frees the hole's pages, a pinned one stays");
    drop(m);
    // GROW: growth past the end is EPERM, also with KEEP_SIZE; inside is fine. SHRINK does not stop a punch.
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, (F_SEAL_GROW | F_SEAL_SHRINK) as u64), 0);
    assert_eq!(fallocate(&s, fd, 0, 0, 4 * P), 0);
    assert_eq!(fallocate(&s, fd, 0, 3 * P, 2 * P), -EPERM);
    assert_eq!(fallocate(&s, fd, KEEP, 3 * P, 2 * P), -EPERM);
    assert_eq!(fallocate(&s, fd, PUNCH | KEEP, 0, P), 0);
    // WRITE / FUTURE_WRITE: a punch is EPERM, allocation inside the file is not.
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, F_SEAL_FUTURE_WRITE as u64), 0);
    assert_eq!(fallocate(&s, fd, PUNCH | KEEP, 0, P), -EPERM);
    assert_eq!(fallocate(&s, fd, 0, 0, P), 0);
    assert_eq!(cache().charged(), 8);
    let _ = ns;
}

#[test]
fn fallocate_on_a_sized_tmpfs_is_all_or_nothing() {
    use crate::abi::FALLOC_FL_KEEP_SIZE as K;
    let pool = Arc::new(crate::pool::Pool::new("fs14-sized"));
    pool.set_size(Some(16));
    let c = crate::pagecache::PageCache::tmpfs(pool.clone());
    assert_eq!(c.fallocate(0, 0, 17 * P), Err(ENOSPC), "the range alone larger than the mount");
    assert_eq!(c.fallocate(K, 0, 17 * P), Err(ENOSPC));
    assert_eq!(c.fallocate(0, 0, 8 * P), Ok(()));
    assert_eq!((pool.used(), c.size(), c.pages()), (8, 8 * P, 8));
    // Pages 8..16 fit, the 17th does not: the pages this call added go again (shmem_undo_range).
    assert_eq!(c.fallocate(0, 8 * P, 9 * P), Err(ENOSPC));
    assert_eq!((pool.used(), c.size(), c.pages()), (8, 8 * P, 8));
    // KEEP_SIZE holds its pages past the end; a truncate drops them with the rest past the new end.
    assert_eq!(c.fallocate(K, 8 * P, 8 * P), Ok(()));
    assert_eq!((pool.used(), c.size()), (16, 8 * P));
    assert_eq!(c.fallocate(K, 16 * P, P), Err(ENOSPC));
    c.truncate(8 * P).unwrap();
    assert_eq!(pool.used(), 8);
    // A write past the mount's size: what fits, then ENOSPC (shmem_write_begin).
    assert_eq!(c.write_at(8 * P, &[1u8; 9 * P as usize]), Ok(8 * P as usize));
    assert_eq!(c.write_at(16 * P, b"x"), Err(ENOSPC));
    assert_eq!((pool.used(), c.size()), (16, 16 * P));
    // A growing truncate costs nothing even past the mount's size (shmem_setattr writes i_size only).
    c.truncate(1 << 30).unwrap();
    assert_eq!(pool.used(), 16);
    drop(c);
    assert_eq!(pool.used(), 0, "the last reference gives every page's charge back");
}

#[test]
fn fallocate_on_a_size_charged_object_is_all_or_nothing() {
    use crate::abi::FALLOC_FL_KEEP_SIZE as K;
    let pool = Arc::new(crate::pool::Pool::new("fs10-sized"));
    pool.set_size(Some(16));
    let c = crate::pagecache::PageCache::tmpfs_accounted(pool.clone());
    // The range alone larger than the mount: ENOSPC before anything (shmem's max_blocks check).
    assert_eq!(c.fallocate(0, 0, 17 * P), Err(ENOSPC));
    assert_eq!(c.fallocate(K, 0, 17 * P), Err(ENOSPC));
    assert_eq!((pool.used(), c.size()), (0, 0));
    assert_eq!(c.fallocate(0, 0, 8 * P), Ok(()));
    assert_eq!((pool.used(), c.size()), (8, 8 * P));
    // Would fit as a range but not with what is held: ENOSPC, nothing changed.
    assert_eq!(c.fallocate(0, 8 * P, 9 * P), Err(ENOSPC));
    assert_eq!((pool.used(), c.size()), (8, 8 * P));
    // KEEP_SIZE asks whether the pages would fit and holds nothing.
    assert_eq!(c.fallocate(K, 8 * P, 8 * P), Ok(()));
    assert_eq!(c.fallocate(K, 8 * P, 9 * P), Err(ENOSPC));
    assert_eq!((pool.used(), c.size()), (8, 8 * P));
    // Modes shmem does not have.
    assert_eq!(c.fallocate(crate::abi::FALLOC_FL_ZERO_RANGE, 0, P), Err(EOPNOTSUPP));
    assert_eq!(c.fallocate(crate::abi::FALLOC_FL_COLLAPSE_RANGE, 0, P), Err(EOPNOTSUPP));
    drop(c);
    assert_eq!(pool.used(), 0, "the last reference gives the charge back");
}

#[test]
fn seal_refusals_are_counted() {
    env!(ns, fds, mem, s);
    let counts = || (REFUSED_WRITE.get(), REFUSED_TRUNCATE.get(), REFUSED_MAP.get(), REFUSED_ADD.get(), REFUSED_FALLOCATE.get());
    let fd = memfd(&s, &mem, b"refused", MFD_ALLOW_SEALING);
    assert_eq!(ftruncate(&s, fd, P), 0);
    let f = fds.get(fd as usize).unwrap();
    let m = mmap::check(&f, RW, MAP_SHARED, 0, P).unwrap();
    let c0 = counts();
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, F_SEAL_WRITE as u64), -EBUSY);
    drop(m);
    assert_eq!(fcntl(&s, fd, F_ADD_SEALS, (F_SEAL_WRITE | F_SEAL_GROW) as u64), 0);
    assert_eq!(pwrite(&s, &mem, fd, b"x", 0), -EPERM);
    assert_eq!(ftruncate(&s, fd, 2 * P), -EPERM);
    assert!(matches!(mmap::check(&f, RW, MAP_SHARED, 0, P), Err(EPERM)));
    assert_eq!(fallocate(&s, fd, PUNCH | KEEP, 0, P), -EPERM);
    let c1 = counts();
    // Other tests run in parallel: each counter moved by at least this test's one refusal.
    assert!(c1.0 > c0.0 && c1.1 > c0.1 && c1.2 > c0.2 && c1.3 > c0.3 && c1.4 > c0.4, "{c0:?} -> {c1:?}");
    let _ = ns;
}

/// F_ADD_SEALS(F_SEAL_WRITE) against threads that keep making and dropping writable shared mappings: a counted
/// mapping and the seal never exist together (each thread looks at the seals while it holds its mapping), the
/// seal is EBUSY only while one is held and comes once none is, and afterwards every new one is EPERM.
#[test]
fn seal_write_and_new_writable_mappings_never_pass_each_other() {
    use crate::pagecache::PageCache;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Barrier;
    let busy_total = AtomicU64::new(0);
    for round in 0..100u64 {
        let c = Arc::new(PageCache::tmpfs(Arc::new(crate::pool::Pool::new("fs10-race"))));
        c.set_initial_seals(0);
        let mappers = 3;
        let barrier = Arc::new(Barrier::new(mappers + 1));
        let bad = Arc::new(AtomicU64::new(0));
        let hs: Vec<_> = (0..mappers)
            .map(|_| {
                let (c, b, bad) = (c.clone(), barrier.clone(), bad.clone());
                std::thread::spawn(move || {
                    b.wait();
                    for _ in 0..5000 {
                        match c.map_shared(true, true) {
                            Ok(true) => {
                                if c.seals().unwrap() & F_SEAL_WRITE != 0 {
                                    bad.fetch_add(1, Ordering::SeqCst);
                                }
                                c.unmap_writable();
                            }
                            Err(EPERM) => return,
                            other => panic!("map_shared: {other:?}"),
                        }
                    }
                })
            })
            .collect();
        barrier.wait();
        for _ in 0..round * 50 {
            core::hint::spin_loop();
        }
        loop {
            match c.add_seals(F_SEAL_WRITE, false) {
                Ok(_) => break,
                Err(EBUSY) => {
                    busy_total.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => panic!("add_seals: {e}"),
            }
        }
        assert!(matches!(c.map_shared(true, true), Err(EPERM)));
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(bad.load(Ordering::SeqCst), 0, "a writable mapping was counted while F_SEAL_WRITE was in place");
        assert_eq!(c.writable_maps(), 0);
    }
    std::println!("seal race: {} EBUSY answers", busy_total.load(Ordering::Relaxed));
}

/// F_ADD_SEALS(F_SEAL_WRITE) against a thread that keeps writing through the syscall path: what the file holds
/// right after the seal is granted is what it holds at the end (no write lands after it).
#[test]
fn seal_write_and_writes_never_pass_each_other() {
    use std::sync::Barrier;
    for _round in 0..200 {
        env!(ns, fds, mem, s);
        let fd = memfd(&s, &mem, b"wrace", MFD_ALLOW_SEALING);
        assert_eq!(ftruncate(&s, fd, P), 0);
        let f = fds.get(fd as usize).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let (f2, b2) = (f.clone(), barrier.clone());
        let h = std::thread::spawn(move || {
            b2.wait();
            for i in 1u64..=2000 {
                match f2.seal_clip(0, 8).and_then(|_| f2.pwrite(0, &i.to_le_bytes())) {
                    Ok(8) => {}
                    Err(EPERM) => return,
                    other => panic!("write: {other:?}"),
                }
            }
        });
        barrier.wait();
        while fcntl(&s, fd, F_ADD_SEALS, F_SEAL_WRITE as u64) != 0 {}
        let mut at_seal = [0u8; 8];
        f.pread(0, &mut at_seal).unwrap();
        h.join().unwrap();
        let mut end = [0u8; 8];
        f.pread(0, &mut end).unwrap();
        assert_eq!(at_seal, end, "a write landed after F_SEAL_WRITE was granted");
        let _ = ns;
    }
}
