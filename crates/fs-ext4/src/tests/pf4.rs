//! PF-4 on ext4: a read(2)/pread(2) of an ext4 file into a program buffer that faults part way returns the
//! bytes before the first faulting page (the partial-fault check of crates/vfs pf4_tests, through ext4's
//! `InodeOps::read_cache`; the first read comes from the disk).

use super::util::*;
use crate::mount;
use core::cell::{Cell, RefCell};
use std::sync::Arc;
use std::vec;
use std::vec::Vec;
use vfs::abi::O_RDONLY;
use vfs::fd::{FdTable, FsState};
use vfs::inode::Cred;
use vfs::mount_ns::MountNs;
use vfs::ops::Ctx;
use vfs::sys::{self, Sys, UserMem};

const P: u64 = 4096;
const BASE: u64 = 0x2000_0000;
const NR_READ: usize = 0;
const NR_PREAD: usize = 17;

/// Memory standing in for a program's: writable from `BASE` below `bad`, a fault from there on.
struct FaultMem {
    bytes: RefCell<Vec<u8>>,
    bad: Cell<u64>,
    direct: Cell<bool>,
}

impl UserMem for FaultMem {
    fn read(&self, _addr: u64, _buf: &mut [u8]) -> bool {
        false
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        let Some(end) = addr.checked_add(data.len() as u64) else { return false };
        let mut m = self.bytes.borrow_mut();
        if addr < BASE || end > self.bad.get() || end > BASE + m.len() as u64 {
            return false;
        }
        let o = (addr - BASE) as usize;
        m[o..o + data.len()].copy_from_slice(data);
        true
    }
    fn direct_page_copy(&self) -> bool {
        self.direct.get()
    }
    fn read_cstr(&self, _addr: u64, _max: usize) -> Result<Vec<u8>, vfs::Errno> {
        Err(uapi::errno::EFAULT)
    }
}

/// PF-4 through this file system's `read_cache`: pread and read of `path` (holding `data`) into memory that
/// faults from a chosen page end exactly at the first page that faults (Linux: the bytes before it, EFAULT
/// when none), every byte checked and nothing written past; the first read comes from the disk (cold cache).
fn faulting_reads(ns: MountNs, path: &[u8], data: &[u8]) {
    let ns = Arc::new(ns);
    let fds = FdTable::new();
    let f = Ctx::kernel(&ns).open(None, path, O_RDONLY, 0).unwrap();
    assert!(f.file.read_cache().is_some(), "a regular file reads through its page cache");
    let fd = fds.install(f).unwrap() as u64;
    let fs = FsState::new();
    let mem = FaultMem { bytes: RefCell::new(vec![0xEE; 5 * P as usize]), bad: Cell::new(BASE), direct: Cell::new(false) };
    let s = Sys { mem: &mem, fds: &fds, fs: &fs, ns: &ns, cred: Cred::ROOT, subject: policy::Subject::KERNEL };
    let size = data.len() as u64;
    for &off in &[3 * P / 2, 0, 1, 4095, 4096, 3 * P] {
        for &mis in &[0u64, 1, 2048, 4095] {
            for (bad_pages, direct) in (0..=4u64).flat_map(|b| [(b, false), (b, true)]) {
                mem.direct.set(direct);
                mem.bytes.borrow_mut().fill(0xEE);
                mem.bad.set(BASE + bad_pages * P);
                let r = sys::call(&s, NR_PREAD, [fd, BASE + mis, 3 * P, off, 0, 0]);
                let want = (size - off).min(3 * P).min((bad_pages * P).saturating_sub(mis));
                let expect = if want == 0 { -(uapi::errno::EFAULT as i64) } else { want as i64 };
                assert_eq!(r, expect, "pread at {off} into BASE+{mis}, {bad_pages} pages writable (direct {direct})");
                let m = mem.bytes.borrow();
                let (o, n) = (mis as usize, r.max(0) as usize);
                assert!(m[o..o + n] == data[off as usize..off as usize + n], "a byte differs (pread at {off} into BASE+{mis})");
                assert!(m[o + n..].iter().all(|&b| b == 0xEE), "bytes written past the {n} returned");
            }
        }
    }
    mem.direct.set(false);
    mem.bad.set(BASE + 2 * P);
    let r = sys::call(&s, NR_READ, [fd, BASE + 10, 3 * P, 0, 0, 0]);
    assert_eq!(r, 2 * P as i64 - 10, "read stops at the fault");
    mem.bad.set(BASE + 5 * P);
    let r2 = sys::call(&s, NR_READ, [fd, BASE, 4 * P, 0, 0, 0]);
    assert_eq!(r2, size as i64 - r, "the next read goes on where the first stopped");
    assert!(mem.bytes.borrow()[..r2 as usize] == data[r as usize..], "the next read's bytes differ");
    fds.close_all();
}

#[test]
fn a_fault_ends_an_ext4_read_at_the_first_page_that_faults() {
    let s = Scratch::new();
    let tree = s.path("pf4tree");
    std::fs::create_dir_all(&tree).unwrap();
    let data: Vec<u8> = (0..3 * P + 100).map(|o| (o % 251) as u8 + 1).collect();
    std::fs::write(tree.join("f"), &data).unwrap();
    let img = mkfs(&s, "16M", &[], Some(&tree));
    let ns = MountNs::new_empty();
    mount::mount_dev(Arc::new(crate::MemDev::new(img)), None, &ns, "/data", opts()).unwrap();
    faulting_reads(ns, b"/data/f", &data);
}
