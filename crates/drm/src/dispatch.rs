//! The ioctl front end of a DRM file (Linux `drm_ioctl`, linux-6.6/drivers/gpu/drm/drm_ioctl.c:812-890): the
//! table is looked up by number only, the argument is copied in by the caller's size, zero-extended to the
//! kernel's size, and copied back by the caller's size, each only for the direction bits both sides share. So a
//! program built against older or newer headers (libdrm 2.4.134's 64-byte `drm_amdgpu_gem_va`, 6.6's 40) works.

use crate::device::Device;
use crate::file::DrmFile;
use crate::ioc;
use crate::uapi::{DRM_COMMAND_BASE, DRM_COMMAND_END, DRM_IOCTL_BASE};
use alloc::vec::Vec;
use linux::errno::*;
use vfs::Errno;
use vfs::sys::UserMem;

/// `drm_ioctl_desc.flags` (linux-6.6/include/drm/drm_ioctl.h).
pub mod flags {
    pub const AUTH: u32 = 0x1;
    pub const MASTER: u32 = 0x2;
    pub const ROOT_ONLY: u32 = 0x4;
    pub const UNLOCKED: u32 = 0x10;
    pub const RENDER_ALLOW: u32 = 0x20;
}

/// A core handler: works on the kernel copy of the argument (`data`, at least the table's size).
pub type CoreFn = fn(&Ctx, &mut [u8]) -> Result<i64, Errno>;

#[derive(Clone, Copy)]
pub enum Handler {
    Core(CoreFn),
    /// The driver's `Driver::ioctl` answers (None there = not done yet).
    Driver,
    /// In Linux 6.6's table, not done in V2 yet: EINVAL after the copy and the permission check, as Linux's
    /// `drm_invalid_op`, plus one Warn event per number and device.
    Unsupported,
}

/// One table entry. `cmd` is the kernel's own number: its size and direction bits decide the copy.
#[derive(Clone, Copy)]
pub struct Desc {
    pub cmd: u32,
    pub flags: u32,
    pub name: &'static str,
    pub handler: Handler,
}

/// Who calls (Linux reads `current` at ioctl time).
#[derive(Clone, Copy, Debug)]
pub struct Caller {
    pub pid: u32,
    /// `capable(CAP_SYS_ADMIN)`.
    pub sys_admin: bool,
    /// `capable(CAP_SYS_NICE)` (DR-4: context priorities above NORMAL).
    pub sys_nice: bool,
}

pub struct Ctx<'a> {
    pub dev: &'a Device,
    pub file: &'a DrmFile,
    pub caller: Caller,
    pub mem: &'a dyn UserMem,
    /// The caller's fd table (ioctls that make or take fds); None for a kernel thread without one.
    pub fds: Option<alloc::sync::Arc<vfs::fd::FdTable>>,
}

/// Builds a table indexed by `nr - base` from a list (const, so tables are statics). Holes stay None.
pub const fn table<const N: usize>(list: &[Desc], base: u32) -> [Option<Desc>; N] {
    let mut t: [Option<Desc>; N] = [None; N];
    let mut i = 0;
    while i < list.len() {
        let nr = ioc::nr(list[i].cmd);
        assert!(nr >= base && ((nr - base) as usize) < N, "ioctl outside its table");
        assert!(t[(nr - base) as usize].is_none(), "two entries for one number");
        t[(nr - base) as usize] = Some(list[i]);
        i += 1;
    }
    t
}

/// Number of slots a table needs (highest `nr - base` plus one; Linux `ARRAY_SIZE` of a designated table).
pub const fn table_len(list: &[Desc], base: u32) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < list.len() {
        let k = (ioc::nr(list[i].cmd) - base) as usize + 1;
        if k > n {
            n = k;
        }
        i += 1;
    }
    n
}

/// The table entry for `nr`: driver range 0x40..0xa0 from the driver, the rest from the core.
fn lookup(dev: &Device, nr: u32) -> Option<Desc> {
    if (DRM_COMMAND_BASE..DRM_COMMAND_END).contains(&nr) {
        dev.driver().ioctls().get((nr - DRM_COMMAND_BASE) as usize).copied().flatten()
    } else {
        crate::core_ioctls::TABLE.get(nr as usize).copied().flatten()
    }
}

/// `drm_ioctl`: the whole call. Error order as Linux: ENODEV (unplugged), ENOTTY (not type 'd'), EINVAL (no
/// entry), ENOMEM, EFAULT (copy in), EACCES (`drm_ioctl_permit`), the handler; EFAULT from the copy back
/// replaces the handler's answer (Linux copies back even after an error).
pub fn ioctl(ctx: &Ctx, cmd: u32, arg: u64) -> Result<i64, Errno> {
    let Some(_inside) = ctx.dev.enter() else {
        klog::event!(Debug, "drm", "ioctl.unplugged", minor = ctx.dev.render_minor(), cmd = klog::Hex(cmd as u64); "ENODEV");
        return Err(ENODEV);
    };
    if ioc::ty(cmd) != DRM_IOCTL_BASE {
        return Err(ENOTTY);
    }
    let nr = ioc::nr(cmd);
    let Some(d) = lookup(ctx.dev, nr) else {
        ctx.dev.note_invalid(nr, cmd);
        return Err(EINVAL);
    };
    let size = ioc::size(cmd) as usize;
    let shared = ioc::dir(cmd) & ioc::dir(d.cmd);
    let in_size = if shared & ioc::IOC_WRITE != 0 { size } else { 0 };
    let out_size = if shared & ioc::IOC_READ != 0 { size } else { 0 };
    let ksize = in_size.max(out_size).max(ioc::size(d.cmd) as usize);
    let mut k: Vec<u8> = Vec::new();
    k.try_reserve_exact(ksize).map_err(|_| ENOMEM)?;
    k.resize(ksize, 0);
    if in_size > 0 && !ctx.mem.read(arg, &mut k[..in_size]) {
        return Err(EFAULT);
    }
    let mut ret = permit(d.flags, ctx).and_then(|()| run(ctx, &d, nr, &mut k));
    if out_size > 0 && !ctx.mem.write(arg, &k[..out_size]) {
        ret = Err(EFAULT);
    }
    ret
}

fn run(ctx: &Ctx, d: &Desc, nr: u32, k: &mut [u8]) -> Result<i64, Errno> {
    match d.handler {
        Handler::Core(f) => f(ctx, k),
        Handler::Driver => match ctx.dev.driver().ioctl(ctx, nr, k) {
            Some(r) => r,
            None => ctx.dev.unsupported(d),
        },
        Handler::Unsupported => ctx.dev.unsupported(d),
    }
}

/// `drm_ioctl_permit` (drm_ioctl.c:530).
pub fn permit(f: u32, ctx: &Ctx) -> Result<(), Errno> {
    let render = ctx.file.is_render();
    if f & flags::ROOT_ONLY != 0 && !ctx.caller.sys_admin {
        return Err(EACCES);
    }
    if f & flags::AUTH != 0 && !render && !ctx.file.authenticated() {
        return Err(EACCES);
    }
    if f & flags::MASTER != 0 && !ctx.file.is_master() {
        return Err(EACCES);
    }
    if f & flags::RENDER_ALLOW == 0 && render {
        return Err(EACCES);
    }
    Ok(())
}
