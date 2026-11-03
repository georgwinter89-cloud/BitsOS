//! WY-16: the DRM sync object ioctls bitscomp (wp_linux_drm_syncobj_v1) and wyclient (`--syncobj`) use, on a DRM
//! node (the render node). Struct layouts and numbers as in Linux's uapi (linux-6.6/include/uapi/drm/drm.h:778,
//! :863-955, :1018, :1151-1199); every ioctl answers an `io::Error` with the kernel's errno, nothing panics.
//! Only syncobj objects: no GPU work is ever submitted through this file.
#![allow(dead_code)] // bitscomp and wyclient (`#[path]`) use different parts

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

const DRM_CAP_SYNCOBJ_TIMELINE: u64 = 0x14;
const WAIT_FOR_SUBMIT: u32 = 1 << 1;

#[repr(C)]
#[derive(Default)]
struct GetCap {
    capability: u64,
    value: u64,
}

#[repr(C)]
#[derive(Default)]
struct Create {
    handle: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Default)]
struct Destroy {
    handle: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct Handle {
    handle: u32,
    flags: u32,
    fd: i32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct TimelineWait {
    handles: u64,
    points: u64,
    timeout_nsec: i64,
    count_handles: u32,
    flags: u32,
    first_signaled: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct TimelineArray {
    handles: u64,
    points: u64,
    count_handles: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Default)]
struct EventFd {
    handle: u32,
    flags: u32,
    point: u64,
    fd: i32,
    pad: u32,
}

/// `_IOWR('d', nr, T)` (asm-generic/ioctl.h: dir 3 in bits 30-31, size in 16-29, type in 8-15).
const fn iowr<T>(nr: u64) -> u64 {
    (3 << 30) | ((std::mem::size_of::<T>() as u64) << 16) | ((b'd' as u64) << 8) | nr
}

const GET_CAP: u64 = iowr::<GetCap>(0x0c);
const CREATE: u64 = iowr::<Create>(0xbf);
const DESTROY: u64 = iowr::<Destroy>(0xc0);
const HANDLE_TO_FD: u64 = iowr::<Handle>(0xc1);
const FD_TO_HANDLE: u64 = iowr::<Handle>(0xc2);
const TIMELINE_WAIT: u64 = iowr::<TimelineWait>(0xca);
const TIMELINE_SIGNAL: u64 = iowr::<TimelineArray>(0xcd);
const EVENTFD: u64 = iowr::<EventFd>(0xcf);

/// An open DRM node.
pub struct Drm(OwnedFd);

impl Drm {
    pub fn open(path: &str) -> io::Result<Drm> {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Drm(f.into()))
    }

    fn ioctl<T>(&self, req: u64, arg: &mut T) -> io::Result<()> {
        loop {
            // SAFETY: `req` is one of the consts above, each encoding exactly `size_of::<T>()` of the #[repr(C)]
            // struct `arg` points to (the kernel reads and writes no more than that); the struct lives for the
            // call; pointers inside it (handles/points) are set by the callers to locals alive for the call.
            let r = unsafe { libc::ioctl(self.0.as_raw_fd(), req as _, arg as *mut T) };
            if r == 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINTR) {
                return Err(e);
            }
        }
    }

    /// DRM_CAP_SYNCOBJ_TIMELINE and DRM_IOCTL_SYNCOBJ_EVENTFD both work here (the eventfd probe on handle 0, which
    /// never exists, must say ENOENT: linux-6.6/drivers/gpu/drm/drm_syncobj.c:1403-1422 checks in that order).
    pub fn has_timelines_and_eventfd(&self) -> Result<(), String> {
        let mut cap = GetCap { capability: DRM_CAP_SYNCOBJ_TIMELINE, value: 0 };
        self.ioctl(GET_CAP, &mut cap).map_err(|e| format!("DRM_CAP_SYNCOBJ_TIMELINE: {e}"))?;
        if cap.value == 0 {
            return Err("no syncobj timelines on this node".into());
        }
        let efd = new_eventfd().map_err(|e| format!("eventfd: {e}"))?;
        let mut a = EventFd { handle: 0, flags: 0, point: 1, fd: efd.as_raw_fd(), pad: 0 };
        match self.ioctl(EVENTFD, &mut a) {
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(()),
            Err(e) => Err(format!("DRM_IOCTL_SYNCOBJ_EVENTFD: {e}")),
            Ok(()) => Err("DRM_IOCTL_SYNCOBJ_EVENTFD accepted handle 0".into()),
        }
    }

    pub fn create(&self) -> io::Result<u32> {
        let mut a = Create::default();
        self.ioctl(CREATE, &mut a)?;
        Ok(a.handle)
    }

    pub fn destroy(&self, handle: u32) {
        let mut a = Destroy { handle, pad: 0 };
        let _ = self.ioctl(DESTROY, &mut a);
    }

    /// The syncobj file of `handle` (HANDLE_TO_FD without flags), what import_timeline sends.
    pub fn export(&self, handle: u32) -> io::Result<OwnedFd> {
        let mut a = Handle { handle, flags: 0, fd: -1, pad: 0 };
        self.ioctl(HANDLE_TO_FD, &mut a)?;
        // SAFETY: the kernel just installed this fd for us; nobody else owns it.
        Ok(unsafe { OwnedFd::from_raw_fd(a.fd) })
    }

    /// A handle on this file for a syncobj fd (FD_TO_HANDLE without flags).
    pub fn import(&self, fd: BorrowedFd<'_>) -> io::Result<u32> {
        let mut a = Handle { handle: 0, flags: 0, fd: fd.as_raw_fd(), pad: 0 };
        self.ioctl(FD_TO_HANDLE, &mut a)?;
        Ok(a.handle)
    }

    /// Signals `point` on the timeline `handle` (and every point before it).
    pub fn signal(&self, handle: u32, point: u64) -> io::Result<()> {
        let (h, p) = (handle, point);
        let mut a = TimelineArray { handles: &h as *const u32 as u64, points: &p as *const u64 as u64, count_handles: 1, flags: 0 };
        self.ioctl(TIMELINE_SIGNAL, &mut a)
    }

    /// Whether `point` has signalled, without waiting (absolute timeout 0: one look; no fence yet counts as not
    /// signalled, WAIT_FOR_SUBMIT, as drm_syncobj.c's array wait answers ETIME then).
    pub fn signalled(&self, handle: u32, point: u64) -> io::Result<bool> {
        let (h, p) = (handle, point);
        let mut a = TimelineWait {
            handles: &h as *const u32 as u64,
            points: &p as *const u64 as u64,
            timeout_nsec: 0,
            count_handles: 1,
            flags: WAIT_FOR_SUBMIT,
            ..Default::default()
        };
        match self.ioctl(TIMELINE_WAIT, &mut a) {
            Ok(()) => Ok(true),
            Err(e) if e.raw_os_error() == Some(libc::ETIME) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// An eventfd (nonblocking, cloexec) that becomes readable once `point` has signalled.
    pub fn eventfd(&self, handle: u32, point: u64) -> io::Result<OwnedFd> {
        let efd = new_eventfd()?;
        let mut a = EventFd { handle, flags: 0, point, fd: efd.as_raw_fd(), pad: 0 };
        self.ioctl(EVENTFD, &mut a)?;
        Ok(efd)
    }
}

fn new_eventfd() -> io::Result<OwnedFd> {
    // SAFETY: eventfd(2) makes a new fd or answers -1; checked before it is owned.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the fd was just made and is owned by nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The render node of the char device `rdev` (226:128 -> /dev/dri/renderD128), found by stat as libdrm does.
pub fn node_of(rdev: u64) -> Option<String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let dir = std::fs::read_dir("/dev/dri").ok()?;
    let mut names: Vec<String> = dir.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names.into_iter().filter(|n| n.starts_with("renderD")).map(|n| format!("/dev/dri/{n}")).find(|p| {
        std::fs::metadata(p).is_ok_and(|m| m.file_type().is_char_device() && m.rdev() == rdev)
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn ioctl_numbers_are_linux_ones() {
        // Linux 6.6's sizes (checked with a C compiler; CachyOS's newer drm.h grew drm_syncobj_handle to 24 and
        // drm_syncobj_timeline_wait to 48 bytes, and drm_ioctl zero-fills a shorter caller's struct,
        // linux-6.6/drivers/gpu/drm/drm_ioctl.c:853-890, as V2's size-tolerant dispatcher does, dr-DR-3.md).
        assert_eq!(super::GET_CAP, 0xc010_640c);
        assert_eq!(super::CREATE, 0xc008_64bf);
        assert_eq!(super::HANDLE_TO_FD, 0xc010_64c1);
        assert_eq!(super::FD_TO_HANDLE, 0xc010_64c2);
        assert_eq!(super::TIMELINE_WAIT, 0xc028_64ca);
        assert_eq!(super::TIMELINE_SIGNAL, 0xc018_64cd);
        assert_eq!(super::EVENTFD, 0xc018_64cf);
    }
}
