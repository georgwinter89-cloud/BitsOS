//! FS-10: `vm.memfd_noexec` (linux-6.6/kernel/pid_sysctl.h, `check_sysctl_memfd_noexec`). Its own test binary:
//! the value is system-wide and would change what the unit tests' memfds get.

use std::cell::RefCell;
use std::sync::Arc;
use uapi::errno::*;
use vfs::memfd::*;
use vfs::sys::{call, Sys, UserMem};
use vfs::{Cred, Errno, FdTable, MountNs};

const BASE: u64 = 0x1000_0000;

struct Mem(RefCell<Vec<u8>>);

impl Mem {
    fn cstr(&self, s: &[u8]) -> u64 {
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
fn noexec_scope_decides_the_default_and_can_require_noexec() {
    let ns = Arc::new(MountNs::new_empty());
    let fds = FdTable::new();
    let mem = Mem(RefCell::new(Vec::new()));
    let s = Sys { mem: &mem, fds: &fds, ns: &ns, cred: Cred { uid: 1000, gid: 1000 }, subject: policy::Subject::KERNEL };
    let make = |flags: u32| call(&s, 319, [mem.cstr(b"scope"), flags as u64, 0, 0, 0, 0]);
    let seals = |fd: i64| call(&s, 72, [fd as u64, F_GET_SEALS, 0, 0, 0, 0]);
    assert_eq!(noexec_scope(), 0);
    let fd = make(0);
    assert_eq!(seals(fd), F_SEAL_SEAL as i64, "scope 0: executable by default");
    assert_eq!(set_noexec_scope(1), Ok(()));
    let fd = make(0);
    assert_eq!(seals(fd), F_SEAL_EXEC as i64, "scope 1: MFD_NOEXEC_SEAL by default");
    let fd = make(MFD_EXEC);
    assert_eq!(seals(fd), F_SEAL_SEAL as i64, "asking for MFD_EXEC still works at 1");
    assert_eq!(set_noexec_scope(2), Ok(()));
    assert_eq!(make(MFD_EXEC), -EACCES);
    let fd = make(MFD_ALLOW_SEALING);
    assert_eq!(seals(fd), F_SEAL_EXEC as i64, "without either flag scope 2 picks MFD_NOEXEC_SEAL, which passes");
    assert!(make(MFD_NOEXEC_SEAL) >= 0);
    assert_eq!(set_noexec_scope(3), Err(EINVAL));
    assert_eq!(set_noexec_scope(0), Ok(()));
    assert!(make(MFD_EXEC) >= 0);
}
