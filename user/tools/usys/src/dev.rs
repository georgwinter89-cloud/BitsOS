//! Device calls for UL-4b's tools (`evtest`, `fbtest`, `memstress`): ioctls with a buffer the caller owns and
//! shared mappings of a file (`/dev/fb0`, `/dev/shm` files).
//!
//! The rule of the crate holds: the kernel writes only into a Rust buffer the wrapper was given, sized for the
//! request. An ioctl number carries its argument size in bits 16..29 when its direction bits are set
//! (linux-6.6/include/uapi/asm-generic/ioctl.h `_IOC`); old numbers without a size (fbdev's 0x46xx) are allowed
//! only from a table with their struct sizes. A request whose size does not fit the buffer is refused here
//! (`REFUSED`) without a syscall.

use super::{Errno, REFUSED, SysResult, nr, raw, ret};

/// Old fbdev requests without a size in the number, with the size of the struct they read or write
/// (linux-6.6/include/uapi/linux/fb.h: fb_var_screeninfo 160 bytes, fb_fix_screeninfo 80, fb_con2fbmap 8).
const LEGACY: &[(u32, usize)] = &[
    (0x4600, 160), // FBIOGET_VSCREENINFO
    (0x4601, 160), // FBIOPUT_VSCREENINFO
    (0x4602, 80),  // FBIOGET_FSCREENINFO
    (0x4606, 160), // FBIOPAN_DISPLAY
    (0x460F, 8),   // FBIOGET_CON2FBMAP
];

/// Requests whose argument is a plain value, not a pointer (evdev.c `evdev_do_ioctl`: EVIOCGRAB and EVIOCREVOKE
/// take the int itself; fbmem.c: FBIOBLANK takes the blank level).
const BY_VALUE: &[u32] = &[0x4004_4590, 0x4004_4591, 0x4611];

/// The size a request names for its argument, or None when it names none and is not in [`LEGACY`].
pub fn ioctl_size(req: u32) -> Option<usize> {
    if req >> 30 != 0 {
        return Some(((req >> 16) & 0x3fff) as usize);
    }
    LEGACY.iter().find(|(r, _)| *r == req).map(|(_, s)| *s)
}

/// ioctl(fd, req, buf): the kernel may read and write the first `ioctl_size(req)` bytes of `buf`. Returns the
/// syscall's value (EVIOCGNAME and friends return the length they copied).
pub fn ioctl_buf(fd: usize, req: u32, buf: &mut [u8]) -> SysResult {
    match ioctl_size(req) {
        Some(n) if n <= buf.len() => {}
        _ => return Err(REFUSED),
    }
    // SAFETY: the request's argument is at most `buf.len()` bytes (checked above), and buf is ours for the call.
    ret(unsafe { raw(nr::IOCTL, [fd, req as usize, buf.as_mut_ptr() as usize, 0, 0, 0]) })
}

/// ioctl(fd, req, value) for the requests in [`BY_VALUE`]; any other request is refused.
pub fn ioctl_val(fd: usize, req: u32, value: usize) -> SysResult {
    if !BY_VALUE.contains(&req) {
        return Err(REFUSED);
    }
    // SAFETY: these requests read their argument as a number, never as an address.
    ret(unsafe { raw(nr::IOCTL, [fd, req as usize, value, 0, 0, 0]) })
}

/// A MAP_SHARED mapping of a file this program owns; unmapped on drop. Rust code reaches its bytes only
/// through the copying methods below (volatile: another process or the kernel's deferred io sees and may change
/// them).
pub struct SharedMap {
    addr: usize,
    len: usize,
}

impl SharedMap {
    /// mmap(NULL, len, prot, MAP_SHARED, fd, off).
    pub fn new(fd: usize, len: usize, prot: usize, off: usize) -> SysResult<SharedMap> {
        if len == 0 {
            return Err(Errno(22));
        }
        // SAFETY: no address hint and no MAP_FIXED: the kernel places the mapping where nothing is mapped.
        let a = ret(unsafe { raw(nr::MMAP, [0, len, prot, super::MAP_SHARED, fd, off]) })?;
        Ok(SharedMap { addr: a, len })
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
    /// Copies `data` to `off` (false outside the mapping). A page without PROT_WRITE ends the program with
    /// SIGSEGV; past the end of the file with SIGBUS (Linux's rules for shared file mappings).
    pub fn write_at(&self, off: usize, data: &[u8]) -> bool {
        if !self.inside(off, data.len()) {
            return false;
        }
        for (i, b) in data.iter().enumerate() {
            // SAFETY: inside our own mapping, which no Rust reference points into.
            unsafe { core::ptr::write_volatile((self.addr + off + i) as *mut u8, *b) };
        }
        true
    }
    /// Copies `data` (pixels) to `off` as 32-bit stores, the way a program draws (false outside or unaligned).
    pub fn write_u32s(&self, off: usize, data: &[u32]) -> bool {
        if off % 4 != 0 || !self.inside(off, data.len() * 4) {
            return false;
        }
        let p = (self.addr + off) as *mut u32;
        for (i, v) in data.iter().enumerate() {
            // SAFETY: inside our own mapping and 4-aligned (the mapping starts on a page).
            unsafe { core::ptr::write_volatile(p.add(i), *v) };
        }
        true
    }
    /// Copies bytes at `off` into `out` (false outside the mapping).
    pub fn read_at(&self, off: usize, out: &mut [u8]) -> bool {
        if !self.inside(off, out.len()) {
            return false;
        }
        for (i, b) in out.iter_mut().enumerate() {
            // SAFETY: inside our own mapping.
            *b = unsafe { core::ptr::read_volatile((self.addr + off + i) as *const u8) };
        }
        true
    }
}

impl Drop for SharedMap {
    fn drop(&mut self) {
        // SAFETY: our own range.
        let _ = unsafe { raw(nr::MUNMAP, [self.addr, self.len, 0, 0, 0, 0]) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(ioctl_size(0x8004_4501), Some(4)); // EVIOCGVERSION
        assert_eq!(ioctl_size(0x8100_4506), Some(256)); // EVIOCGNAME(256)
        assert_eq!(ioctl_size(0x4600), Some(160));
        assert_eq!(ioctl_size(0x5a5a), None);
        let mut small = [0u8; 4];
        assert_eq!(ioctl_buf(0, 0x4600, &mut small), Err(REFUSED));
        assert_eq!(ioctl_buf(0, 0x5a5a, &mut small), Err(REFUSED));
        assert_eq!(ioctl_val(0, 0x4600, 0), Err(REFUSED));
    }

    /// The numbers the tools use, as CachyOS's <linux/input.h> and <linux/fb.h> spell them (printed by a C
    /// program on the host, 20.10.2026; the same headers user/abi-tests/c/evdev.c and fbdev.c include), with the
    /// sizes this module derives from them.
    #[test]
    fn numbers_match_the_linux_headers() {
        let evdev = [
            (0x8004_4501u32, 4usize), // EVIOCGVERSION
            (0x8008_4502, 8),         // EVIOCGID
            (0x8008_4503, 8),         // EVIOCGREP
            (0x8100_4506, 256),       // EVIOCGNAME(256)
            (0x8060_4518, 96),        // EVIOCGKEY(96)
            (0x8002_4519, 2),         // EVIOCGLED(2)
            (0x8003_451b, 3),         // EVIOCGSW(3)
            (0x8004_4520, 4),         // EVIOCGBIT(0, 4)
            (0x8060_4521, 96),        // EVIOCGBIT(EV_KEY, 96)
            (0x8018_4540, 24),        // EVIOCGABS(ABS_X): struct input_absinfo
            (0x8004_4509, 4),         // EVIOCGPROP(4)
            (0x4004_45a0, 4),         // EVIOCSCLOCKID
            (0x4004_4620, 4),         // FBIO_WAITFORVSYNC
        ];
        for (req, size) in evdev {
            assert_eq!(ioctl_size(req), Some(size), "{req:#x}");
        }
        for (req, size) in [(0x4600u32, 160usize), (0x4601, 160), (0x4602, 80), (0x4606, 160), (0x460f, 8)] {
            assert_eq!(ioctl_size(req), Some(size), "{req:#x}");
        }
        // EVIOCGRAB, EVIOCREVOKE, FBIOBLANK take their value itself.
        for req in [0x4004_4590u32, 0x4004_4591, 0x4611] {
            assert!(BY_VALUE.contains(&req), "{req:#x}");
        }
    }
}
