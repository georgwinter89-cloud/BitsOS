//! The core ioctls (linux-6.6/drivers/gpu/drm/drm_ioctl.c, drm_gem.c) and Linux 6.6's core table: every number
//! with its flags, so the render node refuses what Linux refuses there (EACCES) before it says "not done yet".

use crate::dispatch::{flags as F, Ctx, Desc, Handler};
use crate::syncobj as so;
use crate::uapi::*;
use crate::wire::Wire;
use linux::errno::*;
use vfs::Errno;
use vfs::sys::UserMem;

/// `overflowuid` (GET_CLIENT's uid; Linux 6.6 hollowed the ioctl out, drm_ioctl.c:182).
const OVERFLOW_UID: u64 = 65534;

fn put(data: &mut [u8], off: usize, v: &[u8]) -> Result<(), Errno> {
    data.get_mut(off..off + v.len()).ok_or(EINVAL)?.copy_from_slice(v);
    Ok(())
}

fn get<T: Wire>(data: &[u8]) -> Result<T, Errno> {
    T::decode(data).ok_or(EINVAL)
}

/// `drm_copy_field` (drm_ioctl.c:472): at most `*len` bytes of `value` (no NUL), `*len` becomes the full length;
/// a null buffer or a zero length copies nothing.
fn copy_field(mem: &dyn UserMem, buf: u64, len: &mut u64, value: &str) -> Result<(), Errno> {
    let n = value.len().min(usize::try_from(*len).unwrap_or(usize::MAX));
    *len = value.len() as u64;
    if n > 0 && buf != 0 && !mem.write(buf, &value.as_bytes()[..n]) {
        return Err(EFAULT);
    }
    Ok(())
}

/// `drm_version` (drm_ioctl.c:509). Fields are written one by one, so the caller's padding comes back as it was.
fn version(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    let mut v: drm_version = get(data)?;
    let i = ctx.dev.driver().info();
    let r = copy_field(ctx.mem, v.name, &mut v.name_len, i.name)
        .and_then(|()| copy_field(ctx.mem, v.date, &mut v.date_len, i.date))
        .and_then(|()| copy_field(ctx.mem, v.desc, &mut v.desc_len, i.desc));
    put(data, 0, &i.major.to_le_bytes())?;
    put(data, 4, &i.minor.to_le_bytes())?;
    put(data, 8, &i.patch.to_le_bytes())?;
    put(data, 16, &v.name_len.to_le_bytes())?;
    put(data, 32, &v.date_len.to_le_bytes())?;
    put(data, 48, &v.desc_len.to_le_bytes())?;
    r.map(|()| 0)
}

/// `drm_getunique` (drm_ioctl.c:116): the bus id when the buffer holds all of it, always its length.
fn get_unique(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    let u: drm_unique = get(data)?;
    let unique = ctx.dev.unique();
    if u.unique_len >= unique.len() as u64 && !unique.is_empty() && !ctx.mem.write(u.unique, unique.as_bytes()) {
        return Err(EFAULT);
    }
    put(data, 0, &(unique.len() as u64).to_le_bytes())?;
    Ok(0)
}

/// `drm_getclient` (drm_ioctl.c:182): only index 0, the caller itself.
fn get_client(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    let c: drm_client = get(data)?;
    if c.idx != 0 {
        return Err(EINVAL);
    }
    put(data, 4, &(ctx.file.authenticated() as i32).to_le_bytes())?;
    put(data, 8, &(ctx.caller.pid as u64).to_le_bytes())?;
    put(data, 16, &OVERFLOW_UID.to_le_bytes())?;
    put(data, 24, &0u64.to_le_bytes())?;
    put(data, 32, &0u64.to_le_bytes())?;
    Ok(0)
}

/// `drm_getstats`: all zero.
fn get_stats(_ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    data.get_mut(..drm_stats::SIZE).ok_or(EINVAL)?.fill(0);
    Ok(0)
}

/// `drm_getcap` (drm_ioctl.c:235): value 0 first; four caps for every driver, the rest only for KMS drivers
/// (EOPNOTSUPP otherwise), unknown ones EINVAL.
fn get_cap(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    let req: drm_get_cap = get(data)?;
    put(data, 8, &0u64.to_le_bytes())?;
    let v = cap_value(ctx.dev.driver().info(), req.capability)?;
    put(data, 8, &v.to_le_bytes())?;
    Ok(0)
}

pub fn cap_value(i: &crate::device::DriverInfo, cap: u64) -> Result<u64, Errno> {
    let f = &i.features;
    let m = &i.mode;
    let cap = u32::try_from(cap).map_err(|_| EINVAL)?;
    let v = match cap {
        DRM_CAP_TIMESTAMP_MONOTONIC => return Ok(1),
        DRM_CAP_PRIME => return Ok((DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT) as u64),
        DRM_CAP_SYNCOBJ => return Ok(f.syncobj as u64),
        DRM_CAP_SYNCOBJ_TIMELINE => return Ok(f.syncobj_timeline as u64),
        _ if !f.modeset => return Err(EOPNOTSUPP),
        DRM_CAP_DUMB_BUFFER => m.dumb_create as u64,
        DRM_CAP_VBLANK_HIGH_CRTC => 1,
        DRM_CAP_DUMB_PREFERRED_DEPTH => m.preferred_depth as u64,
        DRM_CAP_DUMB_PREFER_SHADOW => m.prefer_shadow as u64,
        DRM_CAP_ASYNC_PAGE_FLIP => m.async_page_flip as u64,
        DRM_CAP_PAGE_FLIP_TARGET => m.page_flip_target as u64,
        DRM_CAP_CURSOR_WIDTH => if m.cursor_width != 0 { m.cursor_width as u64 } else { 64 },
        DRM_CAP_CURSOR_HEIGHT => if m.cursor_height != 0 { m.cursor_height as u64 } else { 64 },
        DRM_CAP_ADDFB2_MODIFIERS => m.fb_modifiers as u64,
        DRM_CAP_CRTC_IN_VBLANK_EVENT => 1,
        _ => return Err(EINVAL),
    };
    Ok(v)
}

/// `drm_gem_close_ioctl` (drm_gem.c:804): EINVAL for a handle the file does not have.
fn gem_close(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    if !ctx.dev.driver().info().features.gem {
        return Err(EOPNOTSUPP);
    }
    let c: drm_gem_close = get(data)?;
    ctx.file.handle_delete(c.handle)?;
    Ok(0)
}

const U: Handler = Handler::Unsupported;

const fn d(cmd: u32, flags: u32, name: &'static str, handler: Handler) -> Desc {
    Desc { cmd, flags, name, handler }
}

/// Linux 6.6's `drm_ioctls[]` (drm_ioctl.c:569-719): numbers and flags as there; the legacy entries
/// (`DRM_LEGACY_IOCTL_DEF`, `drm_invalid_op` without CONFIG_DRM_LEGACY) are kept for their flags.
const LIST: &[Desc] = &[
    d(DRM_IOCTL_VERSION, F::RENDER_ALLOW, "VERSION", Handler::Core(version)),
    d(DRM_IOCTL_GET_UNIQUE, 0, "GET_UNIQUE", Handler::Core(get_unique)),
    d(DRM_IOCTL_GET_MAGIC, 0, "GET_MAGIC", U),
    d(DRM_IOCTL_IRQ_BUSID, F::MASTER | F::ROOT_ONLY, "IRQ_BUSID", U),
    d(DRM_IOCTL_GET_MAP, 0, "GET_MAP", U),
    d(DRM_IOCTL_GET_CLIENT, 0, "GET_CLIENT", Handler::Core(get_client)),
    d(DRM_IOCTL_GET_STATS, 0, "GET_STATS", Handler::Core(get_stats)),
    d(DRM_IOCTL_GET_CAP, F::RENDER_ALLOW, "GET_CAP", Handler::Core(get_cap)),
    d(DRM_IOCTL_SET_CLIENT_CAP, 0, "SET_CLIENT_CAP", U),
    d(DRM_IOCTL_SET_VERSION, F::MASTER, "SET_VERSION", U),
    d(DRM_IOCTL_SET_UNIQUE, F::AUTH | F::MASTER | F::ROOT_ONLY, "SET_UNIQUE", U),
    d(DRM_IOCTL_BLOCK, F::AUTH | F::MASTER | F::ROOT_ONLY, "BLOCK", U),
    d(DRM_IOCTL_UNBLOCK, F::AUTH | F::MASTER | F::ROOT_ONLY, "UNBLOCK", U),
    d(DRM_IOCTL_AUTH_MAGIC, F::MASTER, "AUTH_MAGIC", U),
    d(DRM_IOCTL_ADD_MAP, F::AUTH | F::MASTER | F::ROOT_ONLY, "ADD_MAP", U),
    d(DRM_IOCTL_RM_MAP, F::AUTH, "RM_MAP", U),
    d(DRM_IOCTL_SET_SAREA_CTX, F::AUTH | F::MASTER | F::ROOT_ONLY, "SET_SAREA_CTX", U),
    d(DRM_IOCTL_GET_SAREA_CTX, F::AUTH, "GET_SAREA_CTX", U),
    d(DRM_IOCTL_SET_MASTER, 0, "SET_MASTER", U),
    d(DRM_IOCTL_DROP_MASTER, 0, "DROP_MASTER", U),
    d(DRM_IOCTL_ADD_CTX, F::AUTH | F::ROOT_ONLY, "ADD_CTX", U),
    d(DRM_IOCTL_RM_CTX, F::AUTH | F::MASTER | F::ROOT_ONLY, "RM_CTX", U),
    d(DRM_IOCTL_MOD_CTX, F::AUTH | F::MASTER | F::ROOT_ONLY, "MOD_CTX", U),
    d(DRM_IOCTL_GET_CTX, F::AUTH, "GET_CTX", U),
    d(DRM_IOCTL_SWITCH_CTX, F::AUTH | F::MASTER | F::ROOT_ONLY, "SWITCH_CTX", U),
    d(DRM_IOCTL_NEW_CTX, F::AUTH | F::MASTER | F::ROOT_ONLY, "NEW_CTX", U),
    d(DRM_IOCTL_RES_CTX, F::AUTH, "RES_CTX", U),
    d(DRM_IOCTL_ADD_DRAW, F::AUTH | F::MASTER | F::ROOT_ONLY, "ADD_DRAW", U),
    d(DRM_IOCTL_RM_DRAW, F::AUTH | F::MASTER | F::ROOT_ONLY, "RM_DRAW", U),
    d(DRM_IOCTL_LOCK, F::AUTH, "LOCK", U),
    d(DRM_IOCTL_UNLOCK, F::AUTH, "UNLOCK", U),
    d(DRM_IOCTL_FINISH, F::AUTH, "FINISH", U),
    d(DRM_IOCTL_ADD_BUFS, F::AUTH | F::MASTER | F::ROOT_ONLY, "ADD_BUFS", U),
    d(DRM_IOCTL_MARK_BUFS, F::AUTH | F::MASTER | F::ROOT_ONLY, "MARK_BUFS", U),
    d(DRM_IOCTL_INFO_BUFS, F::AUTH, "INFO_BUFS", U),
    d(DRM_IOCTL_MAP_BUFS, F::AUTH, "MAP_BUFS", U),
    d(DRM_IOCTL_FREE_BUFS, F::AUTH, "FREE_BUFS", U),
    d(DRM_IOCTL_DMA, F::AUTH, "DMA", U),
    d(DRM_IOCTL_CONTROL, F::AUTH | F::MASTER | F::ROOT_ONLY, "CONTROL", U),
    d(DRM_IOCTL_AGP_ACQUIRE, F::AUTH | F::MASTER | F::ROOT_ONLY, "AGP_ACQUIRE", U),
    d(DRM_IOCTL_AGP_RELEASE, F::AUTH | F::MASTER | F::ROOT_ONLY, "AGP_RELEASE", U),
    d(DRM_IOCTL_AGP_ENABLE, F::AUTH | F::MASTER | F::ROOT_ONLY, "AGP_ENABLE", U),
    d(DRM_IOCTL_AGP_INFO, F::AUTH, "AGP_INFO", U),
    d(DRM_IOCTL_AGP_ALLOC, F::AUTH | F::MASTER | F::ROOT_ONLY, "AGP_ALLOC", U),
    d(DRM_IOCTL_AGP_FREE, F::AUTH | F::MASTER | F::ROOT_ONLY, "AGP_FREE", U),
    d(DRM_IOCTL_AGP_BIND, F::AUTH | F::MASTER | F::ROOT_ONLY, "AGP_BIND", U),
    d(DRM_IOCTL_AGP_UNBIND, F::AUTH | F::MASTER | F::ROOT_ONLY, "AGP_UNBIND", U),
    d(DRM_IOCTL_SG_ALLOC, F::AUTH | F::MASTER | F::ROOT_ONLY, "SG_ALLOC", U),
    d(DRM_IOCTL_SG_FREE, F::AUTH | F::MASTER | F::ROOT_ONLY, "SG_FREE", U),
    d(DRM_IOCTL_WAIT_VBLANK, F::UNLOCKED, "WAIT_VBLANK", U),
    d(DRM_IOCTL_MODESET_CTL, 0, "MODESET_CTL", U),
    d(DRM_IOCTL_UPDATE_DRAW, F::AUTH | F::MASTER | F::ROOT_ONLY, "UPDATE_DRAW", U),
    d(DRM_IOCTL_GEM_CLOSE, F::RENDER_ALLOW, "GEM_CLOSE", Handler::Core(gem_close)),
    d(DRM_IOCTL_GEM_FLINK, F::AUTH, "GEM_FLINK", Handler::Core(crate::gem_name::flink)),
    d(DRM_IOCTL_GEM_OPEN, F::AUTH, "GEM_OPEN", Handler::Core(crate::gem_name::open)),
    d(DRM_IOCTL_MODE_GETRESOURCES, 0, "MODE_GETRESOURCES", U),
    d(DRM_IOCTL_PRIME_HANDLE_TO_FD, F::RENDER_ALLOW, "PRIME_HANDLE_TO_FD", Handler::Core(crate::prime::handle_to_fd)),
    d(DRM_IOCTL_PRIME_FD_TO_HANDLE, F::RENDER_ALLOW, "PRIME_FD_TO_HANDLE", Handler::Core(crate::prime::fd_to_handle)),
    d(DRM_IOCTL_MODE_GETPLANERESOURCES, 0, "MODE_GETPLANERESOURCES", U),
    d(DRM_IOCTL_MODE_GETCRTC, 0, "MODE_GETCRTC", U),
    d(DRM_IOCTL_MODE_SETCRTC, F::MASTER, "MODE_SETCRTC", U),
    d(DRM_IOCTL_MODE_GETPLANE, 0, "MODE_GETPLANE", U),
    d(DRM_IOCTL_MODE_SETPLANE, F::MASTER, "MODE_SETPLANE", U),
    d(DRM_IOCTL_MODE_CURSOR, F::MASTER, "MODE_CURSOR", U),
    d(DRM_IOCTL_MODE_GETGAMMA, 0, "MODE_GETGAMMA", U),
    d(DRM_IOCTL_MODE_SETGAMMA, F::MASTER, "MODE_SETGAMMA", U),
    d(DRM_IOCTL_MODE_GETENCODER, 0, "MODE_GETENCODER", U),
    d(DRM_IOCTL_MODE_GETCONNECTOR, 0, "MODE_GETCONNECTOR", U),
    d(DRM_IOCTL_MODE_ATTACHMODE, F::MASTER, "MODE_ATTACHMODE", U),
    d(DRM_IOCTL_MODE_DETACHMODE, F::MASTER, "MODE_DETACHMODE", U),
    d(DRM_IOCTL_MODE_GETPROPERTY, 0, "MODE_GETPROPERTY", U),
    d(DRM_IOCTL_MODE_SETPROPERTY, F::MASTER, "MODE_SETPROPERTY", U),
    d(DRM_IOCTL_MODE_GETPROPBLOB, 0, "MODE_GETPROPBLOB", U),
    d(DRM_IOCTL_MODE_GETFB, 0, "MODE_GETFB", U),
    d(DRM_IOCTL_MODE_GETFB2, 0, "MODE_GETFB2", U),
    d(DRM_IOCTL_MODE_ADDFB, 0, "MODE_ADDFB", U),
    d(DRM_IOCTL_MODE_ADDFB2, 0, "MODE_ADDFB2", U),
    d(DRM_IOCTL_MODE_RMFB, 0, "MODE_RMFB", U),
    d(DRM_IOCTL_MODE_PAGE_FLIP, F::MASTER, "MODE_PAGE_FLIP", U),
    d(DRM_IOCTL_MODE_DIRTYFB, F::MASTER, "MODE_DIRTYFB", U),
    d(DRM_IOCTL_MODE_CREATE_DUMB, 0, "MODE_CREATE_DUMB", U),
    d(DRM_IOCTL_MODE_MAP_DUMB, 0, "MODE_MAP_DUMB", U),
    d(DRM_IOCTL_MODE_DESTROY_DUMB, 0, "MODE_DESTROY_DUMB", U),
    d(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, 0, "MODE_OBJ_GETPROPERTIES", U),
    d(DRM_IOCTL_MODE_OBJ_SETPROPERTY, F::MASTER, "MODE_OBJ_SETPROPERTY", U),
    d(DRM_IOCTL_MODE_CURSOR2, F::MASTER, "MODE_CURSOR2", U),
    d(DRM_IOCTL_MODE_ATOMIC, F::MASTER, "MODE_ATOMIC", U),
    d(DRM_IOCTL_MODE_CREATEPROPBLOB, 0, "MODE_CREATEPROPBLOB", U),
    d(DRM_IOCTL_MODE_DESTROYPROPBLOB, 0, "MODE_DESTROYPROPBLOB", U),
    d(DRM_IOCTL_SYNCOBJ_CREATE, F::RENDER_ALLOW, "SYNCOBJ_CREATE", Handler::Core(so::create_ioctl)),
    d(DRM_IOCTL_SYNCOBJ_DESTROY, F::RENDER_ALLOW, "SYNCOBJ_DESTROY", Handler::Core(so::destroy_ioctl)),
    d(DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, F::RENDER_ALLOW, "SYNCOBJ_HANDLE_TO_FD", Handler::Core(so::handle_to_fd_ioctl)),
    d(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, F::RENDER_ALLOW, "SYNCOBJ_FD_TO_HANDLE", Handler::Core(so::fd_to_handle_ioctl)),
    d(DRM_IOCTL_SYNCOBJ_TRANSFER, F::RENDER_ALLOW, "SYNCOBJ_TRANSFER", Handler::Core(so::transfer_ioctl)),
    d(DRM_IOCTL_SYNCOBJ_WAIT, F::RENDER_ALLOW, "SYNCOBJ_WAIT", Handler::Core(so::wait_ioctl)),
    d(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, F::RENDER_ALLOW, "SYNCOBJ_TIMELINE_WAIT", Handler::Core(so::timeline_wait_ioctl)),
    d(DRM_IOCTL_SYNCOBJ_EVENTFD, F::RENDER_ALLOW, "SYNCOBJ_EVENTFD", Handler::Core(so::eventfd_ioctl)),
    d(DRM_IOCTL_SYNCOBJ_RESET, F::RENDER_ALLOW, "SYNCOBJ_RESET", Handler::Core(so::reset_ioctl)),
    d(DRM_IOCTL_SYNCOBJ_SIGNAL, F::RENDER_ALLOW, "SYNCOBJ_SIGNAL", Handler::Core(so::signal_ioctl)),
    d(DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, F::RENDER_ALLOW, "SYNCOBJ_TIMELINE_SIGNAL", Handler::Core(so::timeline_signal_ioctl)),
    d(DRM_IOCTL_SYNCOBJ_QUERY, F::RENDER_ALLOW, "SYNCOBJ_QUERY", Handler::Core(so::query_ioctl)),
    d(DRM_IOCTL_CRTC_GET_SEQUENCE, 0, "CRTC_GET_SEQUENCE", U),
    d(DRM_IOCTL_CRTC_QUEUE_SEQUENCE, 0, "CRTC_QUEUE_SEQUENCE", U),
    d(DRM_IOCTL_MODE_CREATE_LEASE, F::MASTER, "MODE_CREATE_LEASE", U),
    d(DRM_IOCTL_MODE_LIST_LESSEES, F::MASTER, "MODE_LIST_LESSEES", U),
    d(DRM_IOCTL_MODE_GET_LEASE, F::MASTER, "MODE_GET_LEASE", U),
    d(DRM_IOCTL_MODE_REVOKE_LEASE, F::MASTER, "MODE_REVOKE_LEASE", U),
];

pub const COUNT: usize = crate::dispatch::table_len(LIST, 0);
/// Indexed by number; None = a hole (EINVAL before anything is copied, as Linux's "no function").
pub static TABLE: [Option<Desc>; COUNT] = crate::dispatch::table(LIST, 0);
