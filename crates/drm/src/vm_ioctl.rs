//! DR-7: the ioctls of the per-file GPU VM (GEM_VA, AMDGPU_VM), its release at the last close and fdinfo's
//! memory lines (`vm` has the VM itself).

use crate::dispatch::Ctx;
use crate::fence;
use crate::gpu::{BoPlace, GpuDevice};
use crate::uapi::*;
use crate::vm::*;
use crate::wire::Wire;
use alloc::string::String;
use core::fmt::Write;
use linux::errno::{EINVAL, ENOENT};
use vfs::Errno;

/// The 40-byte Linux 6.6 form or libdrm 2.4.134's 64-byte one (the dispatcher zero-fills a short copy).
fn decode_va(data: &[u8]) -> Option<drm_amdgpu_gem_va> {
    let mut b = [0u8; drm_amdgpu_gem_va::SIZE];
    let n = data.len().min(b.len());
    b[..n].copy_from_slice(&data[..n]);
    drm_amdgpu_gem_va::decode(&b)
}

/// `amdgpu_gem_va_ioctl` (amdgpu_gem.c:664): the checks in Linux's order (VA range EINVAL, flags EINVAL, operation
/// EINVAL, handle ENOENT), then the operation; without DELAY_UPDATE the tables follow at once, and like Linux an
/// update error is not the caller's (amdgpu_gem.c:600-633), the job runs again at the next update. The newer
/// form's `vm_timeline_syncobj_out` gets the update's fence at `vm_timeline_point` (0: the binary fence);
/// input fences are not waited for (no update depends on GPU work yet).
pub fn va_ioctl(ctx: &Ctx, gpu: &dyn GpuDevice, data: &mut [u8]) -> Result<i64, Errno> {
    let a = decode_va(data).ok_or(EINVAL)?;
    let lim = Limits::of(&gpu.dev_info());
    if a.va_address < lim.bottom || (HOLE_START..HOLE_END).contains(&a.va_address) {
        klog::event!(Debug, "drm", "vm.bad_va", va = klog::Hex(a.va_address), bottom = klog::Hex(lim.bottom));
        return Err(EINVAL);
    }
    let va = a.va_address & HOLE_MASK;
    if va.checked_add(a.map_size).is_none_or(|end| end > lim.top) {
        klog::event!(Debug, "drm", "vm.bad_va", va = klog::Hex(a.va_address), size = a.map_size, top = klog::Hex(lim.top));
        return Err(EINVAL);
    }
    if a.flags & !VALID_FLAGS != 0 && a.flags & !PRT_FLAGS != 0 {
        return Err(EINVAL);
    }
    if !matches!(a.operation, AMDGPU_VA_OP_MAP | AMDGPU_VA_OP_UNMAP | AMDGPU_VA_OP_CLEAR | AMDGPU_VA_OP_REPLACE) {
        return Err(EINVAL);
    }
    let prt = a.flags & AMDGPU_VM_PAGE_PRT != 0;
    let bo = if a.operation != AMDGPU_VA_OP_CLEAR && !prt { Some(ctx.file.bo(a.handle).ok_or(ENOENT)?) } else { None };
    let out = match a.vm_timeline_syncobj_out {
        0 => None,
        h => Some(ctx.file.syncobj(h).ok_or(ENOENT)?),
    };
    let vm = &ctx.file.vm;
    vm.backend(gpu, ctx.file.client_id())?;
    let owner = bo.as_ref().map_or(Owner::Prt, |b| Owner::Bo(key(b)));
    let r = match a.operation {
        AMDGPU_VA_OP_MAP => map(vm, owner, bo.as_ref(), va, a.offset_in_bo, a.map_size, a.flags, false),
        AMDGPU_VA_OP_REPLACE => map(vm, owner, bo.as_ref(), va, a.offset_in_bo, a.map_size, a.flags, true),
        AMDGPU_VA_OP_UNMAP => unmap(vm, owner, va),
        _ => clear(vm, va, a.map_size),
    };
    if let Err(e) = r {
        klog::event!(Debug, "drm", "vm.op_refused", op = a.operation, va = klog::Hex(a.va_address), size = a.map_size, offset = a.offset_in_bo,
            flags = klog::Hex(a.flags as u64), errno = e);
        return Err(e);
    }
    if a.flags & AMDGPU_VM_DELAY_UPDATE == 0
        && let Err(e) = vm.flush()
    {
        klog::event!(Warn, "drm", "vm.update_failed", errno = e, op = a.operation; "page-table update failed; it runs again at the next update (Linux: not the caller's error)");
    }
    if let Some(o) = out {
        let f = vm.last_update().unwrap_or_else(fence::stub);
        if a.vm_timeline_point != 0 { o.add_point(f, a.vm_timeline_point) } else { o.replace_fence(Some(f)) }
    }
    Ok(0)
}

/// `amdgpu_vm_ioctl` (amdgpu_vm.c:2463): flags must be 0; RESERVE/UNRESERVE_VMID of the GFX hub. A file that
/// holds it reserves again with success; as on CachyOS's kernel (the ABI program shows it) another file then gets
/// ENOENT until the holder gives it back or closes (Linux 6.6 shared one reserved VMID among all of them).
pub fn vm_ioctl(ctx: &Ctx, gpu: &dyn GpuDevice, data: &mut [u8]) -> Result<i64, Errno> {
    let a = drm_amdgpu_vm_in::decode(data).ok_or(EINVAL)?;
    if a.flags != 0 {
        return Err(EINVAL);
    }
    let me = ctx.file.client_id();
    match a.op {
        AMDGPU_VM_OP_RESERVE_VMID => ctx.file.vm.reserve_vmid(gpu.shared(), me)?,
        AMDGPU_VM_OP_UNRESERVE_VMID => ctx.file.vm.unreserve_vmid(Some(gpu.shared()), me),
        _ => return Err(EINVAL),
    }
    Ok(0)
}

/// The last close of a file (after its handles): the VM goes.
pub fn release_file(dev: &crate::device::Device, file: &crate::file::DrmFile) {
    file.vm.release(dev.driver().gpu().map(|g| g.shared()), file.client_id());
}

/// `amdgpu_show_fdinfo`'s memory lines (amdgpu_fdinfo.c:89-103 with amdgpu_bo_get_memory, amdgpu_object.c:1291)
/// over the file's bo_vas. An APU's carve-out is CPU-visible as a whole; objects imported from another device
/// count as GTT like Linux's sg objects. The page tables are the backend's and not counted.
pub fn fdinfo(file: &crate::file::DrmFile, out: &mut String) {
    #[derive(Default)]
    struct S {
        vram: u64,
        gtt: u64,
        cpu: u64,
        visible: u64,
        evicted: u64,
        evicted_visible: u64,
        req_vram: u64,
        req_visible: u64,
        req_gtt: u64,
    }
    let mut s = S::default();
    {
        let g = file.vm.st.lock();
        for v in g.bos.values() {
            let b = &v.bo;
            let size = b.size();
            let place = if b.is_import() && b.place().is_none() { Some(BoPlace::Gtt) } else { b.place() };
            match place {
                Some(BoPlace::Vram) => {
                    s.vram += size;
                    s.visible += size;
                }
                Some(BoPlace::Gtt) => s.gtt += size,
                _ => s.cpu += size,
            }
            let cpu = b.flags() & AMDGPU_GEM_CREATE_CPU_ACCESS_REQUIRED as u64 != 0;
            let pref = b.preferred_domains();
            if pref & AMDGPU_GEM_DOMAIN_VRAM != 0 {
                s.req_vram += size;
                if cpu {
                    s.req_visible += size;
                }
                if place != Some(BoPlace::Vram) {
                    s.evicted += size;
                    if cpu {
                        s.evicted_visible += size;
                    }
                }
            } else if pref & AMDGPU_GEM_DOMAIN_GTT != 0 {
                s.req_gtt += size;
            }
        }
    }
    let k = |v: u64| v / 1024;
    let _ = write!(
        out,
        "drm-memory-vram:\t{} KiB\ndrm-memory-gtt: \t{} KiB\ndrm-memory-cpu: \t{} KiB\namd-memory-visible-vram:\t{} KiB\n\
         amd-evicted-vram:\t{} KiB\namd-evicted-visible-vram:\t{} KiB\namd-requested-vram:\t{} KiB\n\
         amd-requested-visible-vram:\t{} KiB\namd-requested-gtt:\t{} KiB\n",
        k(s.vram),
        k(s.gtt),
        k(s.cpu),
        k(s.visible),
        k(s.evicted),
        k(s.evicted_visible),
        k(s.req_vram),
        k(s.req_visible),
        k(s.req_gtt)
    );
}
