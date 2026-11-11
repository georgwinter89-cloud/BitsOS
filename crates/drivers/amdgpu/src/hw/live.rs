//! GX-8a: the backend on the chip: the CPU's windows into the carve-out (`hal::drv_gx::CarveWindow`, through MM's vmap
//! allocator: programs reach this path, so it must hold the allocator's lock, and a window can go), the registers INFO
//! may read, and the node's life. The node exists only between a passed
//! `gx_bringup {"until":"backend"}` (GX-8c: or the same run at boot, CONFIG `gpu=on`) and `until=stop`; a machine without it
//! has none.
//!
//! `until=stop` refuses while files are open on the node, naming how many (decision in plan/notes/gx-GX-8a.md): the
//! rings and the VMs' VMIDs would go under programs that have no way to know; they can be asked to close. A Reload
//! needs nothing here: the Quiesce hook halts the engines, the programs die with the kernel.
//!
//! GX-8b: the stop ends the job thread first. Jobs the hardware might still run stay in `HELD`, with all they reach,
//! until a stop proved the engines halted; once one left an engine running they stay for the boot.

use super::heap::{CarveIo, MemKind, Window};
use super::jobs::Held;
use super::{HardwareGpu, RegIo};
use crate::dcn::DcnRegs;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use amdgpu_fmt::addr::Carveout;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, Ordering};
use drm::device::{PciIdentity, Place};
use hal::drv_gx::CarveWindow;
use hal::paging::CacheType;
use ksync::{Mutex, SpinLock};

/// A window of the carve-out.
struct IoWindow(CarveWindow);

impl Window for IoWindow {
    fn read64(&self, off: u64) -> Option<u64> {
        self.0.read64(off)
    }
    fn write64(&self, off: u64, value: u64) -> bool {
        self.0.write64(off, value)
    }
    fn read(&self, off: u64, out: &mut [u8]) -> bool {
        self.0.read(off, out)
    }
    fn write(&self, off: u64, data: &[u8]) -> bool {
        self.0.write(off, data)
    }
    fn fill(&self, off: u64, len: u64, value: u64) -> bool {
        self.0.fill(off, len, value)
    }
}

/// The carve-out through the CPU (its physical base from MMMC_VM_FB_OFFSET, as the PSP's pieces).
pub struct LiveCarve {
    pub carve: Carveout,
}

impl CarveIo for LiveCarve {
    fn map(&self, range: Range<u64>, kind: MemKind) -> Option<Arc<dyn Window>> {
        let phys = self.carve.phys_of(range.start)?;
        let len = range.end.checked_sub(range.start)?;
        if let Err(why) = crate::carve::map_check(phys, len) {
            klog::event!(Error, "gx", "hw.map_refused", phys = klog::Hex(phys), why = why.as_str());
            return None;
        }
        let cache = match kind {
            MemKind::Tables => CacheType::Uncached,
            MemKind::User => CacheType::WriteCombining,
        };
        match CarveWindow::map(phys, len, cache) {
            Ok(io) => {
                klog::event!(Info, "gx", "hw.carve_mapped", phys = klog::Hex(phys), bytes = len, kind = format!("{kind:?}").as_str());
                Some(Arc::new(IoWindow(io)))
            }
            Err(e) => {
                klog::event!(Error, "gx", "hw.map_failed", phys = klog::Hex(phys), why = format!("{e:?}").as_str());
                None
            }
        }
    }
    fn barrier(&self) {
        // Drains the write-combining buffers; uncached table stores are ordered already.
        hal::drv_gx::store_barrier();
    }
    fn mappable(&self) -> bool {
        true
    }
}

/// The registers INFO reads, by byte: only these, and never one the display code owns (`state::wide_readable`).
pub struct LiveRegs {
    pub regs: &'static DcnRegs,
    pub allowed: Vec<u32>,
}

impl RegIo for LiveRegs {
    fn read(&self, byte: u32) -> Option<u32> {
        (crate::state::wide_readable(byte) && self.allowed.contains(&byte)).then(|| self.regs.read(byte).ok()).flatten()
    }
}

/// GX-7n: what the IH ring reaches past the chain on the chip: BAR5, BAR2's first page.
pub struct LivePorts {
    pub regs: &'static DcnRegs,
    pub doorbell: Arc<hal::mmio::IoMem>,
}

impl super::ih::IhPorts for LivePorts {
    fn read(&self, byte: u32) -> Option<u32> {
        self.regs.read(byte).ok()
    }
    fn write(&self, byte: u32, v: u32) -> bool {
        self.regs.write_raw(byte, v).is_ok()
    }
    fn doorbell(&self, byte: usize, v: u32) -> bool {
        self.doorbell.write32(byte, v).is_ok()
    }
}

/// GX-7n: the MSI vector as the IH thread waits on it.
pub struct LiveLine {
    handle: pci::IrqHandle,
    set: SpinLock<Option<pci::IrqSet>>,
    text: String,
}

impl LiveLine {
    /// One vector, MSI-X or MSI (Linux's one vector, amdgpu_irq.c:279); polling is not asked for: the job thread polls by
    /// itself without a ring, so a machine without MSI just has no ring.
    pub fn new(f: &pci::PciFunction) -> Result<Arc<LiveLine>, pci::IrqError> {
        let set = f.alloc_irqs(&pci::IrqRequest::new("amdgpu", 1, 1).modes(true, true, false))?;
        let handle = set.handle(0).ok_or(pci::IrqError::NoSuchIndex)?;
        let text = match set.vector(0) {
            Some(v) => format!("{} vector {:#x} on CPU {}", set.mode().name(), v.vector, v.cpu),
            None => format!("{} with no vector", set.mode().name()),
        };
        Ok(Arc::new(LiveLine { handle, set: SpinLock::named("amdgpu.ih.set", Some(set)), text }))
    }
}

impl super::ih::IrqLine for LiveLine {
    fn wait(&self, ns: u64) -> super::ih::Woke {
        match self.handle.wait(ns) {
            pci::Wake::Irq => super::ih::Woke::Message,
            pci::Wake::Poll => super::ih::Woke::Look,
            pci::Wake::Timeout => super::ih::Woke::Timeout,
        }
    }
    fn release(&self) {
        // Outside the lock: dropping a set takes `pci.enable`.
        let set = self.set.lock().take();
        drop(set);
    }
    fn messages(&self) -> u64 {
        self.handle.events()
    }
    fn describe(&self) -> String {
        self.text.clone()
    }
}

/// The registered backend.
struct Up {
    dev: Arc<drm::Device>,
    gpu: Arc<HardwareGpu>,
}

/// The node's place: empty until a passed bring-up, empty again after a stop.
pub struct NodeSlot(Mutex<Option<Up>>);

/// The chip's node.
pub static NODE: NodeSlot = NodeSlot::new();

impl NodeSlot {
    pub const fn new() -> NodeSlot {
        NodeSlot(Mutex::named("amdgpu.hw.up", None))
    }

    /// The render minor of the running backend.
    pub fn minor(&self) -> Option<u32> {
        self.0.lock().as_ref().map(|u| u.dev.render_minor())
    }

    /// Registers the checked backend: `/dev/dri/renderD<minor>` appears.
    pub fn register(&self, gpu: Arc<HardwareGpu>) -> Result<u32, String> {
        let mut up = self.0.lock();
        if let Some(u) = up.as_ref() {
            // A second node would lose the first: nothing could count its files or remove it any more.
            return Err(format!("already up on renderD{}", u.dev.render_minor()));
        }
        let dev = drm::add_device(gpu.clone()).map_err(|e| format!("errno {e}"))?;
        let m = dev.render_minor();
        klog::event!(Info, "gx", "hw.node_up", minor = m; "the 780M answers on the render node");
        *up = Some(Up { dev, gpu: gpu.clone() });
        drop(up);
        crate::dpm_live::start(&gpu);
        Ok(m)
    }

    /// `until=stop`'s first step: `Err(why)` while files are open on the node; `Ok(note)` when the node went (or
    /// never was). The look and the removal are one step under the lock; a file opened after the look answers
    /// ENODEV, as after a hot unplug.
    pub fn stop_gate(&self) -> Result<Option<String>, String> {
        let mut up = self.0.lock();
        let Some(u) = up.as_ref() else { return Ok(None) };
        let n = u.gpu.open_files();
        let m = u.dev.render_minor();
        if n > 0 {
            klog::event!(Info, "gx", "hw.stop_refused", files = n, minor = m);
            return Err(format!("backend: {n} open files on renderD{m}: close them first (the rings would go under them); nothing was stopped"));
        }
        drm::remove_device(&u.dev);
        let kept = HELD.hold(u.gpu.stop_jobs());
        *up = None;
        klog::event!(Info, "gx", "hw.node_down", minor = m, held = kept);
        Ok(Some(format!("backend: renderD{m} removed (no file was open){}", if kept > 0 { format!("; {kept} jobs the engines may still run stay held until they are halted") } else { String::new() })))
    }

    /// The running backend (the tool's look at its jobs).
    pub fn gpu(&self) -> Option<Arc<HardwareGpu>> {
        self.0.lock().as_ref().map(|u| u.gpu.clone())
    }
}

/// GX-9a: whether a stop that finds no chain may say every engine is halted. `stop_chain` takes the chain before it halts
/// anything, so a stop that failed leaves no chain, and the next stop has nothing to look at: this remembers. Not
/// `HeldJobs::left_running`: a stop that found no chain sets that after a good stop too, for the jobs' sake.
pub struct HaltProof {
    failed: AtomicBool,
}

/// The chip's.
pub static HALTS: HaltProof = HaltProof::new();

impl HaltProof {
    pub const fn new() -> HaltProof {
        HaltProof { failed: AtomicBool::new(false) }
    }

    /// A chain's stop ended. `all_engines`: the chain held the CP and SDMA, so a pass speaks for an earlier stop's failure
    /// too; a chain of an early stage halted neither, and what an earlier stop left stays unproven.
    pub fn stop_ran(&self, halted: bool, all_engines: bool) {
        if !halted {
            self.failed.store(true, Ordering::Release);
        } else if all_engines {
            self.failed.store(false, Ordering::Release);
        }
    }

    /// A halt outside a chain's stop failed (a run with no kept chain halts its engines itself).
    pub fn halt_failed(&self) {
        self.failed.store(true, Ordering::Release);
    }

    /// No stop or halt since the last proof left an engine running.
    pub fn proven(&self) -> bool {
        !self.failed.load(Ordering::Acquire)
    }
}

impl Default for HaltProof {
    fn default() -> HaltProof {
        HaltProof::new()
    }
}

/// Jobs the engines may still run, and whether a stop left an engine running.
pub struct HeldJobs {
    jobs: SpinLock<Vec<Held>>,
    left_running: AtomicBool,
}

/// The chip's.
pub static HELD: HeldJobs = HeldJobs::new();

impl HeldJobs {
    pub const fn new() -> HeldJobs {
        HeldJobs { jobs: SpinLock::named("amdgpu.hw.held", Vec::new()), left_running: AtomicBool::new(false) }
    }

    /// Keeps what `stop_jobs` handed back; how many jobs that was.
    pub fn hold(&self, h: Option<Held>) -> usize {
        let Some(h) = h.filter(|h| !h.is_empty()) else { return 0 };
        let n = h.len();
        self.jobs.lock().push(h);
        n
    }

    /// An engine may still run: until a full reset passes (`engines_proven_halted`).
    pub fn engines_left_running(&self) {
        self.left_running.store(true, Ordering::Release);
    }

    /// GX-9a: a full reset passed: every engine was stopped, started from the PSP on a rewritten ring, and answered its ring
    /// test. Nothing of an earlier stop runs any more, so the flag goes and what is held with it: how many jobs that was.
    pub fn engines_proven_halted(&self) -> usize {
        self.left_running.store(false, Ordering::Release);
        let held = core::mem::take(&mut *self.jobs.lock());
        held.iter().map(Held::len).sum()
    }

    /// A stop that found no chain vouches for nothing: what is held went through a stop that failed, or the chain went
    /// another way.
    pub fn stop_found_no_chain(&self) {
        if self.count() > 0 {
            self.engines_left_running();
        }
    }

    pub fn count(&self) -> usize {
        self.jobs.lock().iter().map(Held::len).sum()
    }

    /// A stop answered: `Ok(n)`: `n` jobs let go; `Err(n)`: `n` stay held.
    pub fn after_stop(&self, stop: Result<bool, ()>) -> Result<usize, usize> {
        if stop != Ok(true) || self.left_running.load(Ordering::Acquire) {
            return Err(self.count());
        }
        let held = core::mem::take(&mut *self.jobs.lock());
        Ok(held.iter().map(Held::len).sum())
    }
}

/// One `gx_bringup` at a time. `until=backend` runs long with no lock held between its stages, its proof and the
/// node; a second run, a stop or a stage inside that stretch would pull the kept chain from under it.
pub struct ToolGate {
    busy: AtomicBool,
    /// GX-9b: the hold of a run that started the backend.
    late: SpinLock<Option<crate::dpm::Awake>>,
}

/// The running tool call; dropping it lets the next one in. GX-9b: GC stays awake for it: its stages read and write
/// GC's registers directly, not through the job half's chain.
pub struct ToolRun<'a> {
    gate: &'a ToolGate,
    _awake: Option<crate::dpm::Awake>,
}

impl ToolGate {
    pub const fn new() -> ToolGate {
        ToolGate { busy: AtomicBool::new(false), late: SpinLock::named("amdgpu.hw.tool_late", None) }
    }
    /// None: another call runs. (No `then_some`: its argument is made either way, and its drop would open the gate
    /// under the call that holds it.)
    pub fn enter(&self) -> Option<ToolRun<'_>> {
        (!self.busy.swap(true, Ordering::AcqRel)).then(|| ToolRun { gate: self, _awake: NODE.gpu().and_then(|g| g.dpm()).map(|d| d.awake()) })
    }
    /// GC stays awake for the rest of the running call; no call runs: nothing to hold it for.
    pub fn hold(&self, a: crate::dpm::Awake) {
        if self.busy.load(Ordering::Acquire) {
            let old = self.late.lock().replace(a);
            drop(old);
        }
    }
}

impl Drop for ToolRun<'_> {
    fn drop(&mut self) {
        let late = self.gate.late.lock().take();
        drop(late);
        self.gate.busy.store(false, Ordering::Release);
    }
}

/// The tool's gate.
pub static TOOL: ToolGate = ToolGate::new();

/// The GPU function as libdrm finds it below /sys (PC's PCI view holds its files).
pub fn pci_identity(g: &crate::gpu::Gpu) -> Option<PciIdentity> {
    let f = g.regs.pci();
    let nodes = pci::tree();
    let i = nodes.iter().position(|n| n.loc == f.loc())?;
    Some(PciIdentity {
        domain: f.segment,
        bus: f.addr.bus,
        slot: f.addr.dev,
        func: f.addr.func,
        vendor: f.vendor,
        device: f.device,
        subsystem_vendor: f.subsystem.0,
        subsystem_device: f.subsystem.1,
        revision: f.revision,
        class: f.class,
        driver: "amdgpu",
        place: Place::Pci(pci::sysfs::dir(&nodes, i)),
    })
}
