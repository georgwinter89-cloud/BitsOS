//! The core ioctls (linux-7.3.1/drivers/gpu/drm/drm_ioctl.c, drm_gem.c) and Linux 7.3.1's core table: every number
//! with its flags, so the render node refuses what Linux refuses there (EACCES) before it says "not done yet". The
//! legacy numbers (contexts, maps, buffers, AGP, SG, LOCK, MODESET_CTL) are gone from the table since Linux 6.8:
//! holes, EINVAL before anything is copied.

use crate::dispatch::{flags as F, Ctx, Desc, Handler};
use crate::syncobj as so;
use crate::uapi::*;
use crate::wire::Wire;
use linux::errno::*;
use vfs::Errno;
use vfs::sys::UserMem;

/// `overflowuid` (GET_CLIENT's uid; the ioctl is hollowed out, linux-7.3.1/drivers/gpu/drm/drm_ioctl.c:182).
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

/// `drm_version` (linux-7.3.1/drivers/gpu/drm/drm_ioctl.c:540): the date is "0" for every driver ("userspace
/// expects a non-empty string"). Fields are written one by one, so the caller's padding comes back as it was.
fn version(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    let mut v: drm_version = get(data)?;
    let i = ctx.dev.driver().info();
    let r = copy_field(ctx.mem, v.name, &mut v.name_len, i.name)
        .and_then(|()| copy_field(ctx.mem, v.date, &mut v.date_len, "0"))
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

/// `drm_getcap` (linux-7.3.1/drivers/gpu/drm/drm_ioctl.c:235): value 0 first; four caps for every driver, the rest only for KMS drivers
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
        DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP => (f.atomic && m.async_page_flip) as u64,
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

/// `drm_noop`: the permission checks were the whole work.
fn noop(_ctx: &Ctx, _data: &mut [u8]) -> Result<i64, Errno> {
    Ok(0)
}

/// `drm_invalid_op`: EINVAL after the permission checks.
fn invalid_op(_ctx: &Ctx, _data: &mut [u8]) -> Result<i64, Errno> {
    Err(EINVAL)
}

/// `DRM_CLIENT_NAME_MAX_LEN` (linux-7.3.1/include/uapi/drm/drm.h:1059).
const CLIENT_NAME_MAX: u64 = DRM_CLIENT_NAME_MAX_LEN as u64;

/// `drm_set_client_name` (linux-7.3.1/drivers/gpu/drm/drm_ioctl.c:577): `struct drm_set_client_name { u64 name_len;
/// u64 name; }`; longer than 64 EINVAL, the copy EFAULT, a NUL inside or a byte that is not printable ASCII without
/// space EINVAL; length 0 clears the name.
fn set_client_name(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    let len = u64::from_le_bytes(data.get(0..8).ok_or(EINVAL)?.try_into().map_err(|_| EINVAL)?);
    let ptr = u64::from_le_bytes(data.get(8..16).ok_or(EINVAL)?.try_into().map_err(|_| EINVAL)?);
    if len > CLIENT_NAME_MAX {
        return Err(EINVAL);
    }
    let name = if len == 0 {
        None
    } else {
        let mut b = alloc::vec![0u8; len as usize];
        if !ctx.mem.read(ptr, &mut b) {
            return Err(EFAULT);
        }
        // `strlen(new_name) != len` (a NUL inside) and `drm_validate_value_string` (isascii && isgraph).
        if !b.iter().all(|c| c.is_ascii_graphic()) {
            return Err(EINVAL);
        }
        Some(alloc::string::String::from_utf8(b).map_err(|_| EINVAL)?)
    };
    ctx.file.set_client_name(name);
    Ok(0)
}

const U: Handler = Handler::Unsupported;
const NOOP: Handler = Handler::Core(noop);
const INVALID: Handler = Handler::Core(invalid_op);


const fn d(cmd: u32, flags: u32, name: &'static str, handler: Handler) -> Desc {
    Desc { cmd, flags, name, handler }
}

/// Linux 7.3.1's `drm_ioctls[]` (linux-7.3.1/drivers/gpu/drm/drm_ioctl.c:644-747): numbers, flags and the entries
/// that are `drm_noop` / `drm_invalid_op` there as there.
const LIST: &[Desc] = &[
    d(DRM_IOCTL_VERSION, F::RENDER_ALLOW, "VERSION", Handler::Core(version)),
    d(DRM_IOCTL_GET_UNIQUE, 0, "GET_UNIQUE", Handler::Core(get_unique)),
    d(DRM_IOCTL_GET_MAGIC, 0, "GET_MAGIC", U),
    d(DRM_IOCTL_GET_CLIENT, 0, "GET_CLIENT", Handler::Core(get_client)),
    d(DRM_IOCTL_GET_STATS, 0, "GET_STATS", Handler::Core(get_stats)),
    d(DRM_IOCTL_GET_CAP, F::RENDER_ALLOW, "GET_CAP", Handler::Core(get_cap)),
    d(DRM_IOCTL_SET_CLIENT_CAP, 0, "SET_CLIENT_CAP", U),
    d(DRM_IOCTL_SET_VERSION, F::MASTER, "SET_VERSION", U),
    d(DRM_IOCTL_SET_UNIQUE, F::AUTH | F::MASTER | F::ROOT_ONLY, "SET_UNIQUE", INVALID),
    d(DRM_IOCTL_BLOCK, F::AUTH | F::MASTER | F::ROOT_ONLY, "BLOCK", NOOP),
    d(DRM_IOCTL_UNBLOCK, F::AUTH | F::MASTER | F::ROOT_ONLY, "UNBLOCK", NOOP),
    d(DRM_IOCTL_AUTH_MAGIC, F::MASTER, "AUTH_MAGIC", U),
    d(DRM_IOCTL_SET_MASTER, 0, "SET_MASTER", U),
    d(DRM_IOCTL_DROP_MASTER, 0, "DROP_MASTER", U),
    d(DRM_IOCTL_ADD_DRAW, F::AUTH | F::MASTER | F::ROOT_ONLY, "ADD_DRAW", NOOP),
    d(DRM_IOCTL_RM_DRAW, F::AUTH | F::MASTER | F::ROOT_ONLY, "RM_DRAW", NOOP),
    d(DRM_IOCTL_FINISH, F::AUTH, "FINISH", NOOP),
    d(DRM_IOCTL_WAIT_VBLANK, 0, "WAIT_VBLANK", U),
    d(DRM_IOCTL_UPDATE_DRAW, F::AUTH | F::MASTER | F::ROOT_ONLY, "UPDATE_DRAW", NOOP),
    d(DRM_IOCTL_GEM_CLOSE, F::RENDER_ALLOW, "GEM_CLOSE", Handler::Core(gem_close)),
    d(DRM_IOCTL_GEM_FLINK, F::AUTH, "GEM_FLINK", Handler::Core(crate::gem_name::flink)),
    d(DRM_IOCTL_GEM_OPEN, F::AUTH, "GEM_OPEN", Handler::Core(crate::gem_name::open)),
    // Disabled in Linux for security reasons (drm_gem.c:1018): `drm_invalid_op`.
    d(DRM_IOCTL_GEM_CHANGE_HANDLE, F::RENDER_ALLOW, "GEM_CHANGE_HANDLE", INVALID),
    d(DRM_IOCTL_MODE_GETRESOURCES, 0, "MODE_GETRESOURCES", U),
    d(DRM_IOCTL_PRIME_HANDLE_TO_FD, F::RENDER_ALLOW, "PRIME_HANDLE_TO_FD", Handler::Core(crate::prime::handle_to_fd)),
    d(DRM_IOCTL_PRIME_FD_TO_HANDLE, F::RENDER_ALLOW, "PRIME_FD_TO_HANDLE", Handler::Core(crate::prime::fd_to_handle)),
    d(DRM_IOCTL_SET_CLIENT_NAME, F::RENDER_ALLOW, "SET_CLIENT_NAME", Handler::Core(set_client_name)),
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
    d(DRM_IOCTL_MODE_ATTACHMODE, F::MASTER, "MODE_ATTACHMODE", NOOP),
    d(DRM_IOCTL_MODE_DETACHMODE, F::MASTER, "MODE_DETACHMODE", NOOP),
    d(DRM_IOCTL_MODE_GETPROPERTY, 0, "MODE_GETPROPERTY", U),
    d(DRM_IOCTL_MODE_SETPROPERTY, F::MASTER, "MODE_SETPROPERTY", U),
    d(DRM_IOCTL_MODE_GETPROPBLOB, 0, "MODE_GETPROPBLOB", U),
    d(DRM_IOCTL_MODE_GETFB, 0, "MODE_GETFB", U),
    d(DRM_IOCTL_MODE_GETFB2, 0, "MODE_GETFB2", U),
    d(DRM_IOCTL_MODE_ADDFB, 0, "MODE_ADDFB", U),
    d(DRM_IOCTL_MODE_ADDFB2, 0, "MODE_ADDFB2", U),
    d(DRM_IOCTL_MODE_RMFB, 0, "MODE_RMFB", U),
    d(DRM_IOCTL_MODE_CLOSEFB, 0, "MODE_CLOSEFB", U),
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
