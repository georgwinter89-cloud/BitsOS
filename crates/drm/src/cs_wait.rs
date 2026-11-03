//! DR-8: `DRM_IOCTL_AMDGPU_WAIT_CS` and `DRM_IOCTL_AMDGPU_WAIT_FENCES` (linux-6.6/drivers/gpu/drm/amd/amdgpu/
//! amdgpu_cs.c:1456 and :1708): waits for jobs by context, ring and sequence number until an absolute
//! CLOCK_MONOTONIC deadline (a negative one: no deadline, `amdgpu_gem_timeout`). A deadline in the past looks
//! once and answers at once. Waits sleep in slices and end on a kill, a signal (EINTR) or the unplug (ENODEV).

use crate::ctx;
use crate::dispatch::Ctx;
use crate::entity::{self, Slot};
use crate::fence::{self, Fence};
use crate::gpu::GpuDevice;
use crate::uapi::*;
use crate::wire::Wire;
use alloc::sync::Arc;
use alloc::vec::Vec;
use linux::errno::*;
use vfs::Errno;

fn stop(ctx: &Ctx) -> Option<Errno> {
    if ctx.mem.interrupted() || ctx.mem.signal_pending() {
        return Some(EINTR);
    }
    ctx.dev.unplugged().then_some(ENODEV)
}

/// `amdgpu_cs_get_fence` (:1499): the job's fence; None when it is known done.
fn get_fence(ctx: &Ctx, gpu: &dyn GpuDevice, ctx_id: u32, ip: u32, inst: u32, ring: u32, seq: u64) -> Result<Option<Arc<Fence>>, Errno> {
    let c = ctx::lookup(ctx.file, ctx_id)?;
    let e = c.entity(gpu, ip, inst, ring)?;
    Ok(e.fence(seq)?.map(|s: Slot| s.finished))
}

/// `dma_fence_wait_timeout` with Linux's answer: Ok(true) signaled (its error, if any, as Err), Ok(false)
/// still busy at the deadline.
fn wait_one(ctx: &Ctx, f: &Arc<Fence>, end: Option<u64>) -> Result<bool, Errno> {
    if !entity::wait(f, end, &|| stop(ctx))? {
        return Ok(false);
    }
    let st = fence::status(f);
    if st < 0 { Err(-(st as Errno)) } else { Ok(true) }
}

/// WAIT_CS: `out.status` 1 while the job runs at the deadline; the job's error once it ended with one.
pub fn wait_cs_ioctl(ctx: &Ctx, gpu: &dyn GpuDevice, data: &mut [u8]) -> Result<i64, Errno> {
    let a = drm_amdgpu_wait_cs::decode(data).ok_or(EINVAL)?.r#in();
    let end = crate::gem::deadline(a.timeout);
    let busy = match get_fence(ctx, gpu, a.ctx_id, a.ip_type, a.ip_instance, a.ring, a.handle)? {
        Some(f) => !wait_one(ctx, &f, end)?,
        None => false,
    };
    let mut o = drm_amdgpu_wait_cs::ZERO;
    o.set_out(&drm_amdgpu_wait_cs_out { status: busy as u64 });
    o.encode(data).ok_or(EINVAL)?;
    Ok(0)
}

/// WAIT_FENCES: all of them (`amdgpu_cs_wait_all_fences`, :1599) or the first (`_wait_any_fence`, :1644).
pub fn wait_fences_ioctl(ctx: &Ctx, gpu: &dyn GpuDevice, data: &mut [u8]) -> Result<i64, Errno> {
    let a = drm_amdgpu_wait_fences::decode(data).ok_or(EINVAL)?.r#in();
    // CachyOS: no fences at all is EINVAL before the copy (6.6: EINVAL for "any", status 1 for "all").
    if a.fence_count == 0 {
        return Err(EINVAL);
    }
    let sz = drm_amdgpu_fence::SIZE as u64;
    let raw = crate::bo_list::copy_in(ctx, a.fences, a.fence_count as u64 * sz)?;
    let fs: Vec<drm_amdgpu_fence> = raw.chunks_exact(sz as usize).filter_map(drm_amdgpu_fence::decode).collect();
    let end = crate::gem::deadline(a.timeout_ns);
    let out = |data: &mut [u8], status: bool, first: u32| {
        let mut o = drm_amdgpu_wait_fences::ZERO;
        o.set_out(&drm_amdgpu_wait_fences_out { status: status as u32, first_signaled: first });
        let _ = o.encode(data);
    };
    if a.wait_all != 0 {
        let mut done = true;
        for f in &fs {
            let Some(fence) = get_fence(ctx, gpu, f.ctx_id, f.ip_type, f.ip_instance, f.ring, f.seq_no)? else { continue };
            if !wait_one(ctx, &fence, end)? {
                done = false;
                break;
            }
        }
        out(data, done, 0);
        return Ok(0);
    }
    let mut array = Vec::new();
    array.try_reserve_exact(fs.len()).map_err(|_| ENOMEM)?;
    for (i, f) in fs.iter().enumerate() {
        match get_fence(ctx, gpu, f.ctx_id, f.ip_type, f.ip_instance, f.ring, f.seq_no)? {
            Some(fence) => array.push(fence),
            // Known done: this one is the first (its error is not known any more: 0).
            None => {
                out(data, true, i as u32);
                return Ok(0);
            }
        }
    }
    let first = entity::wait_any(&array, end, &|| stop(ctx))?;
    out(data, first.is_some(), first.map_or(u32::MAX, |i| i as u32));
    // Linux answers the first fence's error after it wrote the result (the result is copied back anyway).
    match first.map(|i| fence::status(&array[i])) {
        Some(st) if st < 0 => Err(-(st as Errno)),
        _ => Ok(0),
    }
}

/// `amdgpu_cs_fence_to_handle_ioctl` (:1531): a job's fence (a signaled stub when it is known done) as a new
/// syncobj handle, a syncobj fd or a sync_file fd (both O_CLOEXEC); the lookup's errors come first, an unknown
/// `what` is EINVAL. Only `out.handle` (the first four bytes) is written: the rest of the union comes back as
/// the caller sent it.
pub fn fence_to_handle_ioctl(ctx: &Ctx, gpu: &dyn GpuDevice, data: &mut [u8]) -> Result<i64, Errno> {
    use crate::sync_file::{self, SyncFile, SyncobjFile};
    use crate::syncobj::Syncobj;
    let a = drm_amdgpu_fence_to_handle::decode(data).ok_or(EINVAL)?.r#in();
    let f = &a.fence;
    let fence = get_fence(ctx, gpu, f.ctx_id, f.ip_type, f.ip_instance, f.ring, f.seq_no)?.unwrap_or_else(fence::stub);
    let fds = ctx.fds.as_deref();
    let handle = match a.what {
        AMDGPU_FENCE_TO_HANDLE_GET_SYNCOBJ => {
            let o = Syncobj::new(false);
            o.replace_fence(Some(fence));
            ctx.file.syncobj_insert(o)?
        }
        AMDGPU_FENCE_TO_HANDLE_GET_SYNCOBJ_FD => {
            let o = Syncobj::new(false);
            o.replace_fence(Some(fence));
            sync_file::install(fds, SyncobjFile::new(o))? as u32
        }
        AMDGPU_FENCE_TO_HANDLE_GET_SYNC_FILE_FD => {
            if fds.is_none() {
                return Err(EMFILE);
            }
            sync_file::install(fds, SyncFile::new(fence, b""))? as u32
        }
        what => {
            klog::event!(Debug, "drm", "cs.fence_to_handle_bad_what", what = what; "EINVAL");
            return Err(EINVAL);
        }
    };
    data.get_mut(..4).ok_or(EINVAL)?.copy_from_slice(&handle.to_le_bytes());
    Ok(0)
}
