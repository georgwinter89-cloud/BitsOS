//! DR-4: the amdgpu driver ioctls the uAPI answers from a `GpuDevice` (INFO, CTX; DR-6: the GEM calls; DR-7: GEM_VA
//! and VM; DR-8: CS, WAIT_CS, WAIT_FENCES; DR-9: BO_LIST, FENCE_TO_HANDLE); None for the others, which the
//! dispatcher then reports as not done yet.

use crate::dispatch::Ctx;
use crate::gpu::GpuDevice;
use crate::uapi::*;
use vfs::Errno;

pub fn ioctl(ctx: &Ctx, gpu: &dyn GpuDevice, nr: u32, data: &mut [u8]) -> Option<Result<i64, Errno>> {
    match nr.checked_sub(DRM_COMMAND_BASE)? {
        DRM_AMDGPU_INFO => Some(crate::info::ioctl(ctx, gpu, data)),
        DRM_AMDGPU_CTX => Some(crate::ctx::ioctl(ctx, gpu, data)),
        DRM_AMDGPU_GEM_CREATE => Some(crate::gem::create_ioctl(ctx, gpu, data)),
        DRM_AMDGPU_GEM_MMAP => Some(crate::gem::mmap_ioctl(ctx, data)),
        DRM_AMDGPU_GEM_WAIT_IDLE => Some(crate::gem::wait_idle_ioctl(ctx, data)),
        DRM_AMDGPU_GEM_METADATA => Some(crate::gem::metadata_ioctl(ctx, data)),
        DRM_AMDGPU_GEM_OP => Some(crate::gem::op_ioctl(ctx, data)),
        DRM_AMDGPU_GEM_USERPTR => Some(crate::gem::userptr_ioctl(ctx, data)),
        DRM_AMDGPU_GEM_VA => Some(crate::vm_ioctl::va_ioctl(ctx, gpu, data)),
        DRM_AMDGPU_VM => Some(crate::vm_ioctl::vm_ioctl(ctx, gpu, data)),
        DRM_AMDGPU_CS => Some(crate::cs::ioctl(ctx, gpu, data)),
        DRM_AMDGPU_WAIT_CS => Some(crate::cs_wait::wait_cs_ioctl(ctx, gpu, data)),
        DRM_AMDGPU_WAIT_FENCES => Some(crate::cs_wait::wait_fences_ioctl(ctx, gpu, data)),
        DRM_AMDGPU_BO_LIST => Some(crate::bo_list::ioctl(ctx, data)),
        DRM_AMDGPU_FENCE_TO_HANDLE => Some(crate::cs_wait::fence_to_handle_ioctl(ctx, gpu, data)),
        _ => None,
    }
}
