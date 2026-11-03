//! The DRM user API as Rust: the structs, constants and ioctl numbers of libdrm 2.4.134's `drm.h` and
//! `amdgpu_drm.h`, the types of `drm_mode.h` that `drm.h`'s ioctl numbers refer to, and Linux's `sync_file.h` and
//! `dma-buf.h` (x86_64 layout).
//!
//! Everything under `uapi/` except `mod.rs` is produced by `host/gen/drm` from those headers; regenerate instead of
//! editing. Names stay the C names (`uapi::drm_amdgpu_gem_create_in`, `uapi::DRM_IOCTL_AMDGPU_GEM_CREATE`), so a
//! header line and its Rust item are found with the same grep. C unions become their bytes plus typed accessors
//! (`union drm_amdgpu_gem_create` has `.r#in()`, `.set_in(..)`, `.out()`, ...), anonymous members are fields
//! `anon0`, `anon1`, ... Every type converts to and from the bytes a program passes with [`wire::Wire`], without
//! `unsafe`; padding is written as zero.
//!
//! DR-2, the DRM core: the render node `/dev/dri/renderD128` (char 226:128, `node`), the size-tolerant ioctl
//! front end (`dispatch`, Linux `drm_ioctl`), the core ioctls with Linux 6.6's table (`core`), open files with
//! their handle tables (`file`, `handles`), the device registry and CONFIG `drm=` (`device`), the /sys tree
//! libdrm walks (`sysfs`), and the fake GPU behind it under QEMU (`fake`).
//!
//! DR-3: fences (`fence`, Linux's dma_fence with chains and arrays), sync objects with their twelve ioctls
//! (`syncobj`) and the sync_file and syncobj fd types (`sync_file`); the fake's ring signals fences.
//!
//! DR-4: the `GpuDevice` trait between the amdgpu uAPI and a GPU (`gpu`), AMDGPU_INFO (`info`) and AMDGPU_CTX
//! (`ctx`) against it, and the 780M's captured chip profile (`profile_780m`) behind the fake.
//!
//! DR-5: the dma-buf fd type (`dma_buf`) with its reservation object (`resv`), and `/dev/udmabuf` (`udmabuf`),
//! which makes dma-bufs of memfd pages.
//!
//! DR-6: GEM buffer objects on the GPU's memory (`gem`: GEM_CREATE/MMAP/WAIT_IDLE/METADATA/OP, USERPTR refused),
//! PRIME export and import as dma-bufs (`prime`), the render node's mmap of object offsets (`gem_map`), and
//! reservation objects for implicit sync on every object; GEM_FLINK/GEM_OPEN names on the primary node (`gem_name`).
//!
//! DR-9: BO lists by handle (`bo_list`: the BO_LIST ioctl and CS by list handle), FENCE_TO_HANDLE, the GPUVM
//! fault query, GEM_USERPTR objects over a program's pages with invalidation on unmap (`userptr`, a page source
//! MM provides: **open (DR-9b)**, no source exists yet, so the ioctl answers ENODEV and only dr9_tests/ktests_dr9
//! reach the success path and the invalidation; nothing may count it as running, C1), page-table clears that wait
//! for the VM's queued jobs (`GpuVm::update_after`).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod bo_list;
pub mod core_ioctls;
pub mod cs;
mod cs_wait;
pub mod ctx;
pub mod device;
pub mod dispatch;
pub mod dma_buf;
pub mod entity;
pub mod fake;
mod fake_cs;
pub mod fake_vm;
pub mod fence;
pub mod file;
pub mod gem;
mod gem_map;
pub mod gem_name;
pub mod gpu;
mod gpu_ioctls;
pub mod handles;
pub mod info;
pub mod job;
pub mod ioc;
pub mod node;
pub mod prime;
pub mod profile_780m;
pub mod resv;
pub mod sync_file;
pub mod syncobj;
pub mod sysfs;
pub mod udmabuf;
pub mod userptr;
pub mod vm;
pub mod vm_ioctl;
#[rustfmt::skip]
pub mod uapi;
pub mod wire;

pub use device::{configure, Device, Driver};
pub use wire::Wire;

/// Registers a backend and makes its node `/dev/dri/renderD<minor>` (and /dev/dri with it).
pub fn add_device(driver: alloc::sync::Arc<dyn Driver>) -> Result<alloc::sync::Arc<Device>, vfs::Errno> {
    let d = device::register(driver)?;
    node::publish(&d);
    Ok(d)
}

/// Unplugs a device (open files answer ENODEV), removes its node, then frees the minor: in this order a
/// concurrent `add_device` can never get the minor while the old node still exists. A second call does nothing
/// (it must not remove the node of a newer device under the same minor).
pub fn remove_device(d: &alloc::sync::Arc<Device>) {
    if !device::unplug(d) {
        return;
    }
    node::unpublish(d);
    device::release_minor(d);
}

/// The boot devices after the VFS is mounted (desk change request: the kernel calls it right after `vfs::init`):
/// with `drm=fake` the fake GPU and /dev/dri/renderD128, otherwise nothing yet.
pub fn boot() {
    device::boot_devices();
}

#[cfg(not(test))]
mod ktests;
#[cfg(not(test))]
mod ktests_dr3;
#[cfg(not(test))]
mod ktests_dr4;
#[cfg(not(test))]
mod ktests_dr5;
#[cfg(not(test))]
mod ktests_dr6;
#[cfg(not(test))]
mod ktests_dr7;
#[cfg(not(test))]
mod ktests_dr8;
#[cfg(not(test))]
mod ktests_dr9;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod dr2_tests;
#[cfg(test)]
mod dr3_tests;
#[cfg(test)]
mod dr4_tests;
#[cfg(test)]
mod dr5_tests;
#[cfg(test)]
mod dr6_tests;
#[cfg(test)]
mod dr7_tests;
#[cfg(test)]
mod dr8_tests;
#[cfg(test)]
mod dr9_tests;
