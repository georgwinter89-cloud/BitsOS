//! GX-8a in-kernel tests of the memory half of the hardware backend: a `HardwareGpu` on the fake chain's memory (the
//! bring-up's chain as the live tool runs it, `Until::Backend`, the same owner and watcher) or, where the carve-out
//! must fill up, on a sparse memory of the same shape. Every mapping is walked page by page with GR-1's software
//! walker through the CPU's view of the tables, against the object's frames and Linux's flags. Nothing touches a
//! device or a screen: all Safe, rerunnable, also while the real backend is up (its own node slot, its own minors).

use crate::bo::{BoManager, Placement, Reserved};
use crate::gpucore::disc::Discovery;
use crate::gpucore::fakechip::{self, StartAs};
use crate::gpucore::fwload;
use crate::gpucore::gfxring::{GfxAddrs, Variant};
use crate::gpucore::sdma::SdmaAddrs;
use crate::gpucore::sdmafake::HostMem;
use crate::gpucore::vmring::{VmAddrs, VmInputs};
use crate::gpucore::psp::{Hw, Until};
use crate::gpucore::pspfake::{self, Hostile, MapSource};
use crate::gpucore::ucode::ListOpts;
use crate::hw::chain::{Chain, ChainOwner};
use crate::hw::chip::{self, Started};
use crate::hw::jobs::JobCfg;
use crate::hw::bo::WIPE_CHUNK;
use crate::hw::heap::{CarveIo, Heaps, MemKind, SlabSource, Window};
use crate::hw::vm::{HwVm, KERNEL_VMIDS, VmJob};
use crate::hw::{HardwareGpu, Inputs, RegIo, SysNote};
use crate::ipmod::gfx11::R;
use crate::ktests_bringup::{GOP, Owner, one_variant};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use amdgpu_fmt::addr::Carveout;
use amdgpu_fmt::pte::{Mtype, Target};
use amdgpu_fmt::vm::Level;
use amdgpu_fmt::walk::{Leaf, Walk};
use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use drm::device::{self, Driver, PciIdentity, Place};
use drm::dispatch::Caller;
use drm::dma_buf::Exporter;
use drm::fence::{self, Fence};
use drm::file::MinorKind;
use drm::gpu::{BoPlace, GpuDevice, GpuVm, PtOp};
use drm::node::{OpenDrm, open_device};
use drm::uapi::*;
use drm::wire::Wire;
use ksync::SpinLock;
use ktest::kassert;
use mm::dma_pages::PinOwner;

const P: u64 = 4096;
const M: u64 = 1 << 20;
const G: u64 = 1 << 30;
/// Below 4 GiB of the mapped space nothing in these tests maps.
const V: u64 = 1 << 34;
const RW: u32 = AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_WRITEABLE;
const MC_BASE: u64 = 0x80_0000_0000;
const PHYS_BASE: u64 = 0x8_4000_0000;
const KERNEL: Caller = Caller { pid: 0, sys_admin: true, sys_nice: true };
const BASE: u64 = 0x1000_0000;
/// GB_ADDR_CONFIG as a chip answers it (the fake's register reads 0).
const GB: u32 = 0x242;

fn pci() -> PciIdentity {
    PciIdentity { domain: 0, bus: 0xc7, slot: 0, func: 0, vendor: 0x1002, device: 0x1900, subsystem_vendor: 0x1002, subsystem_device: 0x0124, revision: 0xd5, class: 0x03_00_00, driver: "amdgpu", place: Place::Own }
}

// ---- the fake chain's memory and registers

use crate::ktests_hw_reset::{FakeReset, Shared};

/// The carve-out through the fake chain's owner, so the scanout watcher and the fake's TMR check see every store.
struct ChainIo {
    owner: Shared,
    carve: Carveout,
    slabs: Arc<SpinLock<Vec<Range<u64>>>>,
}

struct ChainWindow {
    owner: Shared,
    mc: u64,
    len: u64,
}

impl ChainWindow {
    fn at(&self, off: u64, n: u64) -> Option<u64> {
        (off.checked_add(n)? <= self.len).then_some(self.mc + off)
    }
}

impl Window for ChainWindow {
    fn read64(&self, off: u64) -> Option<u64> {
        let mut b = [0u8; 8];
        self.read(off, &mut b).then(|| u64::from_le_bytes(b))
    }
    fn write64(&self, off: u64, value: u64) -> bool {
        self.at(off, 8).is_some_and(|mc| self.owner.lock().hw().mem_write64(mc, value))
    }
    fn read(&self, off: u64, out: &mut [u8]) -> bool {
        self.at(off, out.len() as u64).is_some_and(|mc| self.owner.lock().hw().mem_read(mc, out))
    }
    fn write(&self, off: u64, data: &[u8]) -> bool {
        self.at(off, data.len() as u64).is_some_and(|mc| self.owner.lock().hw().mem_write(mc, data))
    }
    fn fill(&self, off: u64, len: u64, value: u64) -> bool {
        if value == 0 && off % P == 0 && len % P == 0 {
            return self.at(off, len).is_some_and(|mc| self.owner.lock().hw().hw().wipe(mc, len));
        }
        (0..len / 8).all(|i| self.write64(off + i * 8, value))
    }
}

impl CarveIo for ChainIo {
    fn map(&self, r: Range<u64>, _kind: MemKind) -> Option<Arc<dyn Window>> {
        self.slabs.lock().push(r.clone());
        Some(Arc::new(ChainWindow { owner: self.owner.clone(), mc: self.carve.mc_base() + r.start, len: r.end - r.start }))
    }
    fn barrier(&self) {
        self.owner.lock().hw().store_barrier();
    }
    fn mappable(&self) -> bool {
        false
    }
}

struct ChainRegs(Shared);

impl RegIo for ChainRegs {
    fn read(&self, byte: u32) -> Option<u32> {
        self.0.lock().hw().reg_read(byte)
    }
}

/// A backend on a passed fake chain.
pub(crate) struct Rig {
    pub(crate) gpu: Arc<HardwareGpu>,
    owner: Shared,
    /// The carve-out ranges in use before the backend (reserved, the bring-up's BOs).
    kernel: Vec<Range<u64>>,
    /// The windows the backend mapped.
    slabs: Arc<SpinLock<Vec<Range<u64>>>>,
    /// GX-8b: what the job half needs of the chain.
    pub(crate) vmi: VmInputs,
    /// GX-8b: this many looks at the chain find its lock taken.
    pub(crate) chain_busy: Arc<AtomicU32>,
    /// GX-7n: the IH ring's buffers come from here, as the chip's from its manager.
    bos: Arc<SpinLock<BoManager>>,
    carve: Carveout,
    /// GX-9a: the chain's full reset and a test's switches on it.
    pub(crate) reset: Arc<FakeReset>,
}

/// GX-7n: a running IH ring on the fake chain.
pub(crate) struct IhRig {
    pub(crate) ih: Arc<crate::hw::ih::Ih>,
    pub(crate) a: crate::gpucore::ih::IhAddrs,
    pub(crate) found: crate::gpucore::ih::Found,
    pub(crate) place: crate::gpucore::ih::IhPlace,
}

/// GX-7n: the ring past the chain's lock in the live form; the fake has one owner, so here it is that lock.
struct FakeIhIo(Shared, crate::gpucore::ih::IhAddrs);

impl crate::hw::ih::IhIo for FakeIhIo {
    fn entry(&self, off: u32, out: &mut [u32; 8]) -> bool {
        self.0.lock().hw().hw().ih_entry(off, out)
    }
    fn wptr_wb(&self) -> Option<u32> {
        self.0.lock().hw().hw().ih_wptr_wb()
    }
    fn wptr_reg(&self) -> Option<u32> {
        self.0.lock().hw().hw().reg_read(self.1.wptr)
    }
    fn cntl(&self) -> Option<u32> {
        self.0.lock().hw().hw().reg_read(self.1.cntl)
    }
    fn set_cntl(&self, v: u32) -> bool {
        self.0.lock().hw().hw().reg_write(self.1.cntl, v)
    }
    fn set_rptr(&self, v: u32) -> bool {
        self.0.lock().hw().hw().doorbell_write64(crate::ipmod::ih6::DOORBELL_DW, u64::from(v))
    }
}

/// GX-8b: the fake chain as the job half sees it. GX-9a: its full reset.
struct FakeChain(Shared, Arc<AtomicU32>, Arc<FakeReset>);

impl Chain for FakeChain {
    fn with(&self, f: &mut dyn FnMut(&mut dyn ChainOwner)) -> bool {
        // Only the job thread looks while a test has this set.
        let busy = self.1.load(Ordering::Acquire);
        if busy > 0 {
            self.1.store(busy - 1, Ordering::Release);
            return false;
        }
        let mut o = self.0.lock();
        f(&mut **o);
        true
    }
    fn suspend_gc(&self, lines: &mut Vec<String>) -> bool {
        self.2.suspend(&self.0, lines)
    }
    fn mode2_reset(&self, lines: &mut Vec<String>) -> Option<bool> {
        self.2.mode2(&self.0, lines)
    }
    fn reinit_gc(&self, lines: &mut Vec<String>) -> Option<crate::hw::chain::Reinit> {
        self.2.reinit(&self.0, lines)
    }
    fn reset_abandoned(&self, gone: bool) {
        self.2.knobs.abandoned.store(gone, Ordering::Release);
    }
    fn proven_halted(&self) {
        self.2.knobs.proven.fetch_add(1, Ordering::AcqRel);
    }
}

/// GX-8b: the GTT objects' real frames as the fake engines' memory: a frame whose object is gone faults, as nothing may
/// reach it.
struct SysFrames(SpinLock<BTreeMap<u64, (Weak<dyn Exporter>, u64)>>);

impl SysFrames {
    fn at(&self, phys: u64) -> Option<(Arc<dyn Exporter>, u64)> {
        let g = self.0.lock();
        let (w, page) = g.get(&(phys / P))?;
        Some((w.upgrade()?, page * P + phys % P))
    }
}

impl SysNote for SysFrames {
    fn made(&self, mem: Arc<dyn Exporter>) {
        let mut g = self.0.lock();
        g.retain(|_, (w, _)| w.strong_count() > 0);
        for k in 0..mem.size() / P {
            if let Some(phys) = mem.page_phys(k) {
                g.insert(phys / P, (Arc::downgrade(&mem), k));
            }
        }
    }
}

impl HostMem for SysFrames {
    fn read(&self, phys: u64, out: &mut [u8]) -> bool {
        self.at(phys).is_some_and(|(m, off)| m.read(off, out))
    }
    fn write(&self, phys: u64, data: &[u8]) -> bool {
        self.at(phys).is_some_and(|(m, off)| m.write(off, data))
    }
}

impl Rig {
    pub(crate) fn chain(d: &Discovery, size: u64) -> Result<Rig, String> {
        let (mut out, owner, bo, _) = one_variant(d, size, StartAs::Cold, Until::Backend, true, Hostile::None, &ListOpts::PSP13_0_4, &[], GOP, Variant::DEFAULT, &|_| {})?;
        kassert!(out.passed(), "the chain failed: {:?}", out.checks.iter().filter(|c| !c.pass).map(|c| format!("{}: {}", c.name, c.why)).collect::<Vec<_>>());
        kassert!(out.vm.is_none(), "until=backend runs no VM stage");
        let kept = out.kept.take();
        let r = Rig::on_chain(d, size, &out, owner, bo)?;
        r.reset.keep_psp(kept);
        Ok(r)
    }

    /// GX-8c: the backend on a chain that passed, however it came to run.
    pub(crate) fn on_chain(d: &Discovery, size: u64, out: &crate::gpucore::pspbringup::Outcome, owner: Box<Owner>, bo: BoManager) -> Result<Rig, String> {
        let kernel: Vec<Range<u64>> = bo.used().collect();
        let mut owner: Box<Owner> = owner;
        let frames = Arc::new(SysFrames(SpinLock::named("gx.ktest.frames", BTreeMap::new())));
        let (fb_mc, fb_phys) = {
            let f = owner.hw().hw();
            f.sdma.host = Some(frames.clone());
            (f.fb_mc, f.fb_phys)
        };
        let carve = Carveout::new(fb_mc, fb_phys, size).ok_or("carve-out")?;
        let addrs = VmAddrs::from_discovery(d).ok_or("no VM addresses")?;
        let vmi = crate::gpucore::pspbringup::vm_inputs(addrs, carve, SdmaAddrs::from_discovery(d), out).ok_or("no VM inputs")?;
        let (fw, _) = fwload::load(&pspfake::plan_780m().files(), &mut MapSource(pspfake::synthetic_780m()), &ListOpts::PSP13_0_4).map_err(|e| format!("{e:?}"))?;
        let profile = drm::profile_780m::RADEON_780M;
        let pmfw = out.gfx.as_ref().and_then(|g| g.pmfw);
        kassert!(pmfw.is_some(), "the chain's GC stage did not ask PMFW for its version");
        let firmware = chip::with_smc(chip::with_mes(chip::loaded_firmware(&profile, &fw), (Some(0x0102_508b), Some(0x0603_8110))), pmfw);
        let a = GfxAddrs::from_discovery(d).ok_or("GFX addresses")?;
        let s = Started { device: 0x1900, revision: 0xd5, gfx: true, compute: true, sdma: true };
        let facts = chip::read_facts(&mut *owner, &a, &profile, GB, s, firmware)?;
        let owner: Shared = Arc::new(SpinLock::named("gx.ktest.owner", owner));
        let bos = Arc::new(SpinLock::named("gx.ktest.bos", bo));
        let slabs = Arc::new(SpinLock::named("gx.ktest.slabs", Vec::new()));
        let heaps = Heaps::new(carve, Arc::new(ChainIo { owner: owner.clone(), carve, slabs: slabs.clone() }), Box::new(bos.clone()));
        let gpu = HardwareGpu::new(Inputs { heaps, regs: Arc::new(ChainRegs(owner.clone())), tsc: chip::tsc_regs(d), profile, facts, pci: pci(), gtt_total: 4 * G, sys_note: Some(frames) })?;
        let reset = Arc::new(FakeReset::new(d, size, bos.clone(), slabs.clone(), vmi));
        Ok(Rig { gpu, owner, kernel, slabs, vmi, chain_busy: Arc::new(AtomicU32::new(0)), bos, carve, reset })
    }

    /// GX-8b: the job half on the fake chain, with GR-2's interpreters as the engines.
    pub(crate) fn start_jobs(&self, cfg: JobCfg) -> Result<(), String> {
        self.gpu.start_jobs(self.fake_chain(), self.vmi, cfg)
    }

    /// GX-8b's chain, for a test that starts the job half itself.
    pub(crate) fn fake_chain(&self) -> Arc<dyn Chain> {
        Arc::new(FakeChain(self.owner.clone(), self.chain_busy.clone(), self.reset.clone()))
    }

    /// GX-7n: the IH ring, as `psplive::ih_up` starts it.
    pub(crate) fn start_ih(&self, d: &Discovery) -> Result<IhRig, String> {
        use crate::gpucore::ih as chip;
        let (_, range) = chip::alloc_bo(&mut *self.bos.lock()).map_err(|e| format!("{e:?}"))?;
        let place = chip::IhPlace::in_carve(self.carve, range.start).ok_or("the buffer")?;
        self.start_ih_at(d, place)
    }

    /// The same on a buffer that stays (the next kernel, a later start).
    pub(crate) fn start_ih_at(&self, d: &Discovery, place: crate::gpucore::ih::IhPlace) -> Result<IhRig, String> {
        use crate::gpucore::ih as chip;
        let a = chip::IhAddrs::from_discovery(d).ok_or("no IH addresses")?;
        let line = crate::gpucore::ihfake::TestLine::new();
        let fire = line.clone();
        self.fake(|f| f.ih.msi = Some(Arc::new(move || fire.fire())));
        let io = Arc::new(FakeIhIo(self.owner.clone(), a));
        let ih = crate::hw::ih::Ih::start(crate::hw::ih::IrqFeed::new(), io, line, crate::hw::ih::FloodCfg::default())?;
        let found = self.with_owner(|o| chip::start(o, &a, &place, true)).map_err(|e| format!("{e:?}"))?;
        ih.ring_on();
        self.with_owner(|o| chip::sources(o, &a, true)).map_err(|e| format!("{e:?}"))?;
        Ok(IhRig { ih, a, found, place })
    }

    /// GX-7n: as `psplive::ih_down`.
    pub(crate) fn stop_ih(&self, x: &IhRig) -> Result<(), String> {
        x.ih.ring_off();
        let (ok, lines) = self.with_owner(|o| crate::gpucore::ih::stop(o, &x.a, &x.found));
        x.ih.stop();
        if ok { Ok(()) } else { Err(format!("{lines:?}")) }
    }

    /// GX-9b: clocks and GFXOFF on the fake chain, with no thread: the test's `tick` runs on the fake's clock.
    pub(crate) fn start_dpm(&self) -> Result<(Arc<crate::dpm::Dpm>, crate::dpm::Wire), String> {
        let d = fakechip::discovery_780m();
        let w = crate::dpm::Wire { smu: crate::ipmod::smu13::Addrs::from_discovery(&d).ok_or("no SMU addresses")?, gc: crate::gpucore::gfxup::GcAddrs::from_discovery(&d).ok_or("no GC addresses")? };
        let dpm = self.gpu.start_dpm(w, crate::dpm::DpmCfg { thread: false }).ok_or("no job half")?;
        Ok((dpm, w))
    }

    /// The whole ring of a queue (`mc`, 2048 dwords) as it stands, and a look at the fake.
    pub(crate) fn ring_words(&self, mc: u64) -> Vec<u32> {
        let mut b = alloc::vec![0u8; 2048 * 4];
        let _ = self.owner.lock().hw().mem_read(mc, &mut b);
        b.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
    }

    /// GX-8c: the chain's owner, as `until=stop` needs it.
    pub(crate) fn with_owner<T>(&self, f: impl FnOnce(&mut Owner) -> T) -> T {
        f(&mut self.owner.lock())
    }

    pub(crate) fn fake<T>(&self, f: impl FnOnce(&mut crate::gpucore::pspfake::FakeGpu) -> T) -> T {
        f(self.owner.lock().hw().hw())
    }

    /// GX-8b: the test's job half ends first: nothing runs on the rings when the writes are counted.
    pub(crate) fn finish_jobs(self) -> ktest::TestResult {
        drop(self.gpu.stop_jobs());
        self.finish()
    }

    /// Nothing outside the backend's own slabs was written, and nothing the display reads.
    pub(crate) fn finish(self) -> ktest::TestResult {
        for r in self.slabs.lock().iter() {
            kassert!(r.start % (2 * M) == 0 && r.end % (2 * M) == 0, "slab {r:x?} is not whole pieces");
            kassert!(!meets(r, &self.kernel), "slab {r:x?} meets the scanout, the PSP, MES or a ring: {:x?}", self.kernel);
        }
        let mut o = self.owner.lock();
        kassert!(o.hw().total() == 0, "the watcher saw {} writes", o.hw().total());
        let f = o.hw().hw();
        kassert!(f.display_writes.is_empty(), "display registers written: {:x?}", f.display_writes);
        kassert!(f.scanout_writes.is_empty(), "scanout memory written: {:x?}", f.scanout_writes);
        kassert!(f.tmr_writes.is_empty(), "TMR written: {:x?}", f.tmr_writes);
        Ok(())
    }
}

// ---- a sparse carve-out, for tests that fill it

#[derive(Default)]
struct SparseMem {
    pages: BTreeMap<u64, Box<[u8]>>,
}

/// What a sparse carve-out watches.
#[derive(Clone)]
struct Watch {
    /// Makes every store fail.
    fail: Arc<AtomicBool>,
    /// Every window ever mapped.
    kinds: Arc<SpinLock<Vec<(Range<u64>, MemKind)>>>,
    /// The windows mapped now.
    live: Arc<SpinLock<Vec<Range<u64>>>>,
    /// A range had two windows at once, or went back to the manager with a window on it.
    alias: Arc<AtomicBool>,
    /// The largest single fill.
    max_fill: Arc<AtomicU64>,
    /// A store no barrier followed yet (a write-combining buffer that may not have drained).
    dirty: Arc<AtomicBool>,
}

impl Watch {
    fn new() -> Watch {
        Watch {
            fail: Arc::new(AtomicBool::new(false)),
            kinds: Arc::new(SpinLock::named("gx.ktest.kinds", Vec::new())),
            live: Arc::new(SpinLock::named("gx.ktest.live", Vec::new())),
            alias: Arc::new(AtomicBool::new(false)),
            max_fill: Arc::new(AtomicU64::new(0)),
            dirty: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Memory of any size, with only touched non-zero pages kept.
struct SparseIo {
    mem: Arc<SpinLock<SparseMem>>,
    watch: Watch,
}

struct SparseWindow {
    mem: Arc<SpinLock<SparseMem>>,
    watch: Watch,
    start: u64,
    len: u64,
}

impl Drop for SparseWindow {
    fn drop(&mut self) {
        self.watch.live.lock().retain(|r| r.start != self.start);
    }
}

/// The manager, checking that what comes back has no window on it any more.
struct WatchedSource {
    bos: Arc<SpinLock<BoManager>>,
    watch: Watch,
}

impl SlabSource for WatchedSource {
    fn take(&self, bytes: u64, align: u64) -> Option<(u32, Range<u64>)> {
        self.bos.take(bytes, align)
    }
    fn give(&self, handle: u32) {
        let r = self.bos.lock().get(handle).map(|b| b.range.clone());
        if r.is_some_and(|r| meets(&r, &self.watch.live.lock())) {
            self.watch.alias.store(true, Ordering::Release);
        }
        self.bos.give(handle);
    }
    fn free_bytes(&self) -> u64 {
        SlabSource::free_bytes(&self.bos)
    }
}

impl SparseWindow {
    fn io(&self, off: u64, n: usize, f: &mut dyn FnMut(&mut SparseMem, u64, usize, usize)) -> bool {
        let Some(end) = off.checked_add(n as u64).filter(|&e| e <= self.len) else { return false };
        let mut g = self.mem.lock();
        let mut at = self.start + off;
        let stop = self.start + end;
        let mut done = 0;
        while at < stop {
            let k = ((P - at % P).min(stop - at)) as usize;
            f(&mut g, at, done, k);
            at += k as u64;
            done += k;
        }
        true
    }
}

impl Window for SparseWindow {
    fn read64(&self, off: u64) -> Option<u64> {
        let mut b = [0u8; 8];
        self.read(off, &mut b).then(|| u64::from_le_bytes(b))
    }
    fn write64(&self, off: u64, value: u64) -> bool {
        self.write(off, &value.to_le_bytes())
    }
    fn read(&self, off: u64, out: &mut [u8]) -> bool {
        self.io(off, out.len(), &mut |m, at, into, k| {
            let dst = &mut out[into..into + k];
            match m.pages.get(&(at / P)) {
                Some(p) => dst.copy_from_slice(&p[(at % P) as usize..(at % P) as usize + k]),
                None => dst.fill(0),
            }
        })
    }
    fn write(&self, off: u64, data: &[u8]) -> bool {
        if self.watch.fail.load(Ordering::Acquire) {
            return false;
        }
        self.watch.dirty.store(true, Ordering::Release);
        self.io(off, data.len(), &mut |m, at, into, k| {
            let p = m.pages.entry(at / P).or_insert_with(|| alloc::vec![0u8; P as usize].into_boxed_slice());
            p[(at % P) as usize..(at % P) as usize + k].copy_from_slice(&data[into..into + k]);
        })
    }
    /// Zero drops whole pages: a filled carve-out costs nothing.
    fn fill(&self, off: u64, len: u64, value: u64) -> bool {
        if self.watch.fail.load(Ordering::Acquire) {
            return false;
        }
        self.watch.dirty.store(true, Ordering::Release);
        self.watch.max_fill.fetch_max(len, Ordering::AcqRel);
        if value == 0 && off % P == 0 && len % P == 0 && off + len <= self.len {
            let mut g = self.mem.lock();
            let first = (self.start + off) / P;
            let keys: Vec<u64> = g.pages.range(first..first + len / P).map(|(k, _)| *k).collect();
            for k in keys {
                g.pages.remove(&k);
            }
            return true;
        }
        (0..len / 8).all(|i| self.write64(off + i * 8, value))
    }
}

impl CarveIo for SparseIo {
    fn map(&self, r: Range<u64>, kind: MemKind) -> Option<Arc<dyn Window>> {
        self.watch.kinds.lock().push((r.clone(), kind));
        let mut live = self.watch.live.lock();
        if meets(&r, &live) {
            self.watch.alias.store(true, Ordering::Release);
        }
        live.push(r.clone());
        Some(Arc::new(SparseWindow { mem: self.mem.clone(), watch: self.watch.clone(), start: r.start, len: r.end - r.start }))
    }
    fn barrier(&self) {
        self.watch.dirty.store(false, Ordering::Release);
    }
    fn mappable(&self) -> bool {
        false
    }
}

struct NoRegs;

impl RegIo for NoRegs {
    fn read(&self, _byte: u32) -> Option<u32> {
        None
    }
}

struct Sparse {
    gpu: Arc<HardwareGpu>,
    bos: Arc<SpinLock<BoManager>>,
    watch: Watch,
    kernel: Vec<Range<u64>>,
}

/// A backend on `size` bytes with a GOP surface, the discovery blob and two kernel BOs in the manager.
fn sparse(size: u64) -> Result<Sparse, String> {
    let reserved = alloc::vec![Reserved { name: String::from("gop"), range: 0..8 * M }, Reserved { name: String::from("discovery"), range: size - 64 << 10..size }];
    let mut m = BoManager::new(size, reserved);
    // Like the bring-up's buffers: scattered so a slab must find its way around them.
    m.create(2 * M, Placement::Vram, 2 * M, None).map_err(|e| format!("{e:?}"))?;
    let ring = m.create(4 * M, Placement::Vram, 2 * M, None).map_err(|e| format!("{e:?}"))?;
    m.create(M, Placement::Vram, 2 * M, None).map_err(|e| format!("{e:?}"))?;
    m.release(ring);
    let kernel: Vec<Range<u64>> = m.used().collect();
    let bos = Arc::new(SpinLock::named("gx.ktest.bos", m));
    let watch = Watch::new();
    let io = SparseIo { mem: Arc::new(SpinLock::named("gx.ktest.mem", SparseMem::default())), watch: watch.clone() };
    let carve = Carveout::new(MC_BASE, PHYS_BASE, size).ok_or("carve-out")?;
    let heaps = Heaps::new(carve, Arc::new(io), Box::new(WatchedSource { bos: bos.clone(), watch: watch.clone() }));
    let profile = drm::profile_780m::RADEON_780M;
    let facts = chip::ChipFacts { dev_info: profile.dev_info, firmware: Vec::new(), rings: [0; drm::gpu::HW_IP_NUM], gb_addr_config: 0x242 };
    let gpu = HardwareGpu::new(Inputs { heaps, regs: Arc::new(NoRegs), tsc: None, profile, facts, pci: pci(), gtt_total: 64 * M, sys_note: None })?;
    Ok(Sparse { gpu, bos, watch, kernel })
}

fn meets(r: &Range<u64>, others: &[Range<u64>]) -> bool {
    others.iter().any(|o| o.start < r.end && r.start < o.end)
}

// ---- the uAPI, as a program calls it

struct KMem(SpinLock<Vec<u8>>);

impl KMem {
    fn new() -> KMem {
        KMem(SpinLock::named("gx.ktest.umem", alloc::vec![0; 4096]))
    }
    fn put<T: Wire>(&self, v: &T) {
        let mut b = alloc::vec![0u8; T::SIZE];
        let _ = v.encode(&mut b);
        self.0.lock()[..b.len()].copy_from_slice(&b);
    }
    fn take<T: Wire>(&self) -> Option<T> {
        T::decode(&self.0.lock()[..T::SIZE])
    }
}

impl vfs::sys::UserMem for KMem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let Some(s) = addr.checked_sub(BASE).and_then(|a| usize::try_from(a).ok()) else { return false };
        let g = self.0.lock();
        let Some(r) = s.checked_add(buf.len()).and_then(|e| g.get(s..e)) else { return false };
        buf.copy_from_slice(r);
        true
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        let Some(s) = addr.checked_sub(BASE).and_then(|a| usize::try_from(a).ok()) else { return false };
        let mut g = self.0.lock();
        let Some(r) = s.checked_add(data.len()).and_then(|e| g.get_mut(s..e)) else { return false };
        r.copy_from_slice(data);
        true
    }
    fn read_cstr(&self, _addr: u64, _max: usize) -> Result<Vec<u8>, vfs::Errno> {
        Err(linux::errno::EFAULT)
    }
}

fn gem(f: &OpenDrm, m: &KMem, size: u64, domains: u32, flags: u32) -> Result<u32, String> {
    let mut c = drm_amdgpu_gem_create::ZERO;
    c.set_in(&drm_amdgpu_gem_create_in { bo_size: size, alignment: 0, domains: u64::from(domains), domain_flags: u64::from(flags) });
    m.put(&c);
    f.ioctl_as(DRM_IOCTL_AMDGPU_GEM_CREATE, BASE, m, KERNEL).map_err(|e| format!("GEM_CREATE {size} {domains:#x}: errno {e}"))?;
    Ok(m.take::<drm_amdgpu_gem_create>().ok_or("decode")?.out().handle)
}

#[allow(clippy::too_many_arguments)]
fn va(f: &OpenDrm, m: &KMem, h: u32, op: u32, flags: u32, addr: u64, off: u64, size: u64) -> Result<(), String> {
    m.put(&drm_amdgpu_gem_va { handle: h, operation: op, flags, va_address: addr, offset_in_bo: off, map_size: size, ..drm_amdgpu_gem_va::ZERO });
    f.ioctl_as(DRM_IOCTL_AMDGPU_GEM_VA, BASE, m, KERNEL).map(|_| ()).map_err(|e| format!("GEM_VA op {op} at {addr:#x}: errno {e}"))
}

fn vm_op(f: &OpenDrm, m: &KMem, op: u32) -> Result<i64, vfs::Errno> {
    m.put(&drm_amdgpu_vm_in { op, flags: 0 });
    f.ioctl_as(DRM_IOCTL_AMDGPU_VM, BASE, m, KERNEL)
}

fn hw_vm(f: &OpenDrm) -> Result<Arc<dyn GpuVm>, String> {
    f.drm_file().vm.backend_vm().ok_or_else(|| String::from("no VM"))
}

fn as_hw(v: &Arc<dyn GpuVm>) -> Result<&HwVm, String> {
    v.as_any().and_then(|a| a.downcast_ref::<HwVm>()).ok_or_else(|| String::from("not the hardware VM"))
}

/// What a page must walk to.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Want {
    Page { phys: u64, target: Target, read: bool, write: bool, exec: bool, mtype: Mtype },
    Prt,
}

/// The model of a VM: page number -> what it must walk to; a page not in it is a hole.
#[derive(Default)]
struct Model(BTreeMap<u64, Want>);

impl Model {
    fn map(&mut self, f: &OpenDrm, h: u32, at: u64, off: u64, size: u64, flags: u32, target: Target, mtype: Mtype) -> Result<(), String> {
        let bo = f.drm_file().bo(h).ok_or("no object")?;
        for k in 0..size / P {
            let phys = bo.memory().page_phys(off / P + k).ok_or("no frame")?;
            let w = Want::Page { phys, target, read: flags & AMDGPU_VM_PAGE_READABLE != 0, write: flags & AMDGPU_VM_PAGE_WRITEABLE != 0, exec: flags & AMDGPU_VM_PAGE_EXECUTABLE != 0, mtype };
            self.0.insert(at / P + k, w);
        }
        Ok(())
    }
    fn clear(&mut self, at: u64, size: u64) {
        for k in 0..size / P {
            self.0.remove(&(at / P + k));
        }
    }
    /// Every page of the model, and the pages around each of its ranges.
    fn check(&self, vm: &HwVm, what: &str) -> ktest::TestResult {
        let mut last = None;
        for (&pg, want) in &self.0 {
            let w = vm.walk(pg * P + 0x18).map_err(|e| format!("{what}: page {pg:#x}: {e:?}"))?;
            match (want, &w) {
                (Want::Page { phys, target, read, write, exec, mtype }, Walk::Page(Leaf { phys: got, target: t, flags, .. }))
                    if got == &(phys + 0x18) && t == target && flags.read == *read && flags.write == *write && flags.exec == *exec && flags.mtype == *mtype => {}
                (Want::Prt, Walk::Prt { .. }) => {}
                _ => return Err(format!("{what}: page {pg:#x}: {w:?}, want {want:?}")),
            }
            if last != Some(pg.wrapping_sub(1)) && !self.0.contains_key(&(pg.wrapping_sub(1))) {
                let below = vm.walk((pg - 1) * P).map_err(|e| format!("{what}: below {pg:#x}: {e:?}"))?;
                kassert!(matches!(below, Walk::Hole { .. }), "{what}: the page below {pg:#x}: {below:?}");
            }
            if !self.0.contains_key(&(pg + 1)) {
                let above = vm.walk((pg + 1) * P).map_err(|e| format!("{what}: above {pg:#x}: {e:?}"))?;
                kassert!(matches!(above, Walk::Hole { .. }), "{what}: the page above {pg:#x}: {above:?}");
            }
            last = Some(pg);
        }
        Ok(())
    }
}

fn register(gpu: &Arc<HardwareGpu>) -> Result<Arc<device::Device>, String> {
    device::register(gpu.clone() as Arc<dyn Driver>).map_err(|e| format!("register: errno {e}"))
}

// ---- the checks

/// INFO's facts come from the chip and the carve-out, for any size.
pub(crate) fn check_info(d: &Discovery) -> ktest::TestResult {
    for size in [512 * M, 8 * G, 16 * G] {
        let rig = Rig::chain(d, size)?;
        let what = format!("{} MiB", size >> 20);
        let m = rig.gpu.memory();
        let kernel: u64 = rig.kernel.iter().map(|r| r.end - r.start).sum();
        kassert!(m.vram_total == size && m.vram_visible == size, "{what}: {m:?}");
        kassert!(m.vram_pinned >= kernel && m.vram_pinned < size, "{what}: pinned {:#x}, kernel {kernel:#x}", m.vram_pinned);
        kassert!(m.vram_usage >= kernel, "{what}: usage {:#x}, kernel {kernel:#x}", m.vram_usage);
        kassert!(m.gtt_total == 4 * G && m.gtt_usage == 0, "{what}: GTT {m:?}");
        let i = rig.gpu.dev_info();
        kassert!(i.cu_active_number == 12 && i.cu_bitmap[0][0] == 0x3f && i.cu_bitmap[0][1] == 0x3f, "{what}: CUs {} {:x?}", i.cu_active_number, i.cu_bitmap);
        kassert!(i.enabled_rb_pipes_mask == 0xf && i.num_rb_pipes == 4, "{what}: RBs {:#x} {}", i.enabled_rb_pipes_mask, i.num_rb_pipes);
        kassert!(i.ids_flags & 4 == 0, "{what}: TMZ still offered");
        for (ty, n) in [(AMDGPU_HW_IP_GFX, 1), (AMDGPU_HW_IP_COMPUTE, 1), (AMDGPU_HW_IP_DMA, 1), (AMDGPU_HW_IP_VCN_ENC, 0), (AMDGPU_HW_IP_VCN_JPEG, 0)] {
            kassert!(rig.gpu.rings_ready(ty) == n, "{what}: rings of type {ty}: {}", rig.gpu.rings_ready(ty));
        }
        let fw = |t, i| rig.gpu.firmware(t, i).map(|f| (f.ver, f.feature));
        kassert!(fw(AMDGPU_INFO_FW_MES, 0) == Some((0x8b, 1)) && fw(AMDGPU_INFO_FW_MES_KIQ, 0) == Some((0x110, 6)), "{what}: MES {:?}", fw(AMDGPU_INFO_FW_MES, 0));
        kassert!(fw(AMDGPU_INFO_FW_VCN, 0) == Some((0, 0)), "{what}: VCN was not loaded: {:?}", fw(AMDGPU_INFO_FW_VCN, 0));
        kassert!(fw(0xdead, 0).is_none(), "{what}: an unknown firmware type answers");
        let gb = rig.gpu.profile().registers.iter().find(|r| r.offset == drm::profile_780m::GB_ADDR_CONFIG).copied().ok_or("no GB_ADDR_CONFIG in the list")?;
        kassert!(rig.gpu.read_register(&gb, None, None) == Some(GB), "{what}: GB_ADDR_CONFIG {:?}", rig.gpu.read_register(&gb, None, None));
        // The rest of the list is read live.
        let other = rig.gpu.profile().registers.iter().find(|r| r.offset != gb.offset).copied().ok_or("the list is empty")?;
        kassert!(rig.gpu.read_register(&other, None, None).is_some(), "{what}: register {:#x}", other.offset);
        let mut idx = gb;
        idx.indexed = true;
        kassert!(rig.gpu.read_register(&idx, Some(0), Some(0)).is_none(), "{what}: an indexed register answers");
        let c0 = rig.gpu.gpu_clock_counter();
        kassert!(rig.gpu.gpu_clock_counter() >= c0, "{what}: the clock counter went back");
        kassert!(!rig.gpu.dpm_enabled() && rig.gpu.sensor(drm::gpu::Sensor::GfxSclk).is_none(), "{what}: sensors without an SMU path");
        rig.finish()?;
    }
    Ok(())
}

/// A chip with a harvested WGP and RB reports them; another chip's id is refused.
pub(crate) fn check_harvest(d: &Discovery) -> ktest::TestResult {
    let (out, owner, _, _) = one_variant(d, 512 * M, StartAs::Cold, Until::Backend, true, Hostile::None, &ListOpts::PSP13_0_4, &[], GOP, Variant::DEFAULT, &|_| {})?;
    kassert!(out.passed(), "the chain failed");
    let mut owner: Box<Owner> = owner;
    let a = GfxAddrs::from_discovery(d).ok_or("GFX addresses")?;
    let profile = drm::profile_780m::RADEON_780M;
    let s = Started { device: 0x1900, revision: 0xd5, gfx: true, compute: true, sdma: false };
    owner.reg_write(a.byte(R::GcUserShaderArrayConfig).ok_or("reg")?, 0x2 << 16);
    owner.reg_write(a.byte(R::GcUserRbBackendDisable).ok_or("reg")?, 0x8 << 4);
    let f = chip::read_facts(&mut *owner, &a, &profile, 0x242, s, Vec::new())?;
    let i = f.dev_info;
    kassert!(i.cu_active_number == 8 && i.cu_bitmap[0][0] == 0x33 && i.cu_bitmap[0][1] == 0x33, "harvested WGP 1: {} {:x?}", i.cu_active_number, i.cu_bitmap);
    kassert!(i.enabled_rb_pipes_mask == 0x7 && i.num_rb_pipes == 3, "harvested RB 3: {:#x} {}", i.enabled_rb_pipes_mask, i.num_rb_pipes);
    kassert!(f.rings[AMDGPU_HW_IP_DMA as usize] == 0 && f.rings[AMDGPU_HW_IP_GFX as usize] == 1, "rings {:?}", f.rings);
    let index = a.byte(R::GrbmGfxIndex).ok_or("reg")?;
    kassert!(owner.reg_read(index) == Some(crate::ipmod::gfx11::GFX_INDEX_BROADCAST), "GRBM_GFX_INDEX not back on broadcast");
    let other = Started { device: 0x1234, ..s };
    kassert!(chip::read_facts(&mut *owner, &a, &profile, 0x242, other, Vec::new()).is_err(), "another chip's id passes");
    Ok(())
}

/// The firmware table holds what the files say: version and feature of ME, MEC, RLC, SDMA; nothing for what no file gave.
pub(crate) fn check_firmware() -> ktest::TestResult {
    let (fw, _) = fwload::load(&pspfake::plan_780m().files(), &mut MapSource(pspfake::synthetic_780m()), &ListOpts::PSP13_0_4).map_err(|e| format!("{e:?}"))?;
    let profile = drm::profile_780m::RADEON_780M;
    let t = chip::loaded_firmware(&profile, &fw);
    kassert!(t.len() == profile.firmware.len(), "{} entries, the profile has {}", t.len(), profile.firmware.len());
    use crate::gpucore::ucode::Kind;
    for (ty, kind) in [(AMDGPU_INFO_FW_GFX_ME, Kind::Me), (AMDGPU_INFO_FW_GFX_MEC, Kind::Mec), (AMDGPU_INFO_FW_GFX_RLC, Kind::Rlc), (AMDGPU_INFO_FW_SDMA, Kind::Sdma)] {
        let e = t.iter().find(|f| f.fw_type == ty && f.index == 0).ok_or("no entry")?;
        let file = fw.get(kind).ok_or("no file")?;
        kassert!(e.ver == file.header.ucode_version, "{kind:?}: {:#x}, file {:#x}", e.ver, file.header.ucode_version);
    }
    let m = chip::with_mes(t, (Some(0x0102_508b), None));
    let e = |ty| m.iter().find(|f| f.fw_type == ty).map(|f| (f.ver, f.feature));
    kassert!(e(AMDGPU_INFO_FW_MES) == Some((0x8b, 1)) && e(AMDGPU_INFO_FW_MES_KIQ) == Some((0, 0)), "MES {:?}", e(AMDGPU_INFO_FW_MES));
    kassert!(e(AMDGPU_INFO_FW_SMC) == Some((0, 0)), "SMC before a GC stage asked: {:?}", e(AMDGPU_INFO_FW_SMC));
    let s = chip::with_smc(m.clone(), Some(crate::gpucore::gfxup::LINUX_780M_PMFW));
    let want = drm::profile_780m::RADEON_780M.firmware.iter().find(|f| f.fw_type == AMDGPU_INFO_FW_SMC).map(|f| (f.ver, f.feature));
    kassert!(s.iter().find(|f| f.fw_type == AMDGPU_INFO_FW_SMC).map(|f| (f.ver, f.feature)) == want, "SMC is not the entry Linux reports, the capture's");
    kassert!(chip::with_smc(m.clone(), None) == m, "no PMFW answer changed the table");
    Ok(())
}

/// The allocator: never the kernel's ranges, wiped, aligned, full at the size the carve-out has, tables still possible
/// when objects filled it, everything back; slabs of tables and objects never share a type.
pub(crate) fn check_vram() -> ktest::TestResult {
    for size in [512 * M, 8 * G] {
        let s = sparse(size)?;
        let what = format!("{} MiB", size >> 20);
        let owner = PinOwner::new(50);
        let mut bos: Vec<Arc<dyn Exporter>> = Vec::new();
        let chunk = size / 16;
        // A first small object, with data: its pages must read zero for the next user.
        let first = s.gpu.bo_memory(BoPlace::Vram, 16, 1, false, &owner).map_err(|e| format!("{what}: errno {e}"))?;
        kassert!(first.write(0, &[0xa5; 64]), "{what}: write");
        let base = first.page_phys(0).ok_or("no frame")?;
        drop(first);
        let again = s.gpu.bo_memory(BoPlace::Vram, 16, 1, false, &owner).map_err(|e| format!("{what}: errno {e}"))?;
        kassert!(again.page_phys(0) == Some(base), "{what}: the free range was not reused");
        let mut b = [0xffu8; 64];
        kassert!(again.read(0, &mut b) && b == [0; 64], "{what}: not wiped: {b:x?}");
        drop(again);
        while let Ok(bo) = s.gpu.bo_memory(BoPlace::Vram, chunk / P, 1, false, &owner) {
            let start = bo.page_phys(0).ok_or("no frame")? - PHYS_BASE;
            let r = start..start + bo.size();
            kassert!(!meets(&r, &s.kernel), "{what}: object {r:x?} meets a kernel range");
            kassert!(start % (2 * M) == 0, "{what}: object of {chunk:#x} bytes at {start:#x}");
            kassert!(r.end <= size - (64 << 10), "{what}: object {r:x?} in the discovery area");
            bos.push(bo);
            kassert!(bos.len() < 32, "{what}: the carve-out does not fill");
        }
        kassert!(bos.len() >= 8, "{what}: only {} objects of {chunk:#x}", bos.len());
        // Full of objects, and a program's next mapping still gets tables (no eviction to make room).
        let vm = s.gpu.new_vm(1).map_err(|e| format!("{what}: VM: errno {e}"))?;
        let scattered = s.gpu.bo_memory(BoPlace::Gtt, 4096, 1, false, &owner).map_err(|e| format!("{what}: GTT: errno {e}"))?;
        let op = PtOp::Map { va: V, pages: 4096, mem: scattered.clone(), first: 0, place: Some(BoPlace::Gtt), uswc: false, flags: RW };
        kassert!(vm.update(core::slice::from_ref(&op)).error.is_none(), "{what}: no tables with the carve-out full of objects");
        kassert!(matches!(vm.walk(V + 4095 * P), Ok(Walk::Page(_))), "{what}: the last page does not walk");
        let m = s.gpu.memory();
        kassert!(m.vram_usage <= m.vram_total && m.vram_pinned < m.vram_total, "{what}: {m:?}");
        // A slab keeps its memory type: table slabs and object slabs never overlap, and neither meets the kernel's.
        {
            let k = s.watch.kinds.lock();
            for (i, (r, kind)) in k.iter().enumerate() {
                kassert!(r.start % (2 * M) == 0 && r.end % (2 * M) == 0, "{what}: slab {r:x?} is not whole pieces");
                kassert!(!meets(r, &s.kernel), "{what}: slab {r:x?} meets a kernel range");
                kassert!(!k[i + 1..].iter().any(|(o, _)| o.start < r.end && r.start < o.end), "{what}: slab {r:x?} ({kind:?}) overlaps another");
            }
            kassert!(k.iter().any(|x| x.1 == MemKind::Tables) && k.iter().any(|x| x.1 == MemKind::User), "{what}: kinds {:?}", k.iter().map(|x| x.1).collect::<Vec<_>>());
        }
        let used = s.gpu.heaps().bytes().user_used;
        bos.clear();
        drop(vm);
        drop(scattered);
        let h = s.gpu.heaps().bytes();
        kassert!(h.user_used == 0 && h.table_used == 0 && used > 0, "{what}: not given back: {h:?}");
        // The slabs went back to the manager but for a cushion (the reserve), so the display and the PSP find room
        // after a spike; each window went before its range did.
        let keep = (size / 32).max(16 * M);
        kassert!(h.user_slabs < keep + chunk, "{what}: a spike's slabs stayed: {h:?}");
        kassert!(!s.watch.alias.load(Ordering::Acquire), "{what}: a range with a window went back, or got a second one");
        let display = s.bos.lock().create(chunk, Placement::Vram, 2 * M, None).map_err(|e| format!("{what}: the manager is empty after a spike: {e:?}"))?;
        s.bos.lock().release(display);
        // And they serve new objects.
        let bo = s.gpu.bo_memory(BoPlace::Vram, chunk / P, 1, false, &owner).map_err(|e| format!("{what}: errno {e}"))?;
        drop(bo);
        kassert!(s.gpu.bo_memory(BoPlace::Vram, (2 * size) / P, 1, false, &owner).is_err(), "{what}: an object larger than the carve-out");
    }
    Ok(())
}

/// An object over two slabs: one range for the GPU, every byte reached through the right window.
pub(crate) fn check_two_slabs() -> ktest::TestResult {
    let s = sparse(512 * M)?;
    let owner = PinOwner::new(50);
    let e = |x: i64| format!("errno {x}");
    // `sparse` left a 4 MiB hole: filled, the next slabs lie side by side.
    let hole = s.gpu.bo_memory(BoPlace::Vram, 4 * M / P, 1, false, &owner).map_err(e)?;
    let small = s.gpu.bo_memory(BoPlace::Vram, 1, 1, false, &owner).map_err(e)?;
    let at = small.page_phys(0).ok_or("no frame")?;
    drop(small);
    let big = s.gpu.bo_memory(BoPlace::Vram, 3 * M / P, 1, false, &owner).map_err(e)?;
    kassert!(big.page_phys(0) == Some(at), "the second slab did not join the first: {:x?}, {at:#x}", big.page_phys(0));
    kassert!(s.watch.kinds.lock().len() == 3, "slabs {:x?}", s.watch.kinds.lock().iter().map(|k| k.0.clone()).collect::<Vec<_>>());
    let pat: Vec<u8> = (0..64).collect();
    kassert!(big.write(2 * M - 32, &pat), "write over the seam");
    let mut b = [0u8; 64];
    kassert!(big.read(2 * M - 32, &mut b) && b[..] == pat[..], "read over the seam: {b:x?}");
    kassert!(!big.write(3 * M - 8, &[0; 16]) && !big.read(3 * M, &mut b), "an access past the end");
    drop(hole);
    Ok(())
}

/// GTT: charged, limited, scattered pages; USWC not snooped through the uAPI; pool full answers ENOMEM (the uAPI's
/// domain order decides).
pub(crate) fn check_gtt() -> ktest::TestResult {
    let s = sparse(512 * M)?;
    let owner = PinOwner::new(50);
    let a = s.gpu.bo_memory(BoPlace::Gtt, 256, 1, false, &owner).map_err(|e| format!("errno {e}"))?;
    kassert!(s.gpu.memory().gtt_usage == 256 * P, "usage {:#x}", s.gpu.memory().gtt_usage);
    kassert!(a.write(4096, &[0x5a; 16]), "write");
    let mut b = [0u8; 16];
    kassert!(a.read(4096, &mut b) && b == [0x5a; 16], "read back {b:x?}");
    kassert!(s.gpu.bo_memory(BoPlace::Gtt, 64 * M / P, 1, false, &owner).is_err(), "more than the GTT size");
    kassert!(s.gpu.memory().gtt_usage == 256 * P, "a refused object stayed charged");
    let sys = s.gpu.bo_memory(BoPlace::System, 8, 1, false, &owner).map_err(|e| format!("errno {e}"))?;
    kassert!(s.gpu.memory().gtt_usage == 256 * P, "a system object charged GTT");
    drop(sys);
    drop(a);
    kassert!(s.gpu.memory().gtt_usage == 0, "not given back");
    kassert!(s.gpu.bo_memory(BoPlace::Vram, 0, 1, false, &owner).is_err(), "an empty object");
    Ok(())
}

/// GEM_VA through the uAPI: MAP, UNMAP, REPLACE, CLEAR, PRT against a model, every page walked, on the fake chain.
pub(crate) fn check_tables(d: &Discovery) -> ktest::TestResult {
    for size in [512 * M, 8 * G] {
        let rig = Rig::chain(d, size)?;
        let what = format!("{} MiB", size >> 20);
        let dev = register(&rig.gpu)?;
        let r = tables_on(&dev, &what);
        device::unregister(&dev);
        r?;
        let h = rig.gpu.heaps().bytes();
        kassert!(h.user_used == 0 && h.table_used == 0, "{what}: not given back: {h:?}");
        kassert!(rig.gpu.vms_alive() == 0 && rig.gpu.open_files() == 0 && rig.gpu.vm_ids().pasids() == 0, "{what}: a VM, a file or a PASID left");
        rig.finish()?;
    }
    Ok(())
}

fn tables_on(dev: &Arc<device::Device>, what: &str) -> ktest::TestResult {
    let f = open_device(dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
    let m = KMem::new();
    let snooped = Target::System { snooped: true };
    let plain = Target::System { snooped: false };
    let g = gem(&f, &m, 32 * P, AMDGPU_GEM_DOMAIN_GTT, 0)?;
    let wc = gem(&f, &m, 8 * P, AMDGPU_GEM_DOMAIN_GTT, AMDGPU_GEM_CREATE_CPU_GTT_USWC)?;
    let v = gem(&f, &m, 2 * M + 16 * P, AMDGPU_GEM_DOMAIN_VRAM, 0)?;
    let cc = gem(&f, &m, 4 * P, AMDGPU_GEM_DOMAIN_GTT, 0)?;
    let mut model = Model::default();
    let x = AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_EXECUTABLE;
    // (handle, address, size, flags, target, MTYPE as gmc_v11_0.c:521 makes it).
    let cases = [
        (g, V, 32 * P, RW, snooped, Mtype::Nc),
        (wc, V + 2 * M, 8 * P, RW | AMDGPU_VM_MTYPE_WC, plain, Mtype::Wc),
        (v, V + 4 * M, 2 * M + 16 * P, RW, Target::Local, Mtype::Nc),
        (cc, V + 8 * M, 4 * P, RW | AMDGPU_VM_MTYPE_CC, snooped, Mtype::Cc),
    ];
    for &(h, at, sz, fl, t, mt) in &cases {
        va(&f, &m, h, AMDGPU_VA_OP_MAP, fl, at, 0, sz)?;
        model.map(&f, h, at, 0, sz, fl, t, mt)?;
    }
    // Executable, read-only, in the high half.
    let hi = 0xffff_8000_0000_0000u64;
    let x1 = gem(&f, &m, 16 * P, AMDGPU_GEM_DOMAIN_GTT, 0)?;
    va(&f, &m, x1, AMDGPU_VA_OP_MAP, x | AMDGPU_VM_MTYPE_UC, hi + (1 << 33), 0, 16 * P)?;
    model.map(&f, x1, (hi + (1 << 33)) & 0x0000_ffff_ffff_ffff, 0, 16 * P, x, snooped, Mtype::Uc)?;
    let vm = hw_vm(&f)?;
    let hw = as_hw(&vm)?;
    model.check(hw, &format!("{what} after the maps"))?;
    // The 2 MiB of VRAM use a huge leaf where address and VA allow it, as Linux's fragments do.
    let bo = f.drm_file().bo(v).ok_or("no object")?;
    match hw.walk(V + 4 * M) {
        Ok(Walk::Page(Leaf { level: Level::Pdb0, target: Target::Local, .. })) => {}
        w => return Err(format!("{what}: 2 MiB of VRAM at a 2 MiB VA: {w:?}")),
    }
    kassert!(bo.memory().page_phys(0).is_some_and(|p| p % (2 * M) == 0), "{what}: VRAM object not naturally aligned");
    // UNMAP a whole mapping.
    va(&f, &m, wc, AMDGPU_VA_OP_UNMAP, 0, V + 2 * M, 0, 8 * P)?;
    model.clear(V + 2 * M, 8 * P);
    model.check(hw, &format!("{what} after UNMAP"))?;
    // CLEAR through the middle of a mapping: the pieces stay, a hole between them.
    va(&f, &m, 0, AMDGPU_VA_OP_CLEAR, 0, V + 8 * P, 0, 4 * P)?;
    model.clear(V + 8 * P, 4 * P);
    model.check(hw, &format!("{what} after CLEAR"))?;
    // REPLACE over the rest of it with another object and other flags: the old part goes.
    let rep = gem(&f, &m, 8 * P, AMDGPU_GEM_DOMAIN_GTT, 0)?;
    va(&f, &m, rep, AMDGPU_VA_OP_REPLACE, x, V + 4 * P, 0, 8 * P)?;
    model.clear(V + 4 * P, 8 * P);
    model.map(&f, rep, V + 4 * P, 0, 8 * P, x, snooped, Mtype::Nc)?;
    model.check(hw, &format!("{what} after REPLACE"))?;
    // PRT over a whole 2 MiB block and more.
    va(&f, &m, 0, AMDGPU_VA_OP_MAP, AMDGPU_VM_PAGE_PRT, 1 << 35, 0, 2 * M + 8 * P)?;
    for k in 0..(2 * M + 8 * P) / P {
        model.0.insert((1 << 35) / P + k, Want::Prt);
    }
    model.check(hw, &format!("{what} after PRT"))?;
    // Tables written and walked: the base register holds a local PDE, as the rings loaded it on the chip.
    kassert!(hw.pd_addr() & 1 != 0 && hw.pd_addr() & 2 == 0, "{what}: pd {:#x}", hw.pd_addr());
    // GEM_CLOSE: mappings and memory go, the rest stays.
    let frames = f.drm_file().bo(g).ok_or("no object")?.memory().clone();
    let weak = Arc::downgrade(&frames);
    drop(frames);
    for h in [g, wc, v, cc, x1, rep] {
        m.put(&drm_gem_close { handle: h, pad: 0 });
        f.ioctl_as(DRM_IOCTL_GEM_CLOSE, BASE, &m, KERNEL).map_err(|e| format!("GEM_CLOSE {h}: errno {e}"))?;
    }
    kassert!(weak.upgrade().is_none(), "{what}: an object's memory outlived its close");
    model.0.retain(|_, w| *w == Want::Prt);
    model.check(hw, &format!("{what} after the closes"))?;
    drop(vm);
    drop(f);
    Ok(())
}

/// VMIDs per job, from 1-7: reuse without a flush while the VM owns the slot and its tables did not change; a flush
/// otherwise; all busy: wait; the reserved VMID for its holder only.
pub(crate) fn check_vmid() -> ktest::TestResult {
    let s = sparse(512 * M)?;
    let owner = PinOwner::new(50);
    let mem = s.gpu.bo_memory(BoPlace::Gtt, 4, 1, false, &owner).map_err(|e| format!("errno {e}"))?;
    let e = |x: i64| format!("errno {x}");
    let a = s.gpu.new_vm(1).map_err(e)?;
    let b = s.gpu.new_vm(2).map_err(e)?;
    kassert!(a.pasid() != 0 && a.pasid() != b.pasid() && a.pasid() < 1 << 16, "PASIDs {} {}", a.pasid(), b.pasid());
    let j = a.begin_job().map_err(e)?.ok_or("no VMID")?;
    kassert!(KERNEL_VMIDS.contains(&j.vmid) && j.needs_flush && j.pd == a.pd_addr() && j.pasid == a.pasid(), "first job: VMID {} flush {}", j.vmid, j.needs_flush);
    let vmid = j.vmid;
    drop(j);
    let j = a.begin_job().map_err(e)?.ok_or("no VMID")?;
    kassert!(j.vmid == vmid && !j.needs_flush, "same tables: VMID {} flush {}", j.vmid, j.needs_flush);
    // Two jobs of a VM at once share the slot.
    let j2 = a.begin_job().map_err(e)?.ok_or("no VMID")?;
    kassert!(j2.vmid == vmid && !j2.needs_flush, "second job: VMID {} flush {}", j2.vmid, j2.needs_flush);
    drop((j, j2));
    let op = PtOp::Map { va: V, pages: 4, mem: mem.clone(), first: 0, place: Some(BoPlace::Gtt), uswc: false, flags: RW };
    kassert!(a.update(core::slice::from_ref(&op)).error.is_none(), "map");
    let j = a.begin_job().map_err(e)?.ok_or("no VMID")?;
    kassert!(j.vmid == vmid && j.needs_flush, "changed tables: VMID {} flush {}", j.vmid, j.needs_flush);
    drop(j);
    // Another VM takes another slot; the least recently used idle one, with a flush.
    let j = b.begin_job().map_err(e)?.ok_or("no VMID")?;
    kassert!(j.vmid != vmid && j.needs_flush, "second VM: VMID {} flush {}", j.vmid, j.needs_flush);
    drop(j);
    // A job that never reached a ring: its flush did not happen, so the next one flushes. One without a flush loses
    // nothing.
    let j = a.begin_job().map_err(e)?.ok_or("no VMID")?;
    kassert!(!j.needs_flush, "the tables did not change");
    j.cancel();
    let j = a.begin_job().map_err(e)?.ok_or("no VMID")?;
    kassert!(!j.needs_flush, "a cancelled job without a flush cost a flush");
    drop(j);
    kassert!(a.update(&[PtOp::Clear { va: V, pages: 4 }]).error.is_none(), "clear");
    let j = a.begin_job().map_err(e)?.ok_or("no VMID")?;
    kassert!(j.needs_flush, "changed tables");
    let cancelled = j.vmid;
    j.cancel();
    let j = a.begin_job().map_err(e)?.ok_or("no VMID after a cancel")?;
    kassert!(j.needs_flush, "the flush of a cancelled job counted as done (VMID {cancelled}, then {})", j.vmid);
    drop(j);
    kassert!(a.update(core::slice::from_ref(&op)).error.is_none(), "map again");
    let j = a.begin_job().map_err(e)?.ok_or("no VMID")?;
    drop(j);
    // Every slot busy with a VM of its own: the eighth waits, and gets one when a job ended.
    let n = (KERNEL_VMIDS.end - KERNEL_VMIDS.start) as u64;
    let mut vms: Vec<Arc<HwVm>> = Vec::new();
    let mut jobs: Vec<VmJob> = Vec::new();
    for k in 0..n {
        let v = s.gpu.new_vm(10 + k).map_err(e)?;
        jobs.push(v.begin_job().map_err(e)?.ok_or("no VMID for a free slot")?);
        vms.push(v);
    }
    let mut seen: Vec<u32> = jobs.iter().map(|j| j.vmid).collect();
    seen.sort_unstable();
    kassert!(seen == KERNEL_VMIDS.collect::<Vec<_>>(), "VMIDs {seen:?}");
    let last = s.gpu.new_vm(99).map_err(e)?;
    kassert!(last.begin_job().map_err(e)?.is_none(), "an eighth VM got a VMID with all seven busy");
    let freed = jobs.remove(0);
    let v0 = freed.vmid;
    drop(freed);
    let j = last.begin_job().map_err(e)?.ok_or("no VMID after a job ended")?;
    kassert!(j.vmid == v0 && j.needs_flush, "VMID {} flush {}", j.vmid, j.needs_flush);
    drop(j);
    drop(jobs);
    drop(vms);
    // A VM that went leaves no owner behind: its slot flushes for the next.
    drop(last);
    drop((a, b));
    kassert!(s.gpu.vm_ids().pasids() == 0, "PASIDs left: {}", s.gpu.vm_ids().pasids());
    drop(mem);
    // The reserved VMID through the uAPI: one holder, a slot only its jobs get.
    let dev = register(&s.gpu)?;
    let r = reserved_on(&s, &dev);
    device::unregister(&dev);
    r
}

fn reserved_on(s: &Sparse, dev: &Arc<device::Device>) -> ktest::TestResult {
    let e = |x: i64| format!("errno {x}");
    let f0 = open_device(dev.clone(), MinorKind::Render, false).map_err(|x| format!("open: errno {x}"))?;
    let f1 = open_device(dev.clone(), MinorKind::Render, false).map_err(|x| format!("open: errno {x}"))?;
    let m = KMem::new();
    kassert!(vm_op(&f0, &m, AMDGPU_VM_OP_RESERVE_VMID).is_ok(), "RESERVE_VMID");
    kassert!(vm_op(&f1, &m, AMDGPU_VM_OP_RESERVE_VMID) == Err(linux::errno::ENOENT), "a second holder");
    let holder = s.gpu.new_vm(f0.drm_file().client_id()).map_err(e)?;
    let other = s.gpu.new_vm(f1.drm_file().client_id()).map_err(e)?;
    let j = holder.begin_job().map_err(e)?.ok_or("no reserved VMID")?;
    let reserved = j.vmid;
    kassert!(j.needs_flush, "first use of the reserved VMID without a flush");
    drop(j);
    // The others never get the reserved slot, however many take one.
    let mut vms = Vec::new();
    let mut jobs = Vec::new();
    for k in 0..(KERNEL_VMIDS.end - KERNEL_VMIDS.start - 1) as u64 {
        let v = s.gpu.new_vm(100 + k).map_err(e)?;
        let j = v.begin_job().map_err(e)?.ok_or("no VMID")?;
        kassert!(j.vmid != reserved, "VM {k} got the reserved VMID {reserved}");
        jobs.push(j);
        vms.push(v);
    }
    kassert!(other.begin_job().map_err(e)?.is_none(), "the rest of the slots are busy");
    let j = holder.begin_job().map_err(e)?.ok_or("the holder waits for its own VMID")?;
    kassert!(j.vmid == reserved && !j.needs_flush, "VMID {} flush {}", j.vmid, j.needs_flush);
    drop(j);
    drop(jobs);
    drop(vms);
    // UNRESERVE: the slot comes back at the next grab.
    kassert!(vm_op(&f0, &m, AMDGPU_VM_OP_UNRESERVE_VMID).is_ok(), "UNRESERVE_VMID");
    let j = other.begin_job().map_err(e)?.ok_or("no VMID")?;
    drop(j);
    drop((holder, other, f0, f1));
    Ok(())
}

/// What a Clear cut stays until the last job that began before it ended; a job after it holds nothing up.
pub(crate) fn check_pt_memory() -> ktest::TestResult {
    let s = sparse(512 * M)?;
    let owner = PinOwner::new(50);
    let e = |x: i64| format!("errno {x}");
    let mem = s.gpu.bo_memory(BoPlace::Gtt, 8, 1, false, &owner).map_err(e)?;
    let weak = Arc::downgrade(&mem);
    let vm = s.gpu.new_vm(1).map_err(e)?;
    let at = 1u64 << 40;
    let op = PtOp::Map { va: at, pages: 8, mem: mem.clone(), first: 0, place: Some(BoPlace::Gtt), uswc: false, flags: RW };
    kassert!(vm.update(core::slice::from_ref(&op)).error.is_none(), "map");
    drop(op);
    drop(mem);
    kassert!(weak.upgrade().is_some(), "the VM keeps a mapped object's memory");
    let before = vm.counts();
    let old = vm.begin_job().map_err(e)?.ok_or("no VMID")?;
    let older = vm.begin_job().map_err(e)?.ok_or("no VMID")?;
    kassert!(vm.update(&[PtOp::Clear { va: at, pages: 8 }]).error.is_none(), "clear");
    let c = vm.counts();
    kassert!(matches!(vm.walk(at), Ok(Walk::Hole { .. })), "the tables changed at once");
    kassert!(weak.upgrade().is_some() && c.garbage == 1, "the memory went under a running job: {c:?}");
    kassert!(c.pending > 0 && c.pages == before.pages, "the tables went under a running job: {c:?}, before {before:?}");
    // A job after the change flushes and does not keep anything.
    let new = vm.begin_job().map_err(e)?.ok_or("no VMID")?;
    kassert!(new.needs_flush, "a job after the change without a flush");
    drop(new);
    kassert!(weak.upgrade().is_some(), "the old jobs still run");
    drop(old);
    kassert!(weak.upgrade().is_some(), "one old job still runs");
    drop(older);
    let c = vm.counts();
    kassert!(weak.upgrade().is_none() && c.garbage == 0 && c.pending == 0 && c.pages < before.pages, "not freed after the last job: {c:?}");
    // Nothing running: at once.
    let mem = s.gpu.bo_memory(BoPlace::Gtt, 1, 1, false, &owner).map_err(e)?;
    let weak = Arc::downgrade(&mem);
    let op = PtOp::Map { va: at, pages: 1, mem: mem.clone(), first: 0, place: Some(BoPlace::Gtt), uswc: false, flags: RW };
    kassert!(vm.update(core::slice::from_ref(&op)).error.is_none(), "map");
    drop((op, mem));
    kassert!(vm.update(&[PtOp::Clear { va: at, pages: 1 }]).error.is_none(), "clear");
    kassert!(weak.upgrade().is_none(), "held with no job running");
    // The last Arc: tables and PASID go, a job holds the VM until then.
    let job = vm.begin_job().map_err(e)?.ok_or("no VMID")?;
    let w = Arc::downgrade(&vm);
    drop(vm);
    kassert!(w.upgrade().is_some(), "a running job's VM went");
    drop(job);
    kassert!(w.upgrade().is_none(), "the VM stayed after its last job");
    let h = s.gpu.heaps().bytes();
    kassert!(h.table_used == 0 && s.gpu.vm_ids().pasids() == 0, "left: {h:?}, {} PASIDs", s.gpu.vm_ids().pasids());
    Ok(())
}

/// DR-9: an update behind a dependency waits, every later update behind it; the order holds for the whole VM.
pub(crate) fn check_order() -> ktest::TestResult {
    let s = sparse(512 * M)?;
    let owner = PinOwner::new(50);
    let e = |x: i64| format!("errno {x}");
    let mem = s.gpu.bo_memory(BoPlace::Gtt, 8, 1, false, &owner).map_err(e)?;
    let vm = s.gpu.new_vm(1).map_err(e)?;
    let m = |va| PtOp::Map { va, pages: 1, mem: mem.clone(), first: 0, place: Some(BoPlace::Gtt), uswc: false, flags: RW };
    kassert!(vm.update(&[m(V)]).error.is_none(), "map");
    let gate = Fence::new(fence::context_alloc(1), 1, "gx", "ktest");
    let c = vm.update_after(&[PtOp::Clear { va: V, pages: 1 }], core::slice::from_ref(&gate));
    let cf = c.fence.ok_or("no fence for a deferred update")?;
    kassert!(c.error.is_none() && c.done == 1, "deferred");
    kassert!(matches!(vm.walk(V), Ok(Walk::Page(_))), "the update ran before its dependency");
    // A MAP of the same range behind it must not run first (it would overlap).
    let r = vm.update(&[m(V)]);
    let rf = r.fence.ok_or("no fence behind a deferred update")?;
    let r2 = vm.update_after(&[m(V + P)], &[]);
    let rf2 = r2.fence.ok_or("no fence behind a deferred update")?;
    kassert!(vm.counts().deferred == 3 && !fence::is_signaled(&cf) && !fence::is_signaled(&rf), "queue {:?}", vm.counts());
    kassert!(matches!(vm.walk(V + P), Ok(Walk::Hole { .. })), "a later update overtook");
    gate.signal();
    let t0 = time::now_ns();
    while !fence::is_signaled(&rf2) {
        kassert!(time::now_ns().saturating_sub(t0) < 5_000_000_000, "the queue did not run: {:?}", vm.counts());
        ksync::yield_now();
    }
    kassert!(fence::is_signaled(&cf) && fence::is_signaled(&rf) && fence::status(&cf) == 1 && fence::status(&rf) == 1, "fences");
    kassert!(matches!(vm.walk(V), Ok(Walk::Page(_))) && matches!(vm.walk(V + P), Ok(Walk::Page(_))), "the mappings after the queue");
    // A VM that goes ends what still waits, and says it did not run.
    let gate2 = Fence::new(fence::context_alloc(1), 1, "gx", "ktest");
    let d = vm.update_after(&[PtOp::Clear { va: V, pages: 1 }], core::slice::from_ref(&gate2));
    let df = d.fence.ok_or("no fence")?;
    drop(vm);
    kassert!(fence::is_signaled(&df), "a waiter hangs on a gone VM");
    kassert!(fence::status(&df) == -(linux::errno::ESRCH as i32), "an update that never ran reports {}", fence::status(&df));
    gate2.signal();
    Ok(())
}

/// A store that does not reach the carve-out breaks the VM (EIO), never silently; other VMs go on when memory does.
pub(crate) fn check_store_failure() -> ktest::TestResult {
    let s = sparse(512 * M)?;
    let owner = PinOwner::new(50);
    let e = |x: i64| format!("errno {x}");
    let mem = s.gpu.bo_memory(BoPlace::Gtt, 1, 1, false, &owner).map_err(e)?;
    let vm = s.gpu.new_vm(1).map_err(e)?;
    let op = PtOp::Map { va: V, pages: 1, mem: mem.clone(), first: 0, place: Some(BoPlace::Gtt), uswc: false, flags: RW };
    s.watch.fail.store(true, Ordering::Release);
    let r = vm.update(core::slice::from_ref(&op));
    s.watch.fail.store(false, Ordering::Release);
    kassert!(r.error == Some(linux::errno::EIO) && r.done == 0, "error {:?} done {}", r.error, r.done);
    let r = vm.update(&[PtOp::Clear { va: V, pages: 1 }]);
    kassert!(r.error == Some(linux::errno::EIO), "a broken VM went on: {:?}", r.error);
    kassert!(vm.begin_job().is_err(), "a job on a broken VM");
    let vm2 = s.gpu.new_vm(2).map_err(e)?;
    kassert!(vm2.update(core::slice::from_ref(&op)).error.is_none(), "another VM");
    // Memory without frames, and VRAM that is not contiguous: refused, no local PTE over foreign memory.
    let foreign = Arc::new(Scattered);
    let op = PtOp::Map { va: V + P, pages: 2, mem: foreign, first: 0, place: Some(BoPlace::Vram), uswc: false, flags: RW };
    kassert!(vm2.update(core::slice::from_ref(&op)).error == Some(linux::errno::EINVAL), "a scattered range in the carve-out");
    let none = Arc::new(NoFrames);
    let op = PtOp::Map { va: V + P, pages: 1, mem: none, first: 0, place: None, uswc: false, flags: RW };
    kassert!(vm2.update(core::slice::from_ref(&op)).error == Some(linux::errno::EFAULT), "memory without frames");
    Ok(())
}

/// Bad GEM_VA requests through the real ioctl, each with Linux's errno (amdgpu_gem.c:664 `amdgpu_gem_va_ioctl`,
/// amdgpu_vm.c:1519 `amdgpu_vm_bo_map`, :1652 `amdgpu_vm_bo_unmap`); the page tables do not move for any of them.
pub(crate) fn check_va_hostile() -> ktest::TestResult {
    let s = sparse(512 * M)?;
    let dev = register(&s.gpu)?;
    let r = va_hostile_on(&dev);
    device::unregister(&dev);
    r?;
    let h = s.gpu.heaps().bytes();
    kassert!(h.user_used == 0 && h.table_used == 0 && s.gpu.vms_alive() == 0, "left: {h:?}");
    Ok(())
}

fn va_hostile_on(dev: &Arc<device::Device>) -> ktest::TestResult {
    use linux::errno::{EFAULT, EINVAL, ENOENT};
    let f = open_device(dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
    let m = KMem::new();
    let g = gem(&f, &m, 32 * P, AMDGPU_GEM_DOMAIN_GTT, 0)?;
    let v = gem(&f, &m, 2 * M + 16 * P, AMDGPU_GEM_DOMAIN_VRAM, 0)?;
    let closed = gem(&f, &m, P, AMDGPU_GEM_DOMAIN_GTT, 0)?;
    m.put(&drm_gem_close { handle: closed, pad: 0 });
    f.ioctl_as(DRM_IOCTL_GEM_CLOSE, BASE, &m, KERNEL).map_err(|e| format!("GEM_CLOSE: errno {e}"))?;
    let mut model = Model::default();
    va(&f, &m, g, AMDGPU_VA_OP_MAP, RW, V, 0, 16 * P)?;
    model.map(&f, g, V, 0, 16 * P, RW, Target::System { snooped: true }, Mtype::Nc)?;
    va(&f, &m, v, AMDGPU_VA_OP_MAP, RW, V + 4 * M, 0, 2 * M + 16 * P)?;
    model.map(&f, v, V + 4 * M, 0, 2 * M + 16 * P, RW, Target::Local, Mtype::Nc)?;
    let free = V + 16 * M;
    const HOLE: u64 = 0x0000_8000_0000_0000;
    let (map, unmap, clear, replace) = (AMDGPU_VA_OP_MAP, AMDGPU_VA_OP_UNMAP, AMDGPU_VA_OP_CLEAR, AMDGPU_VA_OP_REPLACE);
    // (what, handle, operation, flags, address, offset, size, errno)
    let cases: [(&str, u32, u32, u32, u64, u64, u64, i64); 24] = [
        ("an address inside a page", g, map, RW, free + 1, 0, P, EINVAL),
        ("an offset inside a page", g, map, RW, free, 1, P, EINVAL),
        ("a size inside a page", g, map, RW, free, 0, P + 1, EINVAL),
        ("size 0", g, map, RW, free, 0, 0, EINVAL),
        ("the same range again", g, map, RW, V, 0, 16 * P, EINVAL),
        ("over the end of a mapping", g, map, RW, V + 15 * P, 0, 4 * P, EINVAL),
        ("inside a mapping", g, map, RW, V + 4 * P, 0, P, EINVAL),
        ("over a whole mapping from below", g, map, RW, V - P, 0, 18 * P, EINVAL),
        ("below the reserved bottom", g, map, RW, 0, 0, P, EINVAL),
        ("in the hole", g, map, RW, HOLE, 0, P, EINVAL),
        ("past the top of the space", g, map, RW, 0xffff_ffff_ffff_f000, 0, 2 * P, EINVAL),
        ("an end that wraps", g, map, RW, free, 0, u64::MAX - P + 1, EINVAL),
        ("past the end of the object", g, map, RW, free, 0, 33 * P, EINVAL),
        ("an offset past the end of the object", g, map, RW, free, 32 * P, P, EINVAL),
        ("an offset that wraps", g, map, RW, free, u64::MAX - P + 1, 2 * P, EINVAL),
        ("unknown flags", g, map, RW | 1 << 20, free, 0, P, EINVAL),
        ("PRT with page flags", 0, map, RW | AMDGPU_VM_PAGE_PRT, free, 0, P, EINVAL),
        ("an unknown operation", g, 99, RW, free, 0, P, EINVAL),
        ("a handle never created", 0x7777, map, RW, free, 0, P, ENOENT),
        ("a closed handle", closed, map, RW, free, 0, P, ENOENT),
        ("UNMAP of what is not mapped", g, unmap, 0, free, 0, P, ENOENT),
        ("UNMAP of another object's mapping", g, unmap, 0, V + 4 * M, 0, P, ENOENT),
        ("REPLACE past the end of the object", g, replace, RW, V, 0, 33 * P, EINVAL),
        ("CLEAR inside a page", 0, clear, 0, V + 1, 0, P, EINVAL),
    ];
    let vm = hw_vm(&f)?;
    let hw = as_hw(&vm)?;
    let (seq, counts) = (hw.tlb_seq(), hw.counts());
    model.check(hw, "before the bad requests")?;
    for (what, handle, operation, flags, addr, off, size, want) in cases {
        m.put(&drm_amdgpu_gem_va { handle, operation, flags, va_address: addr, offset_in_bo: off, map_size: size, ..drm_amdgpu_gem_va::ZERO });
        let got = f.ioctl_as(DRM_IOCTL_AMDGPU_GEM_VA, BASE, &m, KERNEL);
        kassert!(got == Err(want), "{what}: {got:?}, Linux answers -{want}");
        kassert!(hw.tlb_seq() == seq && hw.counts() == counts, "{what}: the tables moved: {:?}, before {counts:?}", hw.counts());
        model.check(hw, what)?;
    }
    // A pointer that is no program's memory.
    let got = f.ioctl_as(DRM_IOCTL_AMDGPU_GEM_VA, 0xdead_0000, &m, KERNEL);
    kassert!(got == Err(EFAULT), "a bad pointer: {got:?}");
    kassert!(hw.tlb_seq() == seq, "a bad pointer moved the tables");
    drop(vm);
    Ok(())
}

/// A carve-out full of objects: the reserve's tables run out, the next map answers ENOMEM, what is mapped stays, and a
/// table that came back serves again.
pub(crate) fn check_table_reserve() -> ktest::TestResult {
    let s = sparse(256 * M)?;
    let owner = PinOwner::new(50);
    let e = |x: i64| format!("errno {x}");
    let mut bos: Vec<Arc<dyn Exporter>> = Vec::new();
    while let Ok(b) = s.gpu.bo_memory(BoPlace::Vram, 8 * M / P, 1, false, &owner) {
        bos.push(b);
        kassert!(bos.len() < 64, "the carve-out does not fill");
    }
    let one = s.gpu.bo_memory(BoPlace::Gtt, 1, 1, false, &owner).map_err(e)?;
    let vm = s.gpu.new_vm(1).map_err(e)?;
    let map = |n: u64| PtOp::Map { va: V + n * (2 * M), pages: 1, mem: one.clone(), first: 0, place: Some(BoPlace::Gtt), uswc: false, flags: RW };
    // One table per mapping: each lies in a 2 MiB block of its own.
    let mut n = 0u64;
    let refused = loop {
        let r = vm.update(core::slice::from_ref(&map(n)));
        if let Some(err) = r.error {
            break err;
        }
        n += 1;
        kassert!(n < 1 << 16, "the table heap never ran out");
    };
    kassert!(refused == linux::errno::ENOMEM, "errno {refused}, not ENOMEM, after {n} mappings");
    kassert!(n * P >= 8 * M, "only {n} tables from a reserve of 16 MiB");
    kassert!(matches!(vm.walk(V + n * (2 * M)), Ok(Walk::Hole { .. })), "the refused map left an entry: {:?}", vm.walk(V + n * (2 * M)));
    for k in 0..n {
        let want = one.page_phys(0).ok_or("no frame")?;
        match vm.walk(V + k * (2 * M) + 0x18) {
            Ok(Walk::Page(l)) if l.phys == want + 0x18 => {}
            w => return Err(format!("mapping {k} of {n} after the refusal: {w:?}")),
        }
    }
    // Not stuck: a map that needs no new table goes, and one whose table came back too.
    let near = PtOp::Map { va: V + P, pages: 1, mem: one.clone(), first: 0, place: Some(BoPlace::Gtt), uswc: false, flags: RW };
    kassert!(vm.update(core::slice::from_ref(&near)).error.is_none(), "a map into an existing table");
    kassert!(vm.update(&[PtOp::Clear { va: V + (n - 1) * (2 * M), pages: 1 }]).error.is_none(), "clear");
    kassert!(vm.update(core::slice::from_ref(&map(n))).error.is_none(), "a table that came back");
    drop(vm);
    drop(bos);
    drop(one);
    let h = s.gpu.heaps().bytes();
    kassert!(h.user_used == 0 && h.table_used == 0, "left: {h:?}");
    kassert!(!s.watch.alias.load(Ordering::Acquire), "a range with a window went back, or got a second one");
    Ok(())
}

/// A slab whose pages are all free goes back, window first; a create/free loop stays inside the cushion and maps
/// nothing new; table slabs above the reserve go too, and their range can come back with the other memory type.
pub(crate) fn check_slabs_back() -> ktest::TestResult {
    let s = sparse(512 * M)?;
    let owner = PinOwner::new(50);
    let e = |x: i64| format!("errno {x}");
    let bo = s.gpu.bo_memory(BoPlace::Vram, 16, 1, false, &owner).map_err(e)?;
    let mapped = s.watch.kinds.lock().len();
    drop(bo);
    for _ in 0..16 {
        drop(s.gpu.bo_memory(BoPlace::Vram, 16, 1, false, &owner).map_err(e)?);
    }
    kassert!(s.watch.kinds.lock().len() == mapped, "a create/free loop maps: {} windows, was {mapped}", s.watch.kinds.lock().len());
    let free = s.bos.lock().free_bytes();
    let big = s.gpu.bo_memory(BoPlace::Vram, 64 * M / P, 1, false, &owner).map_err(e)?;
    kassert!(s.bos.lock().free_bytes() == free - 64 * M, "the object's slab");
    let weak = Arc::downgrade(&big);
    drop(big);
    kassert!(weak.upgrade().is_none() && s.bos.lock().free_bytes() == free, "a free slab stayed: {:#x}, was {free:#x}", s.bos.lock().free_bytes());
    // Tables above the reserve: one table per mapping, for more than 16 MiB of them.
    let one = s.gpu.bo_memory(BoPlace::Gtt, 1, 1, false, &owner).map_err(e)?;
    let vm = s.gpu.new_vm(1).map_err(e)?;
    let n = 5000u64;
    for k in 0..n {
        let op = PtOp::Map { va: V + k * (2 * M), pages: 1, mem: one.clone(), first: 0, place: Some(BoPlace::Gtt), uswc: false, flags: RW };
        kassert!(vm.update(core::slice::from_ref(&op)).error.is_none(), "map {k}");
    }
    let grown = s.gpu.heaps().bytes().table_slabs;
    kassert!(grown > 16 * M, "the tables stayed in the reserve: {grown:#x}");
    kassert!(vm.update(&[PtOp::Clear { va: V, pages: n * (2 * M) / P }]).error.is_none(), "clear");
    drop(vm);
    let h = s.gpu.heaps().bytes();
    kassert!(h.table_used == 0 && h.table_slabs <= 16 * M, "table slabs stayed above the reserve: {h:?}");
    // What the tables gave back serves objects: the whole manager, with the cushion on top.
    let room = s.bos.lock().free_bytes().saturating_sub(16 * M);
    let big = s.gpu.bo_memory(BoPlace::Vram, room / P, 1, false, &owner).map_err(e)?;
    drop(big);
    kassert!(!s.watch.alias.load(Ordering::Acquire), "a range with a window went back, or got a second one");
    Ok(())
}

/// The wipe of a large object gives the CPU up between chunks and has left the write-combining buffers before the
/// object can be seen; so has a kernel write.
pub(crate) fn check_wipe() -> ktest::TestResult {
    let s = sparse(512 * M)?;
    let owner = PinOwner::new(50);
    let bo = s.gpu.bo_memory(BoPlace::Vram, 200 * M / P, 1, false, &owner).map_err(|e| format!("errno {e}"))?;
    let most = s.watch.max_fill.load(Ordering::Acquire);
    kassert!(most > 0 && most <= WIPE_CHUNK, "a fill of {most:#x} bytes");
    kassert!(!s.watch.dirty.load(Ordering::Acquire), "a new object's wipe was not drained");
    kassert!(bo.write(100 * M, &[0x5a; 64]), "write");
    kassert!(!s.watch.dirty.load(Ordering::Acquire), "a write was not drained");
    let mut b = [0u8; 64];
    kassert!(bo.read(100 * M, &mut b) && b == [0x5a; 64], "read back {b:x?}");
    Ok(())
}

/// Memory whose frames change under it, as a userptr range's do at a revalidation.
struct Revalidated(SpinLock<Arc<dyn Exporter>>);

impl Exporter for Revalidated {
    fn name(&self) -> &'static str {
        "test"
    }
    fn size(&self) -> u64 {
        self.0.lock().size()
    }
    fn aperture(&self) -> Option<mm::device::Aperture> {
        None
    }
    fn page_phys(&self, page: u64) -> Option<u64> {
        self.0.lock().page_phys(page)
    }
    fn frames(&self) -> Option<Arc<dyn Exporter>> {
        Some(self.0.lock().clone())
    }
    fn read(&self, _off: u64, _out: &mut [u8]) -> bool {
        false
    }
    fn write(&self, _off: u64, _data: &[u8]) -> bool {
        false
    }
}

/// The frames in the entries are the frames held: a revalidation that replaces a userptr object's pages does not free
/// what the GPU can still reach, until the Clear that takes the entries away (GX-8b/DR-9b queue it).
pub(crate) fn check_revalidate() -> ktest::TestResult {
    let s = sparse(512 * M)?;
    let owner = PinOwner::new(50);
    let e = |x: i64| format!("errno {x}");
    let old = s.gpu.bo_memory(BoPlace::Gtt, 4, 1, false, &owner).map_err(e)?;
    let new = s.gpu.bo_memory(BoPlace::Gtt, 4, 1, false, &owner).map_err(e)?;
    let frames: Vec<Option<u64>> = (0..4).map(|k| old.page_phys(k)).collect();
    let weak = Arc::downgrade(&old);
    let mem = Arc::new(Revalidated(SpinLock::named("gx.ktest.revalidated", old.clone())));
    drop(old);
    let vm = s.gpu.new_vm(1).map_err(e)?;
    let map = PtOp::Map { va: V, pages: 4, mem: mem.clone(), first: 0, place: None, uswc: false, flags: RW };
    kassert!(vm.update(core::slice::from_ref(&map)).error.is_none(), "map");
    let replaced = core::mem::replace(&mut *mem.0.lock(), new.clone());
    drop(replaced);
    kassert!(weak.upgrade().is_some(), "the frames in the entries went at the revalidation");
    for k in 0..4u64 {
        let want = frames[k as usize].ok_or("no frame")?;
        match vm.walk(V + k * P) {
            Ok(Walk::Page(l)) if l.phys == want => {}
            w => return Err(format!("page {k}: {w:?}, want the first frames at {want:#x}")),
        }
    }
    let held = vm.held_at(V).and_then(|h| h.0).ok_or("nothing held")?;
    kassert!(held.page_phys(0) == frames[0], "held {:x?}, entries {:x?}", held.page_phys(0), frames[0]);
    drop(held);
    // The revalidation's Clear and Map: the old frames go, the new ones are in the entries and held.
    kassert!(vm.update(&[PtOp::Clear { va: V, pages: 4 }]).error.is_none(), "clear");
    kassert!(weak.upgrade().is_none(), "the old frames stayed after the Clear");
    kassert!(vm.update(core::slice::from_ref(&map)).error.is_none(), "map again");
    let want = new.page_phys(1).ok_or("no frame")?;
    kassert!(matches!(vm.walk(V + P), Ok(Walk::Page(l)) if l.phys == want), "the new frames: {:?}", vm.walk(V + P));
    Ok(())
}

struct Scattered;

impl Exporter for Scattered {
    fn name(&self) -> &'static str {
        "test"
    }
    fn size(&self) -> u64 {
        2 * P
    }
    fn aperture(&self) -> Option<mm::device::Aperture> {
        None
    }
    fn page_phys(&self, page: u64) -> Option<u64> {
        Some(PHYS_BASE + 32 * M + page * 8 * P)
    }
    fn read(&self, _off: u64, _out: &mut [u8]) -> bool {
        false
    }
    fn write(&self, _off: u64, _data: &[u8]) -> bool {
        false
    }
}

struct NoFrames;

impl Exporter for NoFrames {
    fn name(&self) -> &'static str {
        "test"
    }
    fn size(&self) -> u64 {
        P
    }
    fn aperture(&self) -> Option<mm::device::Aperture> {
        None
    }
    fn page_phys(&self, _page: u64) -> Option<u64> {
        None
    }
    fn read(&self, _off: u64, _out: &mut [u8]) -> bool {
        false
    }
    fn write(&self, _off: u64, _data: &[u8]) -> bool {
        false
    }
}

/// The proof the live bring-up runs, on the fake chain, for every carve-out size.
pub(crate) fn check_proof(d: &Discovery) -> ktest::TestResult {
    for size in [512 * M, 8 * G, 16 * G] {
        let rig = Rig::chain(d, size)?;
        let lines = crate::hw::check::run(&rig.gpu)?;
        kassert!(lines.len() >= 5, "{lines:?}");
        kassert!(rig.gpu.vms_alive() == 0 && rig.gpu.vm_ids().pasids() == 0, "a VM left");
        // And again on the same heaps, as after a stop and a new bring-up.
        crate::hw::check::run(&rig.gpu)?;
        rig.finish()?;
    }
    Ok(())
}

/// `until=stop`'s gate: refused with the count while files are open; then the node goes and new opens end.
fn stop_gate() -> ktest::TestResult {
    let s = sparse(512 * M)?;
    let slot = crate::hw::live::NodeSlot::new();
    kassert!(slot.stop_gate() == Ok(None) && slot.minor().is_none(), "an empty slot");
    let m = slot.register(s.gpu.clone())?;
    kassert!(slot.minor() == Some(m), "minor");
    // A second backend must not lose the first: nothing could count its files or remove it any more.
    let count = drm::device::devices().len();
    kassert!(slot.register(s.gpu.clone()).is_err() && drm::device::devices().len() == count && slot.minor() == Some(m), "a second node");
    let gate = crate::hw::live::ToolGate::new();
    let run = gate.enter().ok_or("an idle gate")?;
    kassert!(gate.enter().is_none(), "two runs at once");
    kassert!(gate.enter().is_none(), "a refused call opened the gate");
    drop(run);
    kassert!(gate.enter().is_some(), "the gate stayed shut");
    let dev = drm::device::devices().into_iter().find(|d| d.render_minor() == m).ok_or("not registered")?;
    let f = open_device(dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
    let f2 = open_device(dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
    let why = slot.stop_gate().err().ok_or("stop with open files")?;
    kassert!(why.contains("2 open files"), "{why}");
    kassert!(slot.minor() == Some(m), "the node went on a refusal");
    drop(f);
    kassert!(slot.stop_gate().err().is_some_and(|w| w.contains("1 open files")), "one file open");
    drop(f2);
    kassert!(slot.stop_gate().is_ok_and(|n| n.is_some()), "no file open");
    kassert!(slot.minor().is_none() && !drm::device::devices().iter().any(|d| Arc::ptr_eq(d, &dev)), "the node stayed");
    kassert!(open_device(dev, MinorKind::Render, false).is_err(), "an open after the stop");
    Ok(())
}
ktest::ktest!("gx.hw.stop_gate", Safe, stop_gate);

fn read_text(path: &str) -> Result<String, String> {
    let f = vfs::root_ns().open_flags(path.as_bytes(), 0, 0).map_err(|e| format!("open {path}: errno {e}"))?;
    let mut buf = alloc::vec![0u8; 512];
    let n = f.file.read_at(0, &mut buf).map_err(|e| format!("read {path}: errno {e}"))?;
    buf.truncate(n);
    String::from_utf8(buf).map_err(|_| format!("{path}: not text"))
}

/// A real function's directory (Linux's place for it, nested under its bridges): libdrm finds the PCI files through
/// the render node, and the node's entries go with the device.
fn sysfs() -> ktest::TestResult {
    let nodes = pci::tree();
    let i = nodes.len().checked_sub(1).ok_or("no PCI function")?;
    let n = &nodes[i];
    let s = sparse(512 * M)?;
    let id = PciIdentity { domain: n.loc.segment, bus: n.loc.bus, slot: n.loc.dev, func: n.loc.func, vendor: n.vendor, device: n.device, subsystem_vendor: n.subsystem.0, subsystem_device: n.subsystem.1, revision: n.revision, class: n.class, driver: "amdgpu", place: Place::Pci(pci::sysfs::dir(&nodes, i)) };
    let gpu = s.gpu.with_pci(id)?;
    let dev = register(&gpu)?;
    let m = dev.render_minor();
    let r = (|| -> ktest::TestResult {
        let vendor = read_text(&format!("/sys/class/drm/renderD{m}/device/vendor"))?;
        kassert!(vendor == format!("0x{:04x}\n", n.vendor), "vendor {vendor:?}");
        let ev = read_text(&format!("/sys/dev/char/226:{m}/device/uevent"))?;
        kassert!(ev.contains(&format!("PCI_SLOT_NAME={}", pci::sysfs::name(n.loc))), "uevent {ev:?}");
        kassert!(read_text(&format!("/sys/class/drm/renderD{m}/dev"))? == format!("226:{m}\n"), "dev");
        Ok(())
    })();
    device::unregister(&dev);
    r?;
    kassert!(vfs::root_ns().lookup(&format!("/sys/class/drm/renderD{m}")).is_err(), "the entry stayed");
    Ok(())
}
ktest::ktest!("gx.hw.sysfs", Safe, sysfs);

fn info() -> ktest::TestResult {
    check_info(&fakechip::discovery_780m())
}
ktest::ktest!("gx.hw.info", Safe, info);

fn harvest() -> ktest::TestResult {
    check_harvest(&fakechip::discovery_780m())
}
ktest::ktest!("gx.hw.harvest", Safe, harvest);

ktest::ktest!("gx.hw.firmware", Safe, check_firmware);
ktest::ktest!("gx.hw.vram", Safe, check_vram);
ktest::ktest!("gx.hw.two_slabs", Safe, check_two_slabs);
ktest::ktest!("gx.hw.gtt", Safe, check_gtt);

fn tables() -> ktest::TestResult {
    check_tables(&fakechip::discovery_780m())
}
ktest::ktest!("gx.hw.tables", Safe, tables);

ktest::ktest!("gx.hw.vmid", Safe, check_vmid);
ktest::ktest!("gx.hw.pt_memory", Safe, check_pt_memory);
ktest::ktest!("gx.hw.order", Safe, check_order);
ktest::ktest!("gx.hw.store_failure", Safe, check_store_failure);

fn proof() -> ktest::TestResult {
    check_proof(&fakechip::discovery_780m())
}
ktest::ktest!("gx.hw.proof", Safe, proof);
ktest::ktest!("gx.hw.va_hostile", Safe, check_va_hostile);
ktest::ktest!("gx.hw.table_reserve", Safe, check_table_reserve);
ktest::ktest!("gx.hw.slabs_back", Safe, check_slabs_back);
ktest::ktest!("gx.hw.wipe", Safe, check_wipe);
ktest::ktest!("gx.hw.revalidate", Safe, check_revalidate);

// ---- what needs threads or the real CPU's windows: kernel only

/// One thread's rounds on a shared file: objects (a large one now and then, so slabs come and go), GEM_VA on its own
/// range, a job over the unmap, a VM of its own so the seven VMIDs run short.
#[cfg(not(test))]
fn worker(f: &OpenDrm, gpu: &Arc<HardwareGpu>, t: u64) -> ktest::TestResult {
    const ROUNDS: u64 = 12;
    let m = KMem::new();
    let shared = hw_vm(f)?;
    let hw = as_hw(&shared)?.arc().ok_or("the VM is gone")?;
    let own = gpu.new_vm(1000 + t).map_err(|e| format!("VM: errno {e}"))?;
    let base = V + (t + 1) * (1 << 36);
    let next_job = |vm: &Arc<HwVm>| -> Result<VmJob, String> {
        let t0 = time::now_ns();
        loop {
            if let Some(j) = vm.begin_job().map_err(|e| format!("begin_job: errno {e}"))? {
                return Ok(j);
            }
            kassert!(time::now_ns().saturating_sub(t0) < 20_000_000_000, "no VMID for 20 s");
            ksync::yield_now();
        }
    };
    for r in 0..ROUNDS {
        let vsize = if r % 4 == 0 { 8 * M } else { 16 * P };
        let at = base + r * (32 * M);
        let v = gem(f, &m, vsize, AMDGPU_GEM_DOMAIN_VRAM, 0)?;
        let g = gem(f, &m, 8 * P, AMDGPU_GEM_DOMAIN_GTT, 0)?;
        va(f, &m, v, AMDGPU_VA_OP_MAP, RW, at, 0, vsize)?;
        va(f, &m, g, AMDGPU_VA_OP_MAP, RW, at + 16 * M, 0, 8 * P)?;
        let want = f.drm_file().bo(v).ok_or("no object")?.memory().page_phys(1).ok_or("no frame")?;
        match hw.walk(at + P) {
            Ok(Walk::Page(l)) if l.phys == want && l.target == Target::Local => {}
            w => return Err(format!("thread {t} round {r}: {w:?}, want {want:#x}")),
        }
        let job = next_job(&hw)?;
        let job2 = next_job(&own)?;
        va(f, &m, v, AMDGPU_VA_OP_UNMAP, 0, at, 0, vsize)?;
        kassert!(matches!(hw.walk(at + P), Ok(Walk::Hole { .. })), "thread {t} round {r}: UNMAP left {:?}", hw.walk(at + P));
        drop(job2);
        drop(job);
        va(f, &m, 0, AMDGPU_VA_OP_CLEAR, 0, at + 16 * M, 0, 8 * P)?;
        for h in [v, g] {
            m.put(&drm_gem_close { handle: h, pad: 0 });
            f.ioctl_as(DRM_IOCTL_GEM_CLOSE, BASE, &m, KERNEL).map_err(|e| format!("GEM_CLOSE: errno {e}"))?;
        }
    }
    Ok(())
}

#[cfg(not(test))]
fn threads_on(s: &Sparse, dev: &Arc<device::Device>) -> ktest::TestResult {
    const THREADS: u64 = 8;
    let f = Arc::new(open_device(dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?);
    let m = KMem::new();
    let keep = gem(&f, &m, 16 * P, AMDGPU_GEM_DOMAIN_GTT, 0)?;
    va(&f, &m, keep, AMDGPU_VA_OP_MAP, RW, V, 0, 16 * P)?;
    let mut model = Model::default();
    model.map(&f, keep, V, 0, 16 * P, RW, Target::System { snooped: true }, Mtype::Nc)?;
    let failed: Arc<SpinLock<Option<String>>> = Arc::new(SpinLock::named("gx.ktest.failed", None));
    let done = Arc::new(AtomicU64::new(0));
    for t in 0..THREADS {
        let (f, gpu, failed, done) = (f.clone(), s.gpu.clone(), failed.clone(), done.clone());
        sched::spawn("gx-hw-ktest", sched::Class::Normal, move || {
            if let Err(w) = worker(&f, &gpu, t) {
                *failed.lock() = Some(w);
            }
            done.fetch_add(1, Ordering::AcqRel);
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
    }
    let t0 = time::now_ns();
    while done.load(Ordering::Acquire) < THREADS {
        kassert!(time::now_ns().saturating_sub(t0) < 120_000_000_000, "{} of {THREADS} threads ended in 120 s", done.load(Ordering::Acquire));
        ksync::yield_now();
    }
    if let Some(w) = failed.lock().take() {
        return Err(w);
    }
    let vm = hw_vm(&f)?;
    let hw = as_hw(&vm)?;
    model.check(hw, "after the threads")?;
    let c = hw.counts();
    kassert!(c.garbage == 0 && c.pending == 0 && c.deferred == 0 && c.held == 1, "held back after the last job: {c:?}");
    Ok(())
}

/// GEM_VA, GEM_CREATE and jobs from several threads at once: every thread's mappings walk right all the time, one
/// mapping nobody touches stays, and the books come out at zero.
#[cfg(not(test))]
fn threads() -> ktest::TestResult {
    let s = sparse(512 * M)?;
    let dev = register(&s.gpu)?;
    let r = threads_on(&s, &dev);
    device::unregister(&dev);
    r?;
    let h = s.gpu.heaps().bytes();
    kassert!(h.user_used == 0 && h.table_used == 0, "left: {h:?}");
    kassert!(s.gpu.vms_alive() == 0 && s.gpu.vm_ids().pasids() == 0 && s.gpu.memory().gtt_usage == 0, "a VM, a PASID or GTT left");
    kassert!(!s.watch.alias.load(Ordering::Acquire), "a range with a window went back, or got a second one");
    Ok(())
}
#[cfg(not(test))]
ktest::ktest!("gx.hw.threads", Safe, threads);

/// The real window type on a device both QEMU and the Mini-PC have: the HPET's registers, read only.
#[cfg(not(test))]
fn window() -> ktest::TestResult {
    use hal::drv_gx::CarveWindow;
    use hal::paging::CacheType;
    const HPET: u64 = 0xfed0_0000;
    let w = CarveWindow::map(HPET, P, CacheType::Uncached).map_err(|e| format!("map: {e:?}"))?;
    kassert!(w.len() == P && w.phys() == HPET, "{w:?}");
    kassert!(w.read64(0).is_some_and(|c| c != 0 && c != u64::MAX), "the HPET's capabilities: {:x?}", w.read64(0));
    kassert!(w.read64(P - 8).is_some() && w.read64(P).is_none() && w.read64(P - 4).is_none() && w.read64(4).is_none() && w.read64(u64::MAX - 7).is_none(), "bounds of a load");
    kassert!(!w.write64(P, 0) && !w.write64(4, 0) && !w.write(P - 1, &[0, 0]) && !w.fill(P - 8, 16, 0) && !w.fill(0, 4, 0), "bounds of a store");
    let mut b = [0u8; 2];
    kassert!(!w.read(P - 1, &mut b) && w.read(0, &mut b), "bounds of a read");
    Ok(())
}
#[cfg(not(test))]
ktest::ktest!("gx.hw.window", Safe, window);

/// The real window's stores, on QEMU's framebuffer (never a screen anybody looks at, and write-combined as the boot's
/// own mapping of it, so no second type).
#[cfg(not(test))]
fn window_stores() -> ktest::TestResult {
    use hal::drv_gx::CarveWindow;
    use hal::paging::CacheType;
    let (_, phys, size) = mm::kernel_tables::framebuffer().ok_or("no framebuffer in this QEMU")?;
    kassert!(size >= 2 * P, "a framebuffer of {size} bytes");
    let w = CarveWindow::map(phys, 2 * P, CacheType::WriteCombining).map_err(|e| format!("map: {e:?}"))?;
    let mut saved = alloc::vec![0u8; 2 * P as usize];
    kassert!(w.read(0, &mut saved), "read");
    kassert!(w.fill(8, 2 * P - 16, 0x1122_3344_5566_7788), "fill");
    hal::drv_gx::store_barrier();
    kassert!(w.read64(8) == Some(0x1122_3344_5566_7788) && w.read64(2 * P - 16) == Some(0x1122_3344_5566_7788), "the fill: {:x?}", w.read64(8));
    kassert!(w.write64(P, 0xa5a5) && w.write(P + 8, &[1, 2, 3]), "stores");
    hal::drv_gx::store_barrier();
    let mut b = [0u8; 3];
    kassert!(w.read64(P) == Some(0xa5a5) && w.read(P + 8, &mut b) && b == [1, 2, 3], "read back {b:?}");
    kassert!(w.write(0, &saved), "restore");
    hal::drv_gx::store_barrier();
    Ok(())
}
#[cfg(not(test))]
ktest::ktest!("gx.hw.window_stores", QemuOnly, window_stores);

/// The ABI program of GEM_VA on the backend's own path, against what CachyOS answered (the fixture from the real GPU):
/// libc, the ioctl, GEM_CREATE, MAP/UNMAP/REPLACE/CLEAR, the page tables behind them. In QEMU on the sparse carve-out;
/// the Mini-PC's check list runs the same program and the other three on the chip.
#[cfg(not(test))]
fn abi_va() -> ktest::TestResult {
    const PROG: &str = "/bin/abi-drm-va";
    const EXPECTED: &str = include_str!("../../../../host/tests/dr/fixtures/abi-drm-va.txt");
    kassert!(vfs::root_ns().lookup(PROG).is_ok(), "{PROG} is not in this image");
    let s = sparse(512 * M)?;
    let dev = drm::add_device(s.gpu.clone()).map_err(|e| format!("add: errno {e}"))?;
    let lowest = drm::device::devices().iter().map(|d| d.render_minor()).min();
    let r = (|| -> ktest::TestResult {
        kassert!(lowest == Some(dev.render_minor()), "another node would answer the program: {lowest:?}, ours {}", dev.render_minor());
        let out = proc::stdio::Capture::new(1 << 16);
        let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
        let argv = ["abi-drm-va"];
        let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
        o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
        o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
        let p = proc::spawn_with(PROG, &o).map_err(|e| format!("spawn {PROG}: {e}"))?;
        let ended = p.wait(Some(60_000_000_000)).is_some();
        if !ended {
            p.kill(9);
            p.wait(Some(5_000_000_000));
        }
        let text = String::from_utf8_lossy(&out.contents()).into_owned();
        kassert!(ended && p.status() == Some(proc::ExitStatus::Exited(0)), "{PROG} ended {:?}: {text}", p.status());
        let diff: Vec<(&str, &str)> = text.lines().zip(EXPECTED.lines()).filter(|(a, b)| a != b).collect();
        kassert!(diff.is_empty() && text.lines().count() == EXPECTED.lines().count(), "{PROG} differs from Linux: {diff:?}");
        kassert!(!s.watch.kinds.lock().is_empty() && s.watch.kinds.lock().iter().any(|k| k.1 == MemKind::Tables), "the program did not reach this backend");
        Ok(())
    })();
    drm::remove_device(&dev);
    r?;
    let h = s.gpu.heaps().bytes();
    kassert!(h.user_used == 0 && h.table_used == 0 && s.gpu.vms_alive() == 0, "left after the program: {h:?}");
    Ok(())
}
#[cfg(not(test))]
ktest::ktest!("gx.hw.abi_va", QemuOnly, abi_va);
