//! The fake GPU (gx-roadmap.md §9): the DRM core's backend under QEMU and in host tests until the amdgpu hardware
//! backend exists. DR-2 answers only the core calls (VERSION, GET_CAP, ...); every amdgpu ioctl is in the table
//! with Linux 6.6's flags and says "not done yet" (EINVAL), INFO and friends follow later. DR-3: the fake's
//! GFX ring hands out fences and signals them in order (`FakeRing`), the test stand-in for a job's end. DR-4:
//! the fake is a `GpuDevice` with the 780M's captured `ChipProfile` (INFO and CTX answer as Linux did) and a
//! carve-out of any size.

use crate::device::{DriverInfo, Features, ModeConfig, PciIdentity, Place};
use crate::dispatch::{flags as F, Desc, Handler};
use crate::fence::{self, Fence};
use crate::ioc;
use crate::uapi::*;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use crate::dma_buf::Exporter;
use crate::gpu::{BoPlace, ChipProfile, GpuDevice, GpuShared, MemoryState, Register, Sensor};
use linux::errno::ENOMEM;
use mm::device::Aperture;
#[cfg(not(test))]
use mm::dma::DmaCache;
#[cfg(not(test))]
use mm::dma_pages::DmaPagesOptions;
use mm::dma_pages::{DmaPages, PinOwner};
use crate::profile_780m;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use ksync::SpinLock;

/// amdgpu's `drm_driver` values (linux-6.6/drivers/gpu/drm/amd/amdgpu/amdgpu_drv.c:117-119 and :2817,
/// amdgpu_drv.h:41-43; features DRIVER_GEM | DRIVER_MODESET | DRIVER_SYNCOBJ | DRIVER_SYNCOBJ_TIMELINE; mode values from
/// amdgpu_dm.c:3992-3999 and DCN 3.1.4's `max_cursor_size` 256, dcn314_resource.c:1860). 3.54 is Mesa's floor
/// (gx-roadmap.md §1 item 5).
pub const AMDGPU_INFO: DriverInfo = DriverInfo {
    name: "amdgpu",
    date: "20150101",
    desc: "AMD GPU",
    major: 3,
    minor: 54,
    patch: 0,
    features: Features { gem: true, modeset: true, syncobj: true, syncobj_timeline: true },
    mode: ModeConfig {
        dumb_create: true,
        preferred_depth: 24,
        prefer_shadow: true,
        async_page_flip: true,
        page_flip_target: false,
        cursor_width: 256,
        cursor_height: 256,
        fb_modifiers: true,
    },
};

/// GEM_VA as Linux 6.6 has it (40 bytes); libdrm 2.4.134's 64-byte form is copied in whole and zero-extends.
const GEM_VA_66: u32 = ioc::ioc(ioc::IOC_WRITE, DRM_IOCTL_BASE, 0x48, 40);

const AR: u32 = F::AUTH | F::RENDER_ALLOW;
const D: Handler = Handler::Driver;

const fn d(cmd: u32, flags: u32, name: &'static str) -> Desc {
    Desc { cmd, flags, name, handler: D }
}

/// `amdgpu_ioctls_kms[]` (amdgpu_drv.c:2773): Linux 6.6's sixteen, by number.
const LIST: &[Desc] = &[
    d(DRM_IOCTL_AMDGPU_GEM_CREATE, AR, "AMDGPU_GEM_CREATE"),
    d(DRM_IOCTL_AMDGPU_CTX, AR, "AMDGPU_CTX"),
    d(DRM_IOCTL_AMDGPU_VM, AR, "AMDGPU_VM"),
    d(DRM_IOCTL_AMDGPU_SCHED, F::MASTER, "AMDGPU_SCHED"),
    d(DRM_IOCTL_AMDGPU_BO_LIST, AR, "AMDGPU_BO_LIST"),
    d(DRM_IOCTL_AMDGPU_FENCE_TO_HANDLE, AR, "AMDGPU_FENCE_TO_HANDLE"),
    d(DRM_IOCTL_AMDGPU_GEM_MMAP, AR, "AMDGPU_GEM_MMAP"),
    d(DRM_IOCTL_AMDGPU_GEM_WAIT_IDLE, AR, "AMDGPU_GEM_WAIT_IDLE"),
    d(DRM_IOCTL_AMDGPU_CS, AR, "AMDGPU_CS"),
    d(DRM_IOCTL_AMDGPU_INFO, AR, "AMDGPU_INFO"),
    d(DRM_IOCTL_AMDGPU_WAIT_CS, AR, "AMDGPU_WAIT_CS"),
    d(DRM_IOCTL_AMDGPU_WAIT_FENCES, AR, "AMDGPU_WAIT_FENCES"),
    d(DRM_IOCTL_AMDGPU_GEM_METADATA, AR, "AMDGPU_GEM_METADATA"),
    d(GEM_VA_66, AR, "AMDGPU_GEM_VA"),
    d(DRM_IOCTL_AMDGPU_GEM_OP, AR, "AMDGPU_GEM_OP"),
    d(DRM_IOCTL_AMDGPU_GEM_USERPTR, AR, "AMDGPU_GEM_USERPTR"),
];

pub const AMDGPU_COUNT: usize = crate::dispatch::table_len(LIST, DRM_COMMAND_BASE);
pub static AMDGPU_TABLE: [Option<Desc>; AMDGPU_COUNT] = crate::dispatch::table(LIST, DRM_COMMAND_BASE);

/// A ring's fences (Linux `amdgpu_fence`: one context per ring, seqno counting up, driver "amdgpu", timeline
/// the ring's name), signaled in seqno order as a ring completes jobs.
pub struct FakeRing {
    name: &'static str,
    context: u64,
    next: AtomicU64,
    pending: SpinLock<VecDeque<Arc<Fence>>>,
}

impl FakeRing {
    pub fn new(name: &'static str) -> FakeRing {
        FakeRing { name, context: fence::context_alloc(1), next: AtomicU64::new(1), pending: SpinLock::named("drm.fake.ring", VecDeque::new()) }
    }
    /// A job's fence: the next seqno, unsignaled.
    pub fn emit(&self) -> Arc<Fence> {
        let mut q = self.pending.lock();
        let f = Fence::new(self.context, self.next.fetch_add(1, Ordering::Relaxed), "amdgpu", self.name);
        q.push_back(f.clone());
        f
    }
    /// The ring got through `seqno`: every fence up to it signals, oldest first. Returns how many.
    pub fn signal_through(&self, seqno: u64) -> usize {
        let mut n = 0;
        loop {
            let f = {
                let mut q = self.pending.lock();
                match q.front() {
                    Some(f) if f.seqno() <= seqno => q.pop_front(),
                    _ => None,
                }
            };
            let Some(f) = f else { return n };
            f.signal();
            n += 1;
        }
    }
    pub fn signal_all(&self) -> usize {
        self.signal_through(u64::MAX)
    }
    pub fn pending(&self) -> usize {
        self.pending.lock().len()
    }
    pub fn context(&self) -> u64 {
        self.context
    }
}

/// A ring that goes signals what it still owes (`amdgpu_fence_driver_force_completion` in
/// `amdgpu_fence_driver_hw_fini`, linux-6.6/drivers/gpu/drm/amd/amdgpu/amdgpu_fence.c:614): every fence signals
/// once, so the waiters and eventfd entries hanging on it are let go.
impl Drop for FakeRing {
    fn drop(&mut self) {
        let n = self.signal_all();
        if n > 0 {
            klog::event!(Info, "drm", "fake.ring_forced", ring = self.name, fences = n; "a dropped ring signals its pending fences");
        }
    }
}

/// The fake's memory: the carve-out (any size: 512 MiB to 8 GiB on the Mini-PC, 16 GiB VRAM later), the GTT
/// pool and what the kernel driver itself holds. The defaults are the captured machine's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FakeMemory {
    pub carveout: u64,
    /// CPU-visible part (the BAR); an APU sees all of the carve-out.
    pub visible: u64,
    pub gtt: u64,
    /// The driver's own pinned VRAM (firmware, rings, the display's buffers) and its other VRAM in use.
    pub kernel_vram_pinned: u64,
    pub kernel_vram_other: u64,
    pub kernel_gtt_pinned: u64,
}

impl FakeMemory {
    /// As captured: a 512 MiB carve-out, 15.6 GiB GTT, 83 MiB of the carve-out and 13.5 MiB of GTT the driver's.
    pub const CAPTURED: FakeMemory = FakeMemory {
        carveout: 0x2000_0000,
        visible: 0x2000_0000,
        gtt: 0x3_d035_3000,
        kernel_vram_pinned: 0x52f_d000,
        kernel_vram_other: 0x9000,
        kernel_gtt_pinned: 0xd8_8000,
    };
    /// The same machine with another carve-out size (all of it CPU-visible).
    pub const fn with_carveout(bytes: u64) -> FakeMemory {
        FakeMemory { carveout: bytes, visible: bytes, ..FakeMemory::CAPTURED }
    }
}

pub struct FakeGpu {
    info: DriverInfo,
    pci: PciIdentity,
    ring: FakeRing,
    profile: ChipProfile,
    shared: GpuShared,
    mem: FakeMemory,
    /// DR-6: the bytes of GEM objects in the carve-out and in GTT (beyond the kernel's own).
    usage: Arc<FakeUsage>,
    /// DR-8: the job queue (and the reset and VRAM-lost counters its recovery moves).
    sched: Arc<crate::fake_cs::FakeSched>,
    pstate: AtomicU32,
    /// Tests: the next pstate change fails with this errno (0 = none); set if a change ran under the uAPI's lock.
    pstate_fail: core::sync::atomic::AtomicI64,
    pstate_under_lock: core::sync::atomic::AtomicBool,
}

impl FakeGpu {
    /// The Mini-PC's Radeon 780M as Linux shows it (plan/hw/gx-780m-linux-trace-2026-10-27, pre.txt: c7:00.0, VGA
    /// [0300], 1002:1900 rev d5, subsystem 1002:0124). Its /sys directory is the fake's own (no PCI function behind
    /// it), directly below the root bus.
    pub fn radeon_780m() -> FakeGpu {
        FakeGpu {
            info: AMDGPU_INFO,
            pci: PciIdentity {
                domain: 0,
                bus: 0xc7,
                slot: 0,
                func: 0,
                vendor: 0x1002,
                device: 0x1900,
                subsystem_vendor: 0x1002,
                subsystem_device: 0x0124,
                revision: 0xd5,
                class: 0x03_00_00,
                driver: "amdgpu",
                place: Place::Own,
            },
            ring: FakeRing::new("gfx_0.0.0"),
            profile: profile_780m::RADEON_780M,
            shared: GpuShared::new(),
            mem: FakeMemory::CAPTURED,
            usage: Arc::new(FakeUsage::default()),
            sched: crate::fake_cs::FakeSched::new(),
            pstate: AtomicU32::new(AMDGPU_CTX_STABLE_PSTATE_NONE),
            pstate_fail: core::sync::atomic::AtomicI64::new(0),
            pstate_under_lock: core::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The 780M with other memory (another carve-out size).
    pub fn radeon_780m_with(mem: FakeMemory) -> FakeGpu {
        FakeGpu { mem, ..FakeGpu::radeon_780m() }
    }

    /// The bytes of GEM objects now in the carve-out and in GTT.
    pub fn bo_usage(&self) -> (u64, u64) {
        (self.usage.vram.load(Ordering::Relaxed), self.usage.gtt.load(Ordering::Relaxed))
    }
    /// The fake's memory sizes.
    pub fn fake_memory(&self) -> FakeMemory {
        self.mem
    }
    /// Tests: the next `force_stable_pstate` fails with `e` (as an SMU timeout would).
    pub fn fail_next_pstate(&self, e: vfs::Errno) {
        self.pstate_fail.store(e, Ordering::Release);
    }
    /// Tests: whether a pstate change ever ran while the uAPI's pstate lock was held.
    pub fn pstate_changed_under_lock(&self) -> bool {
        self.pstate_under_lock.load(Ordering::Acquire)
    }
    pub fn reset(&self, vram_lost: bool) {
        self.sched.reset_counter.fetch_add(1, Ordering::AcqRel);
        if vram_lost {
            self.sched.vram_lost.fetch_add(1, Ordering::AcqRel);
        }
        klog::event!(Info, "drm", "fake.reset", vram_lost = vram_lost);
    }

    /// The GFX ring's fences (DR-3; tests use them as dependencies they signal themselves).
    pub fn ring(&self) -> &FakeRing {
        &self.ring
    }
    /// DR-8: jobs run or ended, hangs, submissions waiting.
    pub fn cs_stats(&self) -> (u64, u64, usize) {
        self.sched.stats()
    }

    /// Another fake at a PCI address of the caller's choice (tests of a second device).
    pub fn at(bus: u8, slot: u8) -> FakeGpu {
        let mut f = FakeGpu::radeon_780m();
        f.pci.bus = bus;
        f.pci.slot = slot;
        f
    }
}

impl crate::device::Driver for FakeGpu {
    fn info(&self) -> &DriverInfo {
        &self.info
    }
    fn pci(&self) -> &PciIdentity {
        &self.pci
    }
    fn ioctls(&self) -> &'static [Option<Desc>] {
        &AMDGPU_TABLE
    }
    fn as_fake(&self) -> Option<&FakeGpu> {
        Some(self)
    }
    fn gpu(&self) -> Option<&dyn GpuDevice> {
        Some(self)
    }
    /// DR-7: amdgpu's memory lines (`amdgpu_show_fdinfo`).
    fn fdinfo(&self, file: &crate::file::DrmFile, out: &mut alloc::string::String) {
        crate::vm_ioctl::fdinfo(file, out);
    }
}

impl GpuDevice for FakeGpu {
    fn profile(&self) -> &ChipProfile {
        &self.profile
    }
    fn shared(&self) -> &GpuShared {
        &self.shared
    }
    fn memory(&self) -> MemoryState {
        let m = &self.mem;
        let (bo_vram, bo_gtt) = self.bo_usage();
        let vram = m.kernel_vram_pinned.saturating_add(m.kernel_vram_other).saturating_add(bo_vram);
        MemoryState {
            vram_total: m.carveout,
            vram_visible: m.visible,
            vram_pinned: m.kernel_vram_pinned,
            visible_pinned: m.kernel_vram_pinned.min(m.visible),
            vram_usage: vram,
            vis_vram_usage: vram.min(m.visible),
            gtt_total: m.gtt,
            gart_pinned: m.kernel_gtt_pinned,
            gtt_usage: m.kernel_gtt_pinned.saturating_add(bo_gtt),
            bytes_moved: 0,
            evictions: 0,
            vram_cpu_page_faults: 0,
        }
    }
    /// An idle GPU: the status registers read 0, GB_ADDR_CONFIG its captured value.
    fn read_register(&self, reg: &Register, _se: Option<u32>, _sh: Option<u32>) -> Option<u32> {
        Some(if reg.offset == profile_780m::GB_ADDR_CONFIG { profile_780m::GB_ADDR_CONFIG_VALUE } else { 0 })
    }
    /// The counter at `gpu_counter_freq` kHz from the kernel's monotonic clock.
    fn gpu_clock_counter(&self) -> u64 {
        let ns = fence::now().unwrap_or(0) as u128;
        (ns * self.profile.dev_info.gpu_counter_freq as u128 / 1_000_000) as u64
    }
    fn dpm_enabled(&self) -> bool {
        true
    }
    fn sensor(&self, s: Sensor) -> Option<u32> {
        profile_780m::SENSORS.iter().find(|(k, _)| *k == s).map(|&(_, v)| v)
    }
    fn reset_counter(&self) -> u32 {
        self.sched.reset_counter.load(Ordering::Acquire)
    }
    fn vram_lost_counter(&self) -> u32 {
        self.sched.vram_lost.load(Ordering::Acquire)
    }
    fn stable_pstate(&self) -> u32 {
        self.pstate.load(Ordering::Acquire)
    }
    fn force_stable_pstate(&self, pstate: u32) -> Result<(), vfs::Errno> {
        if self.shared.pstate.try_lock().is_none() {
            self.pstate_under_lock.store(true, Ordering::Release);
        }
        let e = self.pstate_fail.swap(0, Ordering::AcqRel);
        if e != 0 {
            return Err(e);
        }
        self.pstate.store(pstate, Ordering::Release);
        Ok(())
    }
    /// The carve-out and GTT as pools of their sizes (any carve-out size; what the kernel driver holds is not
    /// handed out). The fake's "VRAM" is system pages too (write-back: the fake draws nothing), so a program's
    /// mapping works the same in both places; the alignment does not matter for them.
    /// DR-7: GFX 11 page tables in heap memory (see `fake_vm`).
    fn vm_create(&self) -> Result<Arc<dyn crate::gpu::GpuVm>, vfs::Errno> {
        Ok(crate::fake_vm::FakeVm::new(self.mem.carveout)?)
    }
    /// DR-8: the IBs run in amdgpu-fmt's interpreters on the client VM (`fake_cs`).
    fn submit(&self, sub: crate::job::Submission) -> Result<(), vfs::Errno> {
        self.sched.submit(sub)
    }
    fn kick(&self) {
        self.sched.pump();
    }
    fn bo_memory(&self, place: BoPlace, pages: u64, _align_pages: u32, wc: bool, owner: &Arc<PinOwner>) -> Result<Arc<dyn Exporter>, vfs::Errno> {
        let bytes = pages.checked_mul(4096).ok_or(ENOMEM)?;
        let m = &self.mem;
        let (pool, limit) = match place {
            BoPlace::Vram => (Some(&self.usage.vram), m.carveout.saturating_sub(m.kernel_vram_pinned + m.kernel_vram_other)),
            BoPlace::Gtt => (Some(&self.usage.gtt), m.gtt.saturating_sub(m.kernel_gtt_pinned)),
            BoPlace::System => (None, u64::MAX),
        };
        if let Some(p) = pool {
            if p.try_update(Ordering::AcqRel, Ordering::Acquire, |u| u.checked_add(bytes).filter(|n| *n <= limit)).is_err() {
                klog::event!(Debug, "drm", "fake.pool_full", place = alloc::format!("{place:?}").as_str(), bytes = bytes, limit = limit);
                return Err(ENOMEM);
            }
        }
        let wc = wc && place != BoPlace::Vram;
        match FakeStore::alloc(pages, wc, owner) {
            Ok(store) => Ok(Arc::new(FakeBoMemory { store, place, bytes, usage: self.usage.clone() })),
            Err(e) => {
                if let Some(p) = pool {
                    p.fetch_sub(bytes, Ordering::AcqRel);
                }
                Err(e)
            }
        }
    }
}

/// The bytes of GEM objects in the fake's pools.
#[derive(Default)]
pub struct FakeUsage {
    vram: AtomicU64,
    gtt: AtomicU64,
}

/// The pages behind a fake object: `DmaPages` in the kernel; in host tests (no frames) plain heap bytes.
enum FakeStore {
    #[cfg_attr(test, allow(dead_code))]
    Pages(Arc<DmaPages>),
    #[cfg(test)]
    Heap(SpinLock<alloc::vec::Vec<u8>>),
}

impl FakeStore {
    #[cfg(not(test))]
    fn alloc(pages: u64, wc: bool, owner: &Arc<PinOwner>) -> Result<FakeStore, vfs::Errno> {
        let o = DmaPagesOptions::default();
        let o = if wc { o.with_cache(DmaCache::WriteCombining) } else { o };
        DmaPages::alloc_for(pages, o, Some(owner)).map(FakeStore::Pages).map_err(|e| e.errno())
    }
    #[cfg(test)]
    fn alloc(pages: u64, _wc: bool, _owner: &Arc<PinOwner>) -> Result<FakeStore, vfs::Errno> {
        let n = usize::try_from(pages.checked_mul(4096).ok_or(ENOMEM)?).map_err(|_| ENOMEM)?;
        let mut v = alloc::vec::Vec::new();
        v.try_reserve_exact(n).map_err(|_| ENOMEM)?;
        v.resize(n, 0);
        Ok(FakeStore::Heap(SpinLock::new(v)))
    }
}

/// A fake object's memory: charged to its pool until dropped.
struct FakeBoMemory {
    store: FakeStore,
    place: BoPlace,
    bytes: u64,
    usage: Arc<FakeUsage>,
}

impl Drop for FakeBoMemory {
    fn drop(&mut self) {
        match self.place {
            BoPlace::Vram => self.usage.vram.fetch_sub(self.bytes, Ordering::AcqRel),
            BoPlace::Gtt => self.usage.gtt.fetch_sub(self.bytes, Ordering::AcqRel),
            BoPlace::System => 0,
        };
    }
}

impl Exporter for FakeBoMemory {
    fn name(&self) -> &'static str {
        "amdgpu"
    }
    fn size(&self) -> u64 {
        self.bytes
    }
    fn aperture(&self) -> Option<Aperture> {
        match &self.store {
            FakeStore::Pages(p) => Some(Aperture::Pages(p.clone())),
            #[cfg(test)]
            FakeStore::Heap(_) => None,
        }
    }
    fn page_phys(&self, page: u64) -> Option<u64> {
        match &self.store {
            FakeStore::Pages(p) => p.page_phys(page),
            #[cfg(test)]
            FakeStore::Heap(_) => None,
        }
    }
    fn read(&self, off: u64, out: &mut [u8]) -> bool {
        match &self.store {
            FakeStore::Pages(p) => p.read(off, out),
            #[cfg(test)]
            FakeStore::Heap(v) => heap_io(&mut v.lock(), off, out.len()).map(|r| out.copy_from_slice(r)).is_some(),
        }
    }
    fn write(&self, off: u64, data: &[u8]) -> bool {
        match &self.store {
            FakeStore::Pages(p) => p.write(off, data),
            #[cfg(test)]
            FakeStore::Heap(v) => heap_io(&mut v.lock(), off, data.len()).map(|r| r.copy_from_slice(data)).is_some(),
        }
    }
}

#[cfg(test)]
fn heap_io(v: &mut [u8], off: u64, n: usize) -> Option<&mut [u8]> {
    let s = usize::try_from(off).ok()?;
    v.get_mut(s..s.checked_add(n)?)
}
