//! A program's memory for tests (host tests and ktests drive the ioctls without a process): addresses from
//! `BASE` index one buffer; anything outside it faults (EFAULT paths).

use alloc::vec;
use alloc::vec::Vec;
use ksync::SpinLock;
use vfs::sys::UserMem;
use vfs::Errno;

pub const BASE: u64 = 0x10_0000;

pub struct Arena(SpinLock<Vec<u8>>);

impl Arena {
    pub fn new(size: usize) -> Arena {
        Arena(SpinLock::named("snd.ktest_mem", vec![0; size]))
    }
    pub fn set(&self, addr: u64, data: &[u8]) {
        let o = (addr - BASE) as usize;
        self.0.lock()[o..o + data.len()].copy_from_slice(data);
    }
    pub fn get(&self, addr: u64, len: usize) -> Vec<u8> {
        let o = (addr - BASE) as usize;
        self.0.lock()[o..o + len].to_vec()
    }
    fn range(&self, addr: u64, len: usize) -> Option<usize> {
        let o = addr.checked_sub(BASE)? as usize;
        (o.checked_add(len)? <= self.0.lock().len()).then_some(o)
    }
}

impl UserMem for Arena {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        match self.range(addr, buf.len()) {
            Some(o) => {
                buf.copy_from_slice(&self.0.lock()[o..o + buf.len()]);
                true
            }
            None => false,
        }
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        match self.range(addr, data.len()) {
            Some(o) => {
                self.0.lock()[o..o + data.len()].copy_from_slice(data);
                true
            }
            None => false,
        }
    }
    fn read_cstr(&self, _addr: u64, _max: usize) -> Result<Vec<u8>, Errno> {
        Err(uapi::errno::EFAULT)
    }
    fn interrupted(&self) -> bool {
        false
    }
}
