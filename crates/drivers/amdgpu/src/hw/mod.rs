//! GX-8a: the memory half of the hardware backend: the real 780M behind `drm::GpuDevice`, on a render node once
//! `gx_bringup {"until":"backend"}` has run the kept chain. `heap`: the carve-out's slabs; `bo`: objects in VRAM and
//! GTT; `vm`: page tables, VMIDs, PASIDs; `chip`: what INFO reads from the chip; `check`: the proof a bring-up runs
//! before the node appears; `live`: the chip's windows and registers and the node's life.
//! GX-8b: the job half: `jobs` the queue and its thread, `run` a round of it, `lost` faults and hangs, `rings` a kernel
//! queue, `frame` what goes on it, `chain` the kept owner behind its lock. GX-9a: `rungs` the recovery's steps.

pub mod bo;
pub mod chain;
pub mod boot;
pub mod check;
pub mod chip;
pub mod frame;
pub mod heap;
pub mod ih;
pub mod ihwatch;
pub mod jobs;
pub mod live;
mod lost;
pub mod rings;
mod rungs;
mod run;
pub mod vm;

use crate::dpm::{Dpm, DpmCfg, Wire};
use crate::hw::bo::{GttPool, SysBo, VramBo};
use crate::gpucore::vmring::VmInputs;
use crate::hw::chain::Chain;
use crate::hw::chip::ChipFacts;
use crate::hw::heap::Heaps;
use crate::hw::jobs::{Held, JobCfg, Jobs};
use crate::hw::vm::{HwVm, VmIds};
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use drm::device::{Driver, DriverInfo, PciIdentity};
use drm::dispatch::Desc;
use drm::dma_buf::Exporter;
use drm::fake::{AMDGPU_INFO, AMDGPU_TABLE};
use drm::file::DrmFile;
use drm::gpu::{BoPlace, ChipProfile, Firmware, GpuDevice, GpuShared, GpuVm, MemoryState, Register, Sensor};
use drm::job::Submission;
use drm::uapi::*;
use ksync::SpinLock;
use linux::errno::{ENODEV, ENOMEM};
use mm::dma_pages::PinOwner;
use vfs::Errno;

/// The job half's clock. Host tests have no scheduler clock: the process's.
pub fn clock_ns() -> u64 {
    #[cfg(test)]
    {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
    }
    #[cfg(not(test))]
    {
        drm::fence::now().unwrap_or(0)
    }
}

/// Registers INFO reads while the device runs.
pub trait RegIo: Send + Sync {
    fn read(&self, byte: u32) -> Option<u32>;
}

/// SMUIO's GOLDEN_TSC_COUNT pair (`gfx_v11_0_get_gpu_clock_counter`), by byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TscRegs {
    pub upper: u32,
    pub lower: u32,
}

/// Told of each GTT and system object (a test whose fake engines must reach their frames).
pub trait SysNote: Send + Sync {
    fn made(&self, mem: Arc<dyn Exporter>);
}

pub struct Inputs {
    pub heaps: Arc<Heaps>,
    pub regs: Arc<dyn RegIo>,
    pub tsc: Option<TscRegs>,
    pub profile: ChipProfile,
    pub facts: ChipFacts,
    pub pci: PciIdentity,
    /// The GTT manager's size (Linux: `ttm_tt_pages_limit`, half of RAM, amdgpu_ttm.c:1927).
    pub gtt_total: u64,
    pub sys_note: Option<Arc<dyn SysNote>>,
}

pub struct HardwareGpu {
    info: DriverInfo,
    pci: PciIdentity,
    profile: ChipProfile,
    facts: ChipFacts,
    shared: Arc<GpuShared>,
    heaps: Arc<Heaps>,
    regs: Arc<dyn RegIo>,
    tsc: Option<TscRegs>,
    ids: Arc<VmIds>,
    gtt: Arc<GttPool>,
    /// Files open on the node.
    open: AtomicUsize,
    /// VMs alive.
    vms: Arc<AtomicU64>,
    pstate: AtomicU32,
    sys_note: Option<Arc<dyn SysNote>>,
    /// GX-8b: the rings; none until a kept chain was handed over (`start_jobs`), then `submit` works.
    jobs: SpinLock<Option<Arc<Jobs>>>,
    /// GX-9b: the kept chain as the job half sees it; clocks and GFXOFF from `start_dpm` on.
    dpm: SpinLock<Option<Arc<Dpm>>>,
}

impl HardwareGpu {
    pub fn new(i: Inputs) -> Result<Arc<HardwareGpu>, String> {
        let ids = VmIds::new().ok_or("no VMID range")?;
        Ok(Arc::new(HardwareGpu {
            info: AMDGPU_INFO,
            pci: i.pci,
            profile: i.profile,
            facts: i.facts,
            shared: Arc::new(GpuShared::new()),
            heaps: i.heaps,
            regs: i.regs,
            tsc: i.tsc,
            ids: Arc::new(ids),
            gtt: Arc::new(GttPool { used: AtomicU64::new(0), total: i.gtt_total }),
            open: AtomicUsize::new(0),
            vms: Arc::new(AtomicU64::new(0)),
            pstate: AtomicU32::new(AMDGPU_CTX_STABLE_PSTATE_NONE),
            sys_note: i.sys_note,
            jobs: SpinLock::named("amdgpu.hw.jobs_slot", None),
            dpm: SpinLock::named("amdgpu.hw.dpm_slot", None),
        }))
    }

    pub fn facts(&self) -> &ChipFacts {
        &self.facts
    }
    pub fn open_files(&self) -> usize {
        self.open.load(Ordering::Acquire)
    }
    pub fn vms_alive(&self) -> u64 {
        self.vms.load(Ordering::Acquire)
    }
    pub fn heaps(&self) -> &Arc<Heaps> {
        &self.heaps
    }
    pub fn vm_ids(&self) -> &Arc<VmIds> {
        &self.ids
    }

    /// Tests: this device at another PCI place.
    pub(crate) fn with_pci(&self, pci: PciIdentity) -> Result<Arc<HardwareGpu>, String> {
        HardwareGpu::new(Inputs { heaps: self.heaps.clone(), regs: self.regs.clone(), tsc: self.tsc, profile: self.profile, facts: self.facts.clone(), pci, gtt_total: self.gtt.total, sys_note: self.sys_note.clone() })
    }

    /// The job half on a kept chain. A second call answers an error: there is one thread per device.
    pub(crate) fn start_jobs(&self, chain: Arc<dyn Chain>, vmi: VmInputs, cfg: JobCfg) -> Result<(), String> {
        self.start_jobs_irq(chain, vmi, cfg, None)
    }

    /// GX-7n: the same with interrupts: the fences raise them, the job thread waits for them and looks anyway.
    pub(crate) fn start_jobs_irq(&self, chain: Arc<dyn Chain>, vmi: VmInputs, cfg: JobCfg, feed: Option<Arc<ih::IrqFeed>>) -> Result<(), String> {
        let mut slot = self.jobs.lock();
        if slot.is_some() {
            return Err(String::from("the job half runs already"));
        }
        // gfx_v11_0.c:6267.
        let wave = self.facts.dev_info.cu_active_number.saturating_mul(32).saturating_sub(1);
        drop(slot);
        // GX-9b: every step of the job half passes the GFXOFF gate.
        let dpm = Dpm::new(chain, self.heaps.clone());
        *self.dpm.lock() = Some(dpm.clone());
        let j = Jobs::new(dpm, vmi, wave, cfg, feed, self.heaps.clone(), self.ids.clone())?;
        slot = self.jobs.lock();
        *slot = Some(j);
        Ok(())
    }

    /// GX-9b: clocks by load and GFXOFF on a running job half. A failure is logged inside and changes nothing.
    pub fn start_dpm(&self, wire: Wire, cfg: DpmCfg) -> Option<Arc<Dpm>> {
        let d = self.dpm()?;
        d.start(wire, cfg);
        Some(d)
    }

    pub fn dpm(&self) -> Option<Arc<Dpm>> {
        self.dpm.lock().clone()
    }

    /// Tests: a register the profile lets INFO read, and that read.
    pub(crate) fn chip_profile_register(&self) -> Option<Register> {
        self.profile.registers.iter().find(|r| !r.indexed && r.offset != drm::profile_780m::GB_ADDR_CONFIG).copied()
    }
    pub(crate) fn read_register_for_test(&self, reg: &Register) -> Option<u32> {
        self.read_register(reg, None, None)
    }
    /// Tests: table pages that can still be had.
    pub(crate) fn heaps_table_room(&self) -> usize {
        self.heaps.table_room()
    }

    /// Ends the job thread, with what the hardware may still run (`Held`: keep it until the engines are halted).
    pub fn stop_jobs(&self) -> Option<Held> {
        // GFXOFF ends before anything stops: the stop's registers are GC's.
        if let Some(d) = self.dpm() {
            d.stop();
        }
        let j = self.jobs.lock().take()?;
        Some(j.stop())
    }

    pub fn jobs(&self) -> Option<Arc<Jobs>> {
        self.jobs.lock().clone()
    }

    /// A VM as its concrete type (for jobs and tests).
    pub fn new_vm(&self, client: u64) -> Result<Arc<HwVm>, Errno> {
        HwVm::new(self.ids.clone(), self.shared.clone(), self.heaps.clone(), client, self.vms.clone())
    }
}

impl Driver for HardwareGpu {
    fn info(&self) -> &DriverInfo {
        &self.info
    }
    fn pci(&self) -> &PciIdentity {
        &self.pci
    }
    fn ioctls(&self) -> &'static [Option<Desc>] {
        &AMDGPU_TABLE
    }
    fn gpu(&self) -> Option<&dyn GpuDevice> {
        Some(self)
    }
    /// GX-9a: a device lost to a hang takes no new file, as an unplugged one (drm_open): a program that starts now, the desktop
    /// among them, meets what it meets when a boot's bring-up failed and goes on without the GPU (GX-8c).
    fn open(&self, _file: &DrmFile) -> Result<(), Errno> {
        if self.jobs().is_some_and(|j| j.shared.lost.load(Ordering::Acquire)) {
            klog::event!(Debug, "gx", "hw.open_refused"; "the device is lost");
            return Err(ENODEV);
        }
        self.open.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    fn postclose(&self, _file: &DrmFile) {
        self.open.fetch_sub(1, Ordering::AcqRel);
    }
    fn fdinfo(&self, file: &DrmFile, out: &mut String) {
        drm::vm_ioctl::fdinfo(file, out);
    }
}

impl GpuDevice for HardwareGpu {
    fn profile(&self) -> &ChipProfile {
        &self.profile
    }
    fn shared(&self) -> &GpuShared {
        &self.shared
    }
    fn dev_info(&self) -> drm_amdgpu_info_device {
        self.facts.dev_info
    }
    fn firmware(&self, fw_type: u32, index: u32) -> Option<Firmware> {
        self.facts.firmware.iter().find(|f| f.fw_type == fw_type && f.index == index).copied()
    }
    fn rings_ready(&self, ty: u32) -> u32 {
        self.facts.rings.get(ty as usize).copied().unwrap_or(0)
    }
    /// The carve-out as the hardware says. All of it is CPU-visible, whatever BAR0's size: the CPU reaches an APU's
    /// carve-out as system memory (gmc_v11_0.c:720-723: aper_size = real_vram_size). `vram_pinned`: what programs
    /// can never have, the kernel's BOs (scanout, PSP, rings, MES, GART, discovery) and the tables' slabs and reserve.
    fn memory(&self) -> MemoryState {
        let h = self.heaps.bytes();
        let size = self.heaps.carve.size();
        let kernel = size.saturating_sub(h.free + h.user_slabs + h.table_slabs);
        let pinned = kernel + h.table_slabs + h.reserved;
        let used = kernel + h.table_slabs + h.user_used;
        MemoryState {
            vram_total: size,
            vram_visible: size,
            vram_pinned: pinned,
            visible_pinned: pinned,
            vram_usage: used,
            vis_vram_usage: used,
            gtt_total: self.gtt.total,
            gart_pinned: 0,
            gtt_usage: self.gtt.used.load(Ordering::Acquire),
            bytes_moved: 0,
            evictions: 0,
            vram_cpu_page_faults: 0,
        }
    }
    /// soc21_read_register: GB_ADDR_CONFIG from the value read at bring-up, the rest live. No entry of the list has
    /// `grbm_indexed` on GC 11, so a register's SE/SH is never selected (an indexed one answers None).
    fn read_register(&self, reg: &Register, _se: Option<u32>, _sh: Option<u32>) -> Option<u32> {
        if reg.indexed {
            return None;
        }
        if reg.offset == drm::profile_780m::GB_ADDR_CONFIG {
            return Some(self.facts.gb_addr_config);
        }
        // GX-9b: amdgpu_kms.c:754: GFXOFF held off around the read; a block still asleep answers no register.
        let awake = self.dpm().map(|d| d.awake());
        if awake.as_ref().is_some_and(|a| !a.up) {
            return None;
        }
        self.regs.read(reg.offset.checked_mul(4)?)
    }
    fn gpu_clock_counter(&self) -> u64 {
        let counted = self.tsc.and_then(|t| {
            let pre = self.regs.read(t.upper)?;
            let mut lo = self.regs.read(t.lower)?;
            let hi = self.regs.read(t.upper)?;
            if pre != hi {
                lo = self.regs.read(t.lower)?;
            }
            Some(u64::from(hi) << 32 | u64::from(lo))
        });
        // No SMUIO answer: the kernel's clock at the same rate, so a program's calibration still holds.
        counted.unwrap_or_else(|| {
            let ns = u128::from(drm::fence::now().unwrap_or(0));
            (ns * u128::from(self.facts.dev_info.gpu_counter_freq) / 1_000_000) as u64
        })
    }
    /// GX-9b: until `start_dpm` SENSOR answers ENOENT, as Linux with DPM off.
    fn dpm_enabled(&self) -> bool {
        self.dpm().is_some_and(|d| d.enabled())
    }
    fn sensor(&self, s: Sensor) -> Option<u32> {
        self.dpm()?.metrics().ok()?.0.sensor(s)
    }
    fn reset_counter(&self) -> u32 {
        self.jobs().map_or(0, |j| j.shared.reset_counter.load(Ordering::Acquire))
    }
    fn vram_lost_counter(&self) -> u32 {
        self.jobs().map_or(0, |j| j.shared.vram_lost.load(Ordering::Acquire))
    }
    fn in_reset(&self) -> bool {
        self.jobs().is_some_and(|j| j.shared.in_reset.load(Ordering::Acquire))
    }
    fn stable_pstate(&self) -> u32 {
        self.pstate.load(Ordering::Acquire)
    }
    /// GX-9b runs Linux's AUTO only: a forced level is noted, nothing changes on the chip (Linux's answer without a DPM
    /// backend, amdgpu_dpm_force_performance_level: 0).
    fn force_stable_pstate(&self, pstate: u32) -> Result<(), Errno> {
        klog::event!(Debug, "gx", "hw.pstate_noted", pstate = pstate);
        self.pstate.store(pstate, Ordering::Release);
        Ok(())
    }
    fn bo_memory(&self, place: BoPlace, pages: u64, align_pages: u32, wc: bool, owner: &Arc<PinOwner>) -> Result<Arc<dyn Exporter>, Errno> {
        if pages == 0 {
            return Err(ENOMEM);
        }
        match place {
            BoPlace::Vram => Ok(VramBo::new(&self.heaps, pages, align_pages)?),
            BoPlace::Gtt | BoPlace::System => {
                let b = SysBo::new(&self.gtt, place, pages, wc, owner)?;
                if let Some(n) = &self.sys_note {
                    n.made(b.clone());
                }
                Ok(b)
            }
        }
    }
    fn vm_create(&self, client: u64) -> Result<Arc<dyn GpuVm>, Errno> {
        Ok(self.new_vm(client)?)
    }
    fn submit(&self, sub: Submission) -> Result<(), Errno> {
        match self.jobs() {
            Some(j) => j.submit(sub),
            None => {
                sub.cancel(ENODEV);
                Err(ENODEV)
            }
        }
    }
    fn kick(&self) {
        if let Some(j) = self.jobs() {
            j.kick();
        }
    }
}

impl Drop for HardwareGpu {
    /// A device that goes without `stop_jobs` (a failed bring-up, a test) takes its thread along; its jobs ended or end.
    fn drop(&mut self) {
        if let Some(j) = self.jobs.lock().take() {
            j.leave();
        }
    }
}
