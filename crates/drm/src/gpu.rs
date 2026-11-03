//! DR-4: the `GpuDevice` trait between the amdgpu user API (INFO, CTX; `info`, `ctx`) and a GPU behind it: the
//! fake now (`fake::FakeGpu` with the 780M's captured profile), the amdgpu hardware backend later (GX-8). The
//! uAPI code reads the chip's fixed facts from a `ChipProfile` and the changing state (memory, counters, clocks,
//! registers) through the trait; a backend answers in the units Linux 6.6 hands to user space.

use crate::dma_buf::Exporter;
use crate::uapi::*;
use alloc::sync::Arc;
use ksync::SpinLock;
use mm::dma_pages::PinOwner;
use vfs::Errno;

/// `AMDGPU_HW_IP_NUM` of libdrm 2.4.134 (GFX .. VPE); type numbers at or above it are EINVAL.
pub const HW_IP_NUM: usize = AMDGPU_HW_IP_NUM as usize;

/// One HW IP type of `AMDGPU_INFO_HW_IP_INFO` as the chip has it (Linux `amdgpu_hw_ip_info`,
/// linux-6.6/drivers/gpu/drm/amd/amdgpu/amdgpu_kms.c:361): the IP block's version, its discovery version, the IB
/// alignments and how many rings of the type come up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HwIp {
    pub major: u32,
    pub minor: u32,
    /// `ip_discovery_version` (major << 16 | minor << 8 | revision).
    pub discovery: u32,
    pub ib_start_alignment: u32,
    pub ib_size_alignment: u32,
    /// Rings of the type the driver set up (`sched.ready`); the uAPI caps it by the context entities.
    pub rings: u32,
}

/// One answer of `AMDGPU_INFO_FW_VERSION`: (fw_type, index) -> (ver, feature).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Firmware {
    pub fw_type: u32,
    pub index: u32,
    pub ver: u32,
    pub feature: u32,
}

/// A register `AMDGPU_INFO_READ_MMR_REG` may read (the chip's allowed list, soc21.c:249 for GC 11), by absolute
/// dword offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Register {
    pub offset: u32,
    /// Read through GRBM_GFX_INDEX for one SE/SH (`grbm_indexed`).
    pub indexed: bool,
}

/// One codec of `AMDGPU_INFO_VIDEO_CAPS` (index = `AMDGPU_INFO_VIDEO_CAPS_CODEC_IDX_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Codec {
    pub max_width: u32,
    pub max_height: u32,
    pub max_pixels_per_frame: u32,
    pub max_level: u32,
}

/// The VBIOS strings of `AMDGPU_INFO_VBIOS_INFO` (atom context) and the image size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vbios {
    pub size: u32,
    pub name: &'static str,
    pub pn: &'static str,
    pub version: u32,
    pub ver_str: &'static str,
    pub date: &'static str,
}

/// GDS, GWS and OA sizes (`adev->gds`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gds {
    pub gds_size: u32,
    pub gws_size: u32,
    pub oa_size: u32,
}

/// The chip's fixed facts: everything INFO answers that does not change while the device runs.
#[derive(Clone, Copy, Debug)]
pub struct ChipProfile {
    pub name: &'static str,
    /// `AMDGPU_INFO_DEV_INFO` as the chip answers it (identity, clocks, CUs, VA layout, caches).
    pub dev_info: drm_amdgpu_info_device,
    /// By `AMDGPU_HW_IP_*`; None = the chip has no block of the type (HW_IP_INFO zeros, HW_IP_COUNT 0).
    pub hw_ip: [Option<HwIp>; HW_IP_NUM],
    pub firmware: &'static [Firmware],
    pub registers: &'static [Register],
    /// Decode and encode capabilities by codec index; None = the chip has no video codec table (EINVAL).
    pub video: Option<([Option<Codec>; 8], [Option<Codec>; 8])>,
    /// `AMDGPU_INFO_MAX_IBS` (0x22, newer than Linux 6.6) per HW IP type; None = EINVAL.
    pub max_ibs: Option<[u32; HW_IP_NUM]>,
    pub vbios: Vbios,
    pub gds: Gds,
    /// The chip has RAS (`AMDGPU_INFO_RAS_ENABLED_FEATURES`): the feature mask; None = EINVAL.
    pub ras_features: Option<u64>,
}

impl ChipProfile {
    pub fn hw_ip(&self, ty: u32) -> Option<&HwIp> {
        self.hw_ip.get(ty as usize).and_then(|h| h.as_ref())
    }
    pub fn firmware(&self, fw_type: u32, index: u32) -> Option<&Firmware> {
        self.firmware.iter().find(|f| f.fw_type == fw_type && f.index == index)
    }
}

/// The memory numbers INFO reports (bytes): the carve-out ("VRAM" of an APU, any size), its CPU-visible part,
/// the GTT pool, what the driver pinned and what is in use. Linux reads them from `adev->gmc` and the TTM managers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryState {
    /// `real_vram_size`.
    pub vram_total: u64,
    /// `visible_vram_size`.
    pub vram_visible: u64,
    pub vram_pinned: u64,
    pub visible_pinned: u64,
    pub vram_usage: u64,
    pub vis_vram_usage: u64,
    /// The GTT manager's size.
    pub gtt_total: u64,
    pub gart_pinned: u64,
    pub gtt_usage: u64,
    pub bytes_moved: u64,
    pub evictions: u64,
    pub vram_cpu_page_faults: u64,
}

/// DR-6: where a buffer object's memory lives (Linux's TTM placements of amdgpu: TTM_PL_VRAM, TTM_PL_TT,
/// TTM_PL_SYSTEM). On an APU "VRAM" is the carve-out, of any size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoPlace {
    Vram,
    Gtt,
    System,
}

/// The sensors of `AMDGPU_INFO_SENSOR` in Linux's user units (MHz, millidegrees, %, W, mV).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sensor {
    GfxSclk,
    GfxMclk,
    GpuTemp,
    GpuLoad,
    GpuAvgPower,
    Vddnb,
    Vddgfx,
    StablePstateSclk,
    StablePstateMclk,
    PeakPstateSclk,
    PeakPstateMclk,
}

/// Linux `adev->pm.stable_pstate_ctx` and its mutex: the context that forced a stable pstate (by its id, with
/// the pstate it restores when it ends), and the context whose change is going on right now. The lock is held
/// only to read or commit; the hardware call runs outside it.
#[derive(Clone, Copy, Default)]
pub(crate) struct PstateState {
    pub(crate) owner: Option<(u64, u32)>,
    pub(crate) changing: Option<u64>,
}

/// What the uAPI keeps per device.
pub struct GpuShared {
    pub(crate) pstate: SpinLock<PstateState>,
    /// DR-6: the device's GEM state (mmap offsets, GDS/GWS/OA use, its identity for PRIME imports).
    pub(crate) gem: Arc<crate::gem::GemShared>,
    /// DR-7: the client id of the file that holds the GFX hub's reserved VMID (`AMDGPU_VM_OP_RESERVE_VMID`; 0:
    /// none). One file at a time as on CachyOS's kernel (Linux 6.6 let all files share it, amdgpu_ids.c:462); a
    /// backend gives that file's jobs the reserved VMID.
    pub(crate) reserved_vmid_owner: core::sync::atomic::AtomicU64,
}

impl GpuShared {
    pub fn new() -> GpuShared {
        GpuShared {
            pstate: SpinLock::named("drm.gpu.pstate", PstateState { owner: None, changing: None }),
            gem: crate::gem::GemShared::new(),
            reserved_vmid_owner: core::sync::atomic::AtomicU64::new(0),
        }
    }
    /// DR-7: the client id of the file holding the reserved VMID.
    pub fn reserved_vmid_owner(&self) -> Option<u64> {
        Some(self.reserved_vmid_owner.load(core::sync::atomic::Ordering::Acquire)).filter(|&c| c != 0)
    }
}

impl Default for GpuShared {
    fn default() -> Self {
        GpuShared::new()
    }
}

/// A GPU behind the amdgpu uAPI. Every method answers at once (no waits: INFO runs in the caller's thread), except
/// `force_stable_pstate`.
pub trait GpuDevice: Send + Sync {
    fn profile(&self) -> &ChipProfile;
    /// The uAPI's per-device state, kept by the backend.
    fn shared(&self) -> &GpuShared;
    /// `adev->accel_working`: libdrm refuses a device with 0 (EBADF).
    fn accel_working(&self) -> bool {
        true
    }
    /// DEV_INFO; a backend that learns fields at run time (harvested CUs) overrides it.
    fn dev_info(&self) -> drm_amdgpu_info_device {
        self.profile().dev_info
    }
    /// Rings of HW IP type `ty` that are ready.
    fn rings_ready(&self, ty: u32) -> u32 {
        self.profile().hw_ip(ty).map(|h| h.rings).unwrap_or(0)
    }
    fn memory(&self) -> MemoryState;
    /// A register of the allowed list for one SE/SH (None = broadcast); None = the read failed.
    fn read_register(&self, reg: &Register, se: Option<u32>, sh: Option<u32>) -> Option<u32>;
    /// The GPU clock counter (`amdgpu_gfx_get_gpu_clock_counter`), ticking at DEV_INFO's `gpu_counter_freq` kHz.
    fn gpu_clock_counter(&self) -> u64;
    /// `adev->pm.dpm_enabled`: without it SENSOR answers ENOENT.
    fn dpm_enabled(&self) -> bool;
    /// A sensor value; None = the read failed (EINVAL).
    fn sensor(&self, s: Sensor) -> Option<u32>;
    /// `gpu_reset_counter`, `vram_lost_counter`, `amdgpu_in_reset`.
    fn reset_counter(&self) -> u32;
    fn vram_lost_counter(&self) -> u32;
    fn in_reset(&self) -> bool {
        false
    }
    /// The current performance level as `AMDGPU_CTX_STABLE_PSTATE_*` (NONE for auto and manual levels).
    fn stable_pstate(&self) -> u32;
    /// `amdgpu_dpm_force_performance_level` with a stable pstate (NONE = auto). It may block (on hardware it is an
    /// SMU mailbox exchange): the backend bounds the wait with its own timeout (B4) and answers ETIME/EIO then.
    /// The uAPI calls it without holding any of its locks.
    fn force_stable_pstate(&self, pstate: u32) -> Result<(), Errno>;
    /// DR-6: memory for a new buffer object, `pages` pages in `place` at a multiple of `align_pages` pages (0 and
    /// 1: any page; the carve-out allocator honours it, system pages need none), the CPU view write-combined when
    /// `wc` (CPU_GTT_USWC). The backend charges the place's pool (INFO's VRAM/GTT usage) until the memory drops, and
    /// pinned system pages to `owner` (the DRM file). ENOMEM when the place is full or a pin limit refuses; the
    /// uAPI then tries the next place the request allows. Called without any uAPI lock held.
    fn bo_memory(&self, place: BoPlace, pages: u64, align_pages: u32, wc: bool, owner: &Arc<PinOwner>) -> Result<Arc<dyn Exporter>, Errno>;
    /// DR-7: the page tables of a new client VM (`amdgpu_vm_init`), 48-bit, empty. Dropping the last `Arc` is
    /// `amdgpu_vm_fini`: the backend frees the tables and lets go of every memory its mappings hold once no job
    /// of the VM runs any more. Without a VM backend the uAPI answers GEM_VA with this error.
    fn vm_create(&self) -> Result<Arc<dyn GpuVm>, Errno> {
        Err(linux::errno::ENODEV)
    }
    /// DR-8: queues a command submission (`drm_sched_entity_push_job` for each job) under the contract of
    /// [`crate::job::Submission`]. Answers at once and runs no job in this call: the uAPI holds the entities'
    /// submit locks and the CS implicit-sync lock around it (one entity's jobs reach the backend in sequence
    /// order; a CS on a shared object sees the fences of the CS before it). Err: not taken; the backend ended
    /// its fences with the errno first (`Submission::cancel`). Without a job backend CS answers ENODEV.
    fn submit(&self, sub: crate::job::Submission) -> Result<(), Errno> {
        sub.cancel(linux::errno::ENODEV);
        Err(linux::errno::ENODEV)
    }
    /// DR-8: starts what earlier `submit`s made ready (a backend that runs jobs on the calling thread does it
    /// here); the uAPI calls it after a successful CS with no lock held. A backend with its own rings and
    /// interrupts has nothing to do.
    fn kick(&self) {}
}

/// DR-7: one page-table change of a client VM, as the uAPI hands it to the backend. Addresses are in the
/// 48-bit index space (the canonical hole taken out, amdgpu_gem.c:697), lengths in 4 KiB GPU pages.
#[derive(Clone)]
pub enum PtOp {
    /// `pages` pages at `va` to the memory of an object from its page `first` (amdgpu_vm_bo_update). The op
    /// holds the memory; the backend keeps it while its tables can reach it (until a `Clear` of the range ran
    /// and the GPU flushed its TLB), so a closed object never frees pages the GPU still sees.
    Map { va: u64, pages: u64, mem: Arc<dyn Exporter>, first: u64, place: Option<BoPlace>, uswc: bool, flags: u32 },
    /// A partially resident range without memory (AMDGPU_VM_PAGE_PRT): reads give zero, writes are dropped.
    Prt { va: u64, pages: u64, flags: u32 },
    /// No translation in `[va, va + pages)` any more: whole mappings or the parts of them inside the range.
    Clear { va: u64, pages: u64 },
}

impl PtOp {
    pub fn range(&self) -> (u64, u64) {
        match *self {
            PtOp::Map { va, pages, .. } | PtOp::Prt { va, pages, .. } | PtOp::Clear { va, pages } => (va, pages),
        }
    }
}

/// DR-9: a GPU page fault as AMDGPU_INFO_GPUVM_FAULT reports it (`struct amdgpu_vm_fault_info`): the page
/// address, the hub's L2 protection-fault status register (GFX 11 layout: MORE_FAULTS 0, WALKER_ERROR 1-3,
/// PERMISSION_FAULTS 4-7, MAPPING_ERROR 8, CID 9-17, RW 18, VMID 20-23) and the hub (`AMDGPU_VMHUB_TYPE_*` with
/// its index at `AMDGPU_VMHUB_IDX_SHIFT`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VmFault {
    pub addr: u64,
    pub status: u32,
    pub vmhub: u32,
}

/// What a backend did with a list of ops.
pub struct VmUpdate {
    /// Ops done, from the first; the rest stay queued and run again at the next update (Linux keeps the
    /// bo_va on its invalidated list and retries at the next CS).
    pub done: usize,
    /// Why op `done` was refused (None: all ran).
    pub error: Option<Errno>,
    /// Signals when the tables are written and the TLB flushed (None: already so). CS (DR-8) waits for it.
    pub fence: Option<Arc<crate::fence::Fence>>,
}

/// DR-7: a client VM's page tables in a backend (the fake builds GFX 11 tables in memory; GX-8 writes them by
/// SDMA). Like every `GpuDevice` call it answers at once: a backend that needs the GPU queues the job and hands
/// back its fence. The uAPI calls it with none of its own locks held but the file's VM update lock, so ops of
/// one VM run in the order the ioctls queued them.
pub trait GpuVm: Send + Sync {
    /// Runs `ops` in order and stops at the first one it cannot do.
    fn update(&self, ops: &[PtOp]) -> VmUpdate;
    /// DR-9: `update`, but the tables change only after every fence of `deps` signaled, and every later update
    /// of this VM after this one (Linux: the page-table job syncs to the VM root's fences, `amdgpu_vm_clear_freed`
    /// with AMDGPU_SYNC_EQ_OWNER): an UNMAP does not pull the memory away under a job queued before it. The
    /// answer still comes at once; `fence` signals when the ops ran. The default ignores `deps` (right only for a
    /// backend whose page-table writes queue behind the VM's jobs anyway); the fake defers, GX-8 makes them
    /// dependencies of its SDMA job.
    fn update_after(&self, ops: &[PtOp], deps: &[Arc<crate::fence::Fence>]) -> VmUpdate {
        let _ = deps;
        self.update(ops)
    }
    /// DR-9: the last GPU page fault of this VM (`vm->fault_info`, AMDGPU_INFO_GPUVM_FAULT); None: none yet.
    fn last_fault(&self) -> Option<VmFault> {
        None
    }
    /// The fake's VM (tests read its translations and tables).
    fn as_fake(&self) -> Option<&crate::fake_vm::FakeVm> {
        None
    }
}
