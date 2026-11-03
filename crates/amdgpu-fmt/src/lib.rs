//! Pure AMD GPU hardware formats (owner: GR; GR-1: the GPU VM core, GR-2: the ring and fence core, GR-3: hang and
//! recovery core).
//!
//! Nothing here touches hardware. The GX driver (real GPU) and DR's fake GPU use the same code:
//! - [`pte`]: GFX 11 page-table and page-directory entry encoders and decoders.
//! - [`addr`]: GPU virtual addresses (48 bits, the canonical hole) and the two kinds of carve-out address.
//! - [`vm`]: a 4-level page-table builder over abstract table storage ([`store`]); tables are always
//!   written by the CPU, never by SDMA.
//! - [`walk`]: a software walker that translates a GPU address like the hardware; the tests' oracle.
//! - [`ids`]: VMID manager (a scheduling-time resource) and PASID allocator.
//! - [`gart`]: the flat GART table of VMID 0.
//! - [`pm4`], [`sdma`]: GFX 11 PM4 and SDMA 6.0 packet encoders and decoders.
//! - [`ring`]: ring geometry, reservation, NOP padding, IB padding, IB and fence frames.
//! - [`fence`]: 64-bit fence timelines with wrapping comparison; [`wb`]: writeback slots.
//! - [`sim`]: the interpreters (the fake GPU's CP and SDMA engines) on an abstract [`bus`].
//! - [`hang`]: ring and IB contents as readable packets (hang dumps), used by [`watch`]'s report.
//! - [`watch`]: per-ring job timeout over the fence timeline, the hung-ring verdict and report (GR-3).
//! - [`recover`]: the recovery ladder as data: soft kill, queue reset, GC re-init, mode 2 behind a closed gate.
//! - [`told`]: reset counters, QUERY_STATE2 flags, how jobs end (GR-3). GX-8 and GX-9a call these three.
//!
//! Linux 6.6 is the reference for bit positions and behaviour (`linux-6.6/drivers/gpu/drm/amd/amdgpu/`):
//! amdgpu_vm.h:57-112, gmc_v11_0.c:446-545, amdgpu_vm_pt.c, amdgpu_ids.c, amdgpu_gart.c, nvd.h, gfx_v11_0.c,
//! sdma_v6_0.c, sdma_v6_0_0_pkt_open.h, amdgpu_ring.c, amdgpu_fence.c. No sizes of the
//! carve-out, the GART window or the VMID range are built in: they are constructor arguments.
//!
//! The crate logs nothing (no klog dependency): every state change returns a value and the caller emits the
//! event (B6).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod addr;
pub mod bus;
pub mod encode;
pub mod fence;
pub mod gart;
pub mod hang;
pub mod ids;
pub mod pm4;
pub mod pte;
pub mod recover;
pub mod ring;
pub mod sdma;
pub mod sim;
pub mod store;
pub mod told;
pub mod vm;
pub mod walk;
pub mod watch;
pub mod wb;

mod blocks;

#[cfg(test)]
mod tests;

/// GPU page size: 4 KiB on every level of GFX 11 (amdgpu_vm_pt.c AMDGPU_GPU_PAGE_SIZE).
pub const GPU_PAGE: u64 = 4096;
/// log2 of [`GPU_PAGE`].
pub const GPU_PAGE_SHIFT: u32 = 12;

/// Linux errno values the errors map to (positive, as in include/uapi/asm-generic/errno-base.h).
pub mod errno {
    pub const ENOENT: i32 = 2;
    pub const EIO: i32 = 5;
    pub const EFAULT: i32 = 14;
    pub const EBUSY: i32 = 16;
    pub const ENOMEM: i32 = 12;
    pub const EINVAL: i32 = 22;
    pub const ENOSPC: i32 = 28;
    pub const ENODEV: i32 = 19;
    pub const ENODATA: i32 = 61;
    pub const ETIME: i32 = 62;
    pub const ECANCELED: i32 = 125;
}
