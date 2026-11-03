//! The fields of an ioctl number (asm-generic/ioctl.h layout, which x86_64 uses).

pub const IOC_NONE: u32 = 0;
pub const IOC_WRITE: u32 = 1;
pub const IOC_READ: u32 = 2;

/// Builds an ioctl number; None when a field does not fit (Linux refuses such a size at compile time:
/// linux-6.6/include/uapi/asm-generic/ioctl.h:32 has 14 size bits).
pub const fn checked_ioc(dir: u32, ty: u32, nr: u32, size: u32) -> Option<u32> {
    if dir > 3 || ty > 0xff || nr > 0xff || size > 0x3fff {
        return None;
    }
    Some(dir << 30 | size << 16 | ty << 8 | nr)
}

/// [`checked_ioc`] for constants: `const X: u32 = ioc(..)` with a field out of range does not compile. Never call it
/// with values from a program; use [`checked_ioc`] there.
pub const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u32 {
    match checked_ioc(dir, ty, nr, size) {
        Some(c) => c,
        None => panic!("ioctl field out of range"),
    }
}

/// IOC_WRITE: the kernel reads the argument; IOC_READ: the kernel writes it back.
pub const fn dir(cmd: u32) -> u32 {
    cmd >> 30
}

pub const fn ty(cmd: u32) -> u32 {
    (cmd >> 8) & 0xff
}

pub const fn nr(cmd: u32) -> u32 {
    cmd & 0xff
}

/// Size of the argument as the caller compiled it (DRM copies by this, not by its own size).
pub const fn size(cmd: u32) -> u32 {
    (cmd >> 16) & 0x3fff
}
