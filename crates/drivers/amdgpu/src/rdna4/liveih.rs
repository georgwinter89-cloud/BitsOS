//! Wave 42: the kernel glue of a card's system memory, its one MSI vector and its IH rings: system pages for the
//! card, the thread that reads the rings when a message arrives, and the Reload hook that switches them off. The
//! reader reaches the card only through `ihring::IhIo`, which can write nothing but a ring's control register (the
//! overflow clear), the two IH doorbells and, for a page fault, the clear bit of the faulting hub's fault control
//! (it reads that hub's fault status first; wave 43, `ihfault`).
//!
//! What needs PCI configuration space is `hostprep`'s chain over `livepci`, run before a stage. Pages handed to
//! the card stay for the boot (it may write there until it is reset), and a Reload switches the IH off as Linux
//! does (`ihoff::irq_disable`, the hook registered by the chain before the function is enabled), so no entry is
//! written into the next kernel's memory. The hook writes nothing while no ring is on.
//!
//! Wave 44: the thread also reaches the card for the thermal taker (`LiveCard`, behind `thermalhw`'s gate: the
//! thermal and SMU registers and the driver table, nothing else gets through), wakes when the software CTF check
//! is due (`thermal::wait_ns`), and runs a power-off the check decided after it dropped the session's lock
//! (`thermal::OffWatch`, `PlatformOff`); one that did not happen it tries again every 10 s, since the quiesce it
//! ran switched the IH off and no alert reaches the thread any more. The thread runs outside every stage call
//! from the moment `ih` passed; while a stage holds the session it waits for it (stages are bounded), which is
//! the row `thm-stage-delay` of section 7.

use super::ihoff::{self, OffIo, OffRegs};
use super::ihring::IhIo;
use super::thermal::{self, Look, OffWatch, PowerOff};
use super::session::Session;
use crate::gpucore::psp::Hw;
use super::nbregs::NbRegs;
use crate::devs::{Device, Key};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::mmio::IoMem;
use hal::paging::CacheType;
use ksync::Mutex;
use mm::dma::{DmaBuffer, DmaConstraints};
use rdna4_core::hub41;
use rdna4_core::ih7::{self, Ring};

/// With no message the reader still looks once a second (a message lost on the way shows as entries then).
const IDLE_NS: u64 = 1_000_000_000;

/// System memory handed to cards, kept for the boot.
static PAGES: Mutex<Vec<(Key, DmaBuffer)>> = Mutex::named("amdgpu.rdna4.pages", Vec::new());
/// Each card's MSI vector.
static LINES: Mutex<Vec<(Key, Arc<Line>)>> = Mutex::named("amdgpu.rdna4.msi", Vec::new());

struct Line {
    /// Kept so the vector stays allocated.
    _set: pci::IrqSet,
    handle: pci::IrqHandle,
    text: String,
}

/// The card's one vector (`amdgpu_irq_init`, amdgpu_irq.c:307-350: one vector; this card has MSI and no MSI-X).
pub fn msi(dev: &Device) -> Result<String, String> {
    let f = dev.pci.as_ref().ok_or("no PCI function")?;
    if let Some((_, l)) = LINES.lock().iter().find(|(k, _)| *k == dev.key) {
        return Ok(l.text.clone());
    }
    // Not under `LINES`: the PCI core takes its own locks, which lie outside every driver lock. A second caller
    // at the same moment is refused by the core (one set per function).
    let set = f.alloc_irqs(&pci::IrqRequest::new("amdgpu-rdna4", 1, 1).modes(false, true, false)).map_err(|e| format!("{e}"))?;
    let handle = set.handle(0).ok_or("the vector has no handle")?;
    let text = match set.vector(0) {
        Some(v) => format!("{} vector {:#x} on CPU {}", set.mode().name(), v.vector, v.cpu),
        None => format!("{} with no vector", set.mode().name()),
    };
    LINES.lock().push((dev.key, Arc::new(Line { _set: set, handle, text: text.clone() })));
    Ok(text)
}

/// Zeroed system memory below the card's 44 bits, kept for the boot; its address as the card sees it (the physical
/// address: the survey said no IOMMU translates).
pub fn pages(dev: &Device, bytes: u64) -> Option<u64> {
    let len = usize::try_from(bytes).ok()?;
    let buf = match DmaBuffer::alloc(len, DmaConstraints::from_mask_bits(rdna4_core::layout::DMA_BITS)) {
        Ok(b) => b,
        Err(e) => {
            klog::event!(Warn, "gx", "rdna4.pages", bytes = bytes, why = format!("{e:?}").as_str(); "no system memory for the card");
            return None;
        }
    };
    if !buf.zero() {
        return None;
    }
    let at = buf.device_addr();
    klog::event!(Info, "gx", "rdna4.pages", card = dev.index, at = klog::Hex(at), bytes = bytes; "system memory handed to the card for the rest of the boot");
    PAGES.lock().push((dev.key, buf));
    Some(at)
}

fn with_buf<R>(key: Key, at: u64, len: usize, f: impl FnOnce(&DmaBuffer, usize) -> R) -> Option<R> {
    let pages = PAGES.lock();
    let (_, buf) = pages.iter().find(|(k, b)| *k == key && at >= b.device_addr() && at - b.device_addr() + len as u64 <= b.len() as u64)?;
    Some(f(buf, (at - buf.device_addr()) as usize))
}

pub fn sys_read(dev: &Device, at: u64, out: &mut [u8]) -> bool {
    with_buf(dev.key, at, out.len(), |b, off| b.copy_out(off, out)).unwrap_or(false)
}

pub fn sys_write(dev: &Device, at: u64, data: &[u8]) -> bool {
    with_buf(dev.key, at, data.len(), |b, off| b.copy_in(off, data)).unwrap_or(false)
}

/// A ring's two registers the reader reaches, as BAR5 bytes: (IH_RB_WPTR, IH_RB_CNTL).
fn ring_regs(n: &NbRegs, ring: Ring) -> Option<(u32, u32)> {
    match ring {
        Ring::Ring0 => Some((n.byte(ih7::RB_WPTR)?, n.byte(ih7::RB_CNTL)?)),
        Ring::Ring1 => Some((n.byte(ih7::RB_WPTR_RING1)?, n.byte(ih7::RB_CNTL_RING1)?)),
    }
}

/// The rings as the reader reaches them on the card.
struct LiveIo {
    key: Key,
    sys: u64,
    bar5: IoMem,
    /// BAR2's first page: both IH doorbells lie in it.
    doorbells: IoMem,
    regs: [(u32, u32); 2],
    /// Per hub (GFXHUB, MMHUB): its fault status and its fault control.
    faults: [(u32, u32); 2],
    /// The card for the thermal taker.
    card: LiveCard,
}

/// The card's registers and VRAM for the thermal taker, each access behind `thermalhw::Gated`.
struct LiveCard {
    bar5: IoMem,
    bar0: super::attach::BarFact,
}

impl Hw for LiveCard {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.bar5.read32(byte as usize).ok()
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        self.bar5.write32(byte as usize, value).is_ok()
    }
    fn mem_read(&mut self, at: u64, buf: &mut [u8]) -> bool {
        super::live::window_read(&self.bar0, at, buf)
    }
    fn mem_write(&mut self, _at: u64, _data: &[u8]) -> bool {
        false
    }
    fn now_us(&mut self) -> u64 {
        time::now_ns() / 1000
    }
    // Linux's polls are reads 1 us apart (`udelay`), not sleeps.
    fn delay_us(&mut self, us: u64) {
        time::delay_us(us)
    }
}

/// The platform's orderly power-off (sync, the drivers' quiesce hooks: the card's engines stopped, GFXHUB and
/// the IH off; then ACPI S5), Linux's `orderly_poweroff(true)` for the software CTF.
struct PlatformOff;

impl PowerOff for PlatformOff {
    fn power_off(&mut self, why: &str) -> String {
        klog::event!(Error, "gx", "rdna4.thermal.poweroff", why = why; "the card is too hot: BitsOS switches the PC off in order (files written back, the card stopped)");
        format!("{:?}", platform::power::poweroff())
    }
}

impl LiveIo {
    fn regs(&self, ring: Ring) -> (u32, u32) {
        self.regs[usize::from(ring == Ring::Ring1)]
    }
}

impl IhIo for LiveIo {
    fn mem32(&mut self, off: u64) -> Option<u32> {
        with_buf(self.key, self.sys + off, 4, |b, o| b.read_u32(o)).flatten()
    }
    fn set_mem32(&mut self, off: u64, v: u32) -> bool {
        with_buf(self.key, self.sys + off, 4, |b, o| b.write_u32(o, v)).unwrap_or(false)
    }
    fn wptr_reg(&mut self, ring: Ring) -> Option<u32> {
        self.bar5.read32(self.regs(ring).0 as usize).ok()
    }
    fn cntl(&mut self, ring: Ring) -> Option<u32> {
        self.bar5.read32(self.regs(ring).1 as usize).ok()
    }
    fn set_cntl(&mut self, ring: Ring, v: u32) -> bool {
        self.bar5.write32(self.regs(ring).1 as usize, v).is_ok()
    }
    fn doorbell(&mut self, ring: Ring, rptr: u32) -> bool {
        let index = if ring == Ring::Ring0 { ih7::DOORBELL_RING0 } else { ih7::DOORBELL_RING1 };
        self.doorbells.write32(index as usize * 4, rptr).is_ok()
    }
    fn fault_status(&mut self, hub: hub41::Kind) -> Option<u32> {
        self.bar5.read32(self.faults[usize::from(hub == hub41::Kind::Mm)].0 as usize).ok()
    }
    fn fault_clear(&mut self, hub: hub41::Kind) -> bool {
        let cntl = self.faults[usize::from(hub == hub41::Kind::Mm)].1 as usize;
        // A fault control that reads all ones is a hub that does not answer: nothing is written back.
        self.bar5.read32(cntl).ok().filter(|v| *v != u32::MAX).is_some_and(|v| self.bar5.write32(cntl, hub41::fault_clear(v)).is_ok())
    }
    fn now_us(&mut self) -> u64 {
        time::now_ns() / 1000
    }
    fn thermal_hw(&mut self) -> Option<&mut dyn Hw> {
        Some(&mut self.card)
    }
}

/// A Reload: the IH off as Linux switches it off (`ihoff::irq_disable`), and nothing when no ring is on. No lock,
/// no allocation, no sleep (the one wait is a busy millisecond). Nothing else of the card is touched.
struct IhQuiesce {
    bar5: IoMem,
    regs: OffRegs,
}

pub(super) struct QuiesceIo<'a>(pub(super) &'a IoMem);

impl OffIo for QuiesceIo<'_> {
    fn read(&mut self, byte: u32) -> Option<u32> {
        self.0.read32(byte as usize).ok()
    }
    fn write(&mut self, byte: u32, v: u32) -> bool {
        self.0.write32(byte as usize, v).is_ok()
    }
    fn delay_us(&mut self, us: u64) {
        time::delay_us(us)
    }
}

impl devmodel::Quiesce for IhQuiesce {
    fn name(&self) -> &str {
        "amdgpu.rdna4.ih"
    }
    fn quiesce(&self) {
        let _ = ihoff::irq_disable(&mut QuiesceIo(&self.bar5), &self.regs);
    }
}

fn map(phys: u64, bytes: usize, what: &str) -> Result<IoMem, String> {
    hal::mmio::map_device(phys, bytes, CacheType::Uncached).map_err(|e| format!("{what}: {e:?}"))
}

/// Cards whose Quiesce hook is registered.
static ARMED: Mutex<Vec<Key>> = Mutex::named("amdgpu.rdna4.quiesce", Vec::new());

struct Places {
    bar0: super::attach::BarFact,
    bar5: super::attach::BarFact,
    bar2: super::attach::BarFact,
    /// Per ring: (IH_RB_WPTR, IH_RB_CNTL).
    regs: [(u32, u32); 2],
    faults: [(u32, u32); 2],
    off: OffRegs,
}

fn places(dev: &Device) -> Result<Places, String> {
    let card = super::live::card_now(dev).ok_or("the card is not attached")?;
    let att = card.state.as_ref().map_err(|_| "the attach stopped")?;
    let (Some(bar0), Some(bar2), Some(bar5)) = (card.facts.bar0, card.facts.bar2, card.facts.bar5) else { return Err(String::from("BAR0, BAR2 or BAR5 missing")) };
    let n = super::regs14::Regs14::from(&att.discovery).and_then(|r| NbRegs::from(&att.discovery, &r)).ok_or("the table does not name the registers")?;
    let regs = [ring_regs(&n, Ring::Ring0).ok_or("no ring registers")?, ring_regs(&n, Ring::Ring1).ok_or("no ring registers")?];
    let off = OffRegs::of(&n).ok_or("no ring registers")?;
    let fault = |h: &hub41::Hub| Some((n.byte(h.fault_status)?, n.byte(h.fault_cntl)?));
    let faults = [fault(&hub41::GC).ok_or("no fault registers")?, fault(&hub41::MM).ok_or("no fault registers")?];
    Ok(Places { bar0, bar5, bar2, regs, faults, off })
}

/// From now on a Reload switches the card's IH off if a ring is on, whether the `ih` stage passes or stops with a
/// ring already on (`hostprep::prepare` asks for it after its last check and before its first change).
pub fn arm_quiesce(dev: &Device) -> Result<(), String> {
    let p = places(dev)?;
    let mut armed = ARMED.lock();
    if !armed.contains(&dev.key) {
        let bar5 = map(p.bar5.phys, p.bar5.bytes as usize, "BAR5")?;
        devmodel::register_quiesce(Arc::new(IhQuiesce { bar5, regs: p.off }));
        armed.push(dev.key);
    }
    Ok(())
}

/// After a passed `ih` stage: the thread that reads the rings at each message.
pub fn start(dev: &'static Device) -> Result<(), String> {
    let p = places(dev)?;
    let sys = dev.stages.0.lock().nb.ih.as_ref().map(|i| i.place.sys).ok_or("the ih stage did not pass")?;
    let line = LINES.lock().iter().find(|(k, _)| *k == dev.key).map(|(_, l)| l.clone()).ok_or("no MSI vector")?;
    let card = LiveCard { bar5: map(p.bar5.phys, p.bar5.bytes as usize, "BAR5")?, bar0: p.bar0 };
    let mut io = LiveIo { key: dev.key, sys, bar5: map(p.bar5.phys, p.bar5.bytes as usize, "BAR5")?, doorbells: map(p.bar2.phys, 4096, "BAR2")?, regs: p.regs, faults: p.faults, card };
    sched::spawn("amdgpu-ih7", sched::Class::Interactive, move || {
        let (mut wait, mut off) = (IDLE_NS, OffWatch::default());
        let keep = |failed: Option<String>, tries: u64| {
            if let (Some(f), Some(ih)) = (failed, dev.stages.0.lock().nb.ih.as_mut()) {
                (ih.reader.thermal.off_failed, ih.reader.thermal.off_tries) = (Some(f), tries);
            }
        };
        loop {
            if off.failed.is_some() {
                sched::sleep_ns(thermal::OFF_RETRY_NS);
                let failed = off.after(|| None, &mut PlatformOff);
                keep(failed, off.tries);
                continue;
            }
            let woke = line.handle.wait(wait);
            // The session's lock: a running stage finishes first (stages are bounded).
            let mut s = dev.stages.0.lock();
            let Some(ih) = s.nb.ih.as_mut() else { break };
            let passes = ih.reader.process(&mut io);
            let critical = ih.reader.thermal.power_off.is_some();
            wait = thermal::wait_ns(IDLE_NS, ih.reader.thermal.due_in_us(time::now_ns() / 1000));
            drop(s);
            if critical {
                let take = || dev.stages.0.lock().nb.ih.as_mut().and_then(|i| i.reader.thermal.take_power_off());
                let failed = off.after(take, &mut PlatformOff);
                keep(failed, off.tries);
                continue;
            }
            if passes[0].more || passes[1].more {
                sched::yield_now();
            } else if matches!(woke, pci::Wake::Poll) {
                // A released or stormed line: no message will wake this thread, so it must not spin.
                sched::sleep_ns(IDLE_NS / 100);
            }
        }
    })
    .map(|_| ())
    .map_err(|e| format!("the IH thread: {e:?}"))
}

/// How long the read-only `gpu` tool waits for the session while no stage call runs: the IH thread holds it for
/// one pass, at most a check's one transfer (Linux's 2 000 000 reads of the answer, 1 us apart).
const LOOK_WAIT_NS: u64 = 3_000_000_000;

/// The thermal taker's limits and one fresh read of the temperatures, for the read-only `gpu` tool: one metrics
/// transfer through the taker's gate, under the session's lock and never during a stage call. The IH thread
/// takes the same lock at every pass, so a busy lock alone says nothing: the tool asks whether a stage call runs.
pub fn thermal_look(dev: &Device) -> Look {
    if super::livestage::in_call(dev.key) {
        return Look::none(alloc::vec![String::from("thermal: a stage call of this card is running; nothing read")], "a stage call is running");
    }
    let Some(mut s) = dev.stages.0.lock_timeout(LOOK_WAIT_NS) else {
        return Look::none(alloc::vec![String::from("thermal: the card's session stayed busy for 3 s (the IH thread, or a stage call that began just now); nothing read")], "the session stayed busy");
    };
    look_in(dev, &mut s)
}

/// The same for the measurement run, which holds the card's stage calls itself (`livestage::Claim`): no stage
/// runs between its stages, so it waits for the IH thread's pass like any other access of the session.
pub fn thermal_look_run(dev: &Device) -> Look {
    look_in(dev, &mut dev.stages.0.lock())
}

fn look_in(dev: &Device, s: &mut Session) -> Look {
    let Some(ih) = s.nb.ih.as_mut() else { return Look::none(alloc::vec![String::from("thermal: the stage ih did not pass in this boot: no taker, nothing read")], "no IH") };
    let t = &mut ih.reader.thermal;
    if t.sensors.is_none() {
        let mut lines = t.limit_lines();
        lines.push(String::from("temperatures: not read (the power controller's start did not pass in this boot: no table transfer goes out)"));
        lines.push(format!("thermal taker: {}", t.describe()));
        return Look { lines, temps: Err(String::from("pmfw did not pass")), limits: t.limits() };
    }
    let card = places(dev).and_then(|p| Ok(LiveCard { bar5: map(p.bar5.phys, p.bar5.bytes as usize, "BAR5")?, bar0: p.bar0 }));
    match card {
        Ok(mut c) => t.look(&mut c, time::now_ns() / 1000),
        Err(e) => Look::none(alloc::vec![format!("thermal: the card's registers could not be mapped ({e}); nothing read")], "not mapped"),
    }
}
