//! FS-10 fixer, FS-14: hostile sizes on a memfd against a small system pool. Its own test binary: the pool's
//! limit is system-wide and would change what the unit tests' files get.
//!
//! FS-14 follows Linux (memfds are VM_NORESERVE): ftruncate to any size up to MAX_LFS_FILESIZE succeeds and
//! charges nothing; a page is charged when it is allocated (write, fault, fallocate), and a refusal is ENOSPC
//! (a fault: SIGBUS). fallocate is all or nothing (the pages it added go again).

use std::cell::RefCell;
use std::sync::Arc;
use uapi::errno::*;
use vfs::memfd::*;
use vfs::pool::{set_system_limit, system_charged, system_used};
use vfs::sys::{call, Sys, UserMem};
use vfs::{Cred, Errno, FdTable, MountNs};

const BASE: u64 = 0x1000_0000;
const P: u64 = 4096;

struct Mem(RefCell<Vec<u8>>);

impl Mem {
    fn put(&self, s: &[u8]) -> u64 {
        let mut b = self.0.borrow_mut();
        let at = b.len() as u64;
        b.extend_from_slice(s);
        b.push(0);
        BASE + at
    }
}

impl UserMem for Mem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let b = self.0.borrow();
        let Some(off) = addr.checked_sub(BASE).map(|o| o as usize) else { return false };
        match b.get(off..off + buf.len()) {
            Some(src) => {
                buf.copy_from_slice(src);
                true
            }
            None => false,
        }
    }
    fn write(&self, _addr: u64, _data: &[u8]) -> bool {
        false
    }
    fn read_cstr(&self, addr: u64, max: usize) -> Result<Vec<u8>, Errno> {
        let b = self.0.borrow();
        let off = addr.checked_sub(BASE).ok_or(EFAULT)? as usize;
        let avail = b.get(off..).ok_or(EFAULT)?;
        match avail.iter().take(max).position(|&c| c == 0) {
            Some(n) => Ok(avail[..n].to_vec()),
            None if avail.len() >= max => Err(ENAMETOOLONG),
            None => Err(EFAULT),
        }
    }
}

#[test]
fn hostile_sizes_are_refused_at_the_call_and_leak_nothing() {
    let ns = Arc::new(MountNs::new_empty());
    let fds = FdTable::new();
    let mem = Mem(RefCell::new(Vec::new()));
    let s = Sys { mem: &mem, fds: &fds, ns: &ns, cred: Cred { uid: 1000, gid: 1000 }, subject: policy::Subject::KERNEL };
    let data0 = system_charged();
    let fd = call(&s, 319, [mem.put(b"hostile"), (MFD_ALLOW_SEALING | MFD_CLOEXEC) as u64, 0, 0, 0, 0]);
    assert!(fd >= 0);
    let fdu = fd as u64;
    // 32 pages of room for data from here on.
    set_system_limit(Some(system_used() + 32));
    let charged = || fds.get(fd as usize).unwrap().inode().unwrap().page_cache().unwrap().charged();
    let size = || fds.get(fd as usize).unwrap().inode().unwrap().size();
    let ftruncate = |len: u64| call(&s, 77, [fdu, len, 0, 0, 0, 0]);
    let fallocate = |mode: u64, off: u64, len: u64| call(&s, 285, [fdu, mode, off, len, 0, 0]);
    let pwrite = |data: &[u8], at: u64| call(&s, 18, [fdu, mem.put(data), data.len() as u64, at, 0, 0]);

    // ftruncate: any size up to MAX_LFS_FILESIZE is fine and charges nothing (EINVAL for a negative one).
    assert_eq!(ftruncate(1 << 62), 0);
    assert_eq!(ftruncate(i64::MAX as u64), 0);
    assert_eq!(ftruncate(u64::MAX), -EINVAL);
    assert_eq!(ftruncate(33 * P), 0, "one page more than the pool has");
    assert_eq!((size(), charged(), system_charged()), (33 * P, 0, data0));
    assert_eq!(ftruncate(0), 0);
    // fallocate: ENOSPC (shmem's accounting answer) once the pool is used up, also with KEEP_SIZE, and the pages
    // it allocated go again; the end past MAX_LFS_FILESIZE EFBIG.
    assert_eq!(fallocate(0, 0, 1 << 40), -ENOSPC);
    assert_eq!(fallocate(1, 0, 1 << 40), -ENOSPC);
    assert_eq!(fallocate(0, i64::MAX as u64, 1), -EFBIG);
    assert_eq!((size(), charged(), system_charged()), (0, 0, data0));
    // What fits is taken whole; the rest of the pool stays usable and is given back.
    assert_eq!(fallocate(0, 0, 30 * P), 0);
    assert_eq!((size(), charged()), (30 * P, 30));
    assert_eq!(fallocate(0, 0, 33 * P), -ENOSPC, "all or nothing: no partial growth");
    assert_eq!((size(), charged()), (30 * P, 30));
    assert_eq!(fallocate(1, 30 * P, 2 * P), 0, "KEEP_SIZE: the pages past the end are held");
    assert_eq!((size(), charged()), (30 * P, 32));
    // A write past the pool's end is short at the last page that fits, then ENOSPC.
    let big = vec![7u8; 4 * P as usize];
    assert_eq!(pwrite(&big, 30 * P), 2 * P as i64);
    assert_eq!(pwrite(b"x", 32 * P), -ENOSPC);
    assert_eq!((size(), charged()), (32 * P, 32));
    // Under F_SEAL_GROW a growing write is EPERM before any charge is asked for.
    assert_eq!(ftruncate(16 * P), 0);
    assert_eq!(charged(), 16);
    assert_eq!(call(&s, 72, [fdu, F_ADD_SEALS, F_SEAL_GROW as u64, 0, 0, 0]), 0);
    assert_eq!(pwrite(&big, 15 * P), -EPERM);
    assert_eq!(fallocate(0, 0, 17 * P), -EPERM);
    assert_eq!((size(), charged()), (16 * P, 16));
    // The last close gives every page back.
    assert_eq!(call(&s, 3, [fdu, 0, 0, 0, 0, 0]), 0);
    assert_eq!(system_charged(), data0, "no charge leaked");
    set_system_limit(None);
}
