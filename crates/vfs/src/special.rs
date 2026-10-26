//! Nodes mknod makes that are neither files nor FIFOs (FS-3): socket names and device numbers.
//!
//! A socket name (S_IFSOCK) is only a name until NW-5's AF_UNIX bind gives it a listener; opening it answers
//! ENXIO (linux-6.6/net/socket.c `bad_sock_fops`, `sock_no_open`). A character or block device node made by
//! number (root only: CAP_MKNOD) opens the device the kernel registered under that number, as Linux's
//! `chrdev_open`/`blkdev_open` find the driver by number; without one the open answers ENXIO. Every device
//! node the kernel makes (`MountNs::create_device`: devfs at boot, `devnode!`) is registered here by number.

use crate::node::File;
use crate::{abi, Errno};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use ksync::SpinLock;
use uapi::errno::*;

/// `new_decode_dev` + `new_encode_dev` of a 32-bit `dev_t` from mknod: major 12 bits, minor 20 bits split
/// around it (include/linux/kdev_t.h). The value `stat` reports as `st_rdev`.
pub fn decode_new_dev(dev: u32) -> u64 {
    let major = (dev & 0xfff00) >> 8;
    let minor = (dev & 0xff) | ((dev >> 12) & 0xfff00);
    abi::encode_dev(major, minor)
}

/// Devices by (type, number): the type is S_IFCHR or S_IFBLK.
static BY_NUMBER: SpinLock<BTreeMap<(u32, u64), Arc<dyn File>>> = SpinLock::named("vfs.devnums", BTreeMap::new());

/// Registers a device node's object under its number (a later registration of the same number replaces the
/// earlier one; only character and block devices with a number count).
pub fn register(f: &Arc<dyn File>) {
    let ftype = f.mode() & uapi::S_IFMT;
    let rdev = f.rdev();
    if rdev == 0 || !matches!(ftype, uapi::S_IFCHR | abi::S_IFBLK) {
        return;
    }
    let old = BY_NUMBER.lock().insert((ftype, rdev), f.clone());
    // A replaced object is released outside the lock.
    drop(old);
}

/// The device registered under (type, number), if any.
pub fn lookup(ftype: u32, rdev: u64) -> Option<Arc<dyn File>> {
    BY_NUMBER.lock().get(&(ftype, rdev)).cloned()
}

/// A socket name or a device number made by mknod.
pub struct Special {
    ftype: u32,
    rdev: u64,
}

impl Special {
    pub fn new(ftype: u32, rdev: u64) -> Self {
        Special { ftype, rdev }
    }
}

impl File for Special {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(ENXIO)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(ENXIO)
    }
    fn mode(&self) -> u32 {
        self.ftype
    }
    fn rdev(&self) -> u64 {
        self.rdev
    }
    fn open_node(&self, flags: u64, who: crate::inode::Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        if self.ftype == abi::S_IFSOCK {
            return Some(Err(ENXIO));
        }
        Some(match lookup(self.ftype, self.rdev) {
            Some(dev) => dev.open_node(flags, who).unwrap_or(Ok(dev)),
            None => {
                klog::event!(Debug, "vfs", "devnum.none", major = abi::dev_major(self.rdev), minor = abi::dev_minor(self.rdev), block = self.ftype == abi::S_IFBLK; "no device under this number: ENXIO");
                Err(ENXIO)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_numbers_round_trip() {
        // makedev(1, 3) and a large minor: the 32-bit value comes back as st_rdev.
        assert_eq!(decode_new_dev(0x103), 0x103);
        assert_eq!(decode_new_dev(0x0123_4567), 0x0123_4567);
        assert_eq!(abi::dev_major(decode_new_dev(0x0123_4567)), 0x345);
    }
}
