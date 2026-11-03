//! DR-6: global GEM names (linux-6.6/drivers/gpu/drm/drm_gem.c:830 `drm_gem_flink_ioctl`, :880
//! `drm_gem_open_ioctl`), the DRI2-era way to share an object, still behind libdrm's
//! `amdgpu_bo_export(gem_flink_name)` and `amdgpu_bo_import(gem_flink_name)`. Both are DRM_AUTH without
//! RENDER_ALLOW: the dispatcher answers EACCES on a render node and for an unauthenticated primary client. A name
//! lives while some file has a handle for the object (`drm_gem_object_handle_free`, `gem::Bo::handle_closed`).

use crate::dispatch::Ctx;
use crate::file::GemObject;
use crate::uapi::*;
use crate::wire::Wire;
use alloc::sync::Arc;
use linux::errno::*;
use vfs::Errno;

/// `drm_gem_flink_ioctl`: ENOENT for a handle the file does not have (or one going away), else the object's
/// name, made on the first call (lowest free from 1, ENOSPC when none is left). Only amdgpu objects carry names.
pub fn flink(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    if !ctx.dev.driver().info().features.gem {
        return Err(EOPNOTSUPP);
    }
    let mut a = drm_gem_flink::decode(data).ok_or(EINVAL)?;
    let bo = ctx.file.bo(a.handle).ok_or(ENOENT)?;
    let name = {
        let mut n = bo.gem.names.lock();
        let mut g = bo.st.lock();
        // "prevent races with concurrent gem_close" (drm_gem.c:844).
        if g.handles == 0 {
            return Err(ENOENT);
        }
        if g.name == 0 {
            g.name = n.insert(bo.weak())?;
        }
        g.name
    };
    klog::event!(Debug, "drm", "gem.named", handle = a.handle, name = name);
    a.name = name;
    a.encode(data).ok_or(EINVAL)?;
    Ok(0)
}

/// `drm_gem_open_ioctl`: ENOENT for an unknown name (names past `i32::MAX` included: Linux looks them up as
/// int), EPERM for an always-valid object of another file's VM (`amdgpu_gem_object_open`), else a new handle
/// of this file and the object's size.
pub fn open(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    if !ctx.dev.driver().info().features.gem {
        return Err(EOPNOTSUPP);
    }
    let mut a = drm_gem_open::decode(data).ok_or(EINVAL)?;
    let gpu = ctx.dev.driver().gpu().ok_or(ENOENT)?;
    let bo = {
        let n = gpu.shared().gem.names.lock();
        let bo = if (a.name as i32) > 0 { n.get(a.name).and_then(|w| w.upgrade()) } else { None };
        let bo = bo.ok_or(ENOENT)?;
        // `drm_gem_handle_create_tail` counts the handle under the name lock: the name cannot go meanwhile.
        bo.handle_opened();
        bo
    };
    let foreign_valid = bo.flags() & AMDGPU_GEM_CREATE_VM_ALWAYS_VALID as u64 != 0 && !Arc::ptr_eq(bo.resv(), &ctx.file.gem.vm_resv);
    // DR-9: a userptr object only in its own address space (amdgpu_gem_object_open, amdgpu_gem.c:172).
    if foreign_valid || crate::userptr::check_mm(ctx, &bo).is_err() {
        bo.handle_closed();
        return Err(EPERM);
    }
    let h = ctx.file.handle_insert_opened(bo.clone())?;
    klog::event!(Debug, "drm", "gem.opened_by_name", name = a.name, handle = h);
    a.handle = h;
    a.size = bo.size();
    a.encode(data).ok_or(EINVAL)?;
    Ok(0)
}
