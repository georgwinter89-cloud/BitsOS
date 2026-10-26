//! The kernel side: PCI probe for class 0C0330, the kernel `Env` (BAR0 through `hal::mmio::IoMem`, DMA through
//! `mm::dma`, `time` clock, PCI bus master), per controller an event thread, a completion thread and a hotplug
//! worker, and the poller.
//!
//! Polled for now (no MSI-X before PC-3): the poller (`hal::vectors::register_poller`, called from the idle loop)
//! only looks whether an event ring has something, notes the time (`Controller::interrupt`: the input stamp) and
//! wakes that controller's event thread (class KernelRt since US-ev, B4: it drains the ring, hands pipe reports
//! to their sinks, sleeps with a 100 ms deadline and checks the controller's health on each round). Transfer ends
//! (usb-msd's chained callbacks) go to the `{name}-done` thread (class Normal), so input never waits behind
//! storage work and storage work never runs at KernelRt. PC-3 replaces the
//! poller by MSI-X: interrupter 0 is prepared (IMOD set at start), the vector's handler calls
//! `Controller::interrupt`, which wakes the same thread. The hotplug worker (class Normal) sleeps until a port
//! change, a hub report or a debounce deadline wakes it (at most `HOTPLUG_TICK_NS`).
//!
//! PCI (PC-2, V1 `xhcipci.asm` order): `disable_device` first (MSI/MSI-X and bus master off: a controller a
//! previous kernel left running must not DMA while we take it), `enable_device(false)` (D0, memory decoding,
//! INTx off, stale MSI cleared), and only after HCRST `enable_device(true)`: bus master on the function and on
//! every bridge up to the root (the Mini-PC trap of `usb.md` §8). Quiesce uses `disable_device` (never logs).

use crate::hc::{Controller, Env, DMA_BOUNDARY, MS};
use crate::quirks;
use crate::regs::{walk_ext_caps, Caps};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::mmio::IoMem;
use hal::paging::CacheType;
use ksync::{SpinLock, WaitQueue};
use mm::dma::{DmaBuffer, DmaCache, DmaConstraints};

/// xHCI: class 0C (serial bus), subclass 03 (USB), prog-if 30 (xHCI).
pub const CLASS_XHCI: u32 = 0x0C_03_30;
/// The event thread's deadline per round (health watch interval).
pub const THREAD_TICK_NS: u64 = 100 * MS;
/// BARs above this are not an xHCI register window (bound against a broken BAR, not a count limit).
const BAR_MAX: u64 = 64 << 20;
/// The hotplug worker sleeps at most this long without a reason to wake.
pub const HOTPLUG_TICK_NS: u64 = 1000 * MS;

pub struct KernelEnv {
    io: IoMem,
    pci: pci::PciFunction,
}

impl Env for KernelEnv {
    type Mem = DmaBuffer;
    fn read32(&self, off: usize) -> u32 {
        self.io.read32(off).unwrap_or(u32::MAX)
    }
    fn write32(&self, off: usize, v: u32) {
        let _ = self.io.write32(off, v);
    }
    fn bar_len(&self) -> usize {
        self.io.size()
    }
    fn alloc(&self, len: usize, align: u64, limit: u64) -> Option<DmaBuffer> {
        // Write-back: stores reach the controller in program order without a barrier (`mem.rs`).
        let c = DmaConstraints { limit, align, boundary: DMA_BOUNDARY, cache: DmaCache::WriteBack };
        DmaBuffer::alloc(len, c).ok()
    }
    fn alloc_data(&self, len: usize, limit: u64) -> Option<DmaBuffer> {
        let c = DmaConstraints { limit, align: 4096, boundary: 0, cache: DmaCache::WriteBack };
        DmaBuffer::alloc(len, c).ok()
    }
    fn now_ns(&self) -> u64 {
        time::now_ns()
    }
    fn delay_ns(&self, ns: u64) {
        time::delay_ns(ns)
    }
    fn sleep_ns(&self, ns: u64) {
        sched::sleep_ns(ns)
    }
    fn relax(&self, may_sleep: bool) {
        if may_sleep {
            ksync::yield_now();
        } else {
            hal::cpu::spin_hint();
        }
    }
    fn wait(&self, wq: &WaitQueue, cond: &mut dyn FnMut() -> bool, timeout_ns: u64) {
        wq.wait_until_timeout(cond, timeout_ns);
    }
    fn bus_master(&self, on: bool) -> Result<(), String> {
        if on {
            self.pci.enable_device(true).map(|_| ()).map_err(|e| format!("{e:?}"))
        } else {
            self.pci.disable_device();
            Ok(())
        }
    }
}

impl KernelEnv {
    pub fn pci(&self) -> &pci::PciFunction {
        &self.pci
    }
}

pub type Xhci = Controller<KernelEnv>;

static CONTROLLERS: SpinLock<Vec<Arc<Xhci>>> = SpinLock::named("xhci.controllers", Vec::new());
static POLLER: ksync::Once<()> = ksync::Once::new();

static MATCHES: [pci::Match; 1] = [pci::Match::Class { class: CLASS_XHCI, mask: 0xFF_FF_FF }];
pci::pci_driver!(pci::DriverEntry { name: "xhci", matches: &MATCHES, opt_in_key: None, probe });

pub fn is_xhci(f: &pci::PciFunction) -> bool {
    f.class == CLASS_XHCI
}

/// Every controller the driver took over (also failed ones, with their reason), in probe order.
pub fn controllers() -> Vec<Arc<Xhci>> {
    CONTROLLERS.lock().clone()
}

/// MSI/MSI-X and bus master off, then D0, memory decoding, INTx off (bus master only after the reset).
fn prepare_pci(f: &pci::PciFunction) -> Result<(), String> {
    f.disable_device_logged();
    f.enable_device(false).map(|_| ()).map_err(|e| format!("enable_device: {e:?}"))
}

fn probe(f: &pci::PciFunction) -> Result<(), String> {
    let function = format!("{}", f.addr);
    let fail = |e: String| {
        f.disable_device_logged();
        klog::event!(Error, "usb", "xhci.probe_failed", function = function.as_str(), error = e.as_str());
        e
    };
    prepare_pci(f).map_err(fail)?;
    let Some(pci::Bar::Mem { phys, size, .. }) = f.bar(0) else {
        return Err(fail("BAR0 is not a memory BAR".into()));
    };
    if !(0x100..=BAR_MAX).contains(&size) {
        return Err(fail(format!("BAR0 size {size:#x} is not an xHCI register window")));
    }
    let io = hal::mmio::map_device(phys, size as usize, CacheType::Uncached).map_err(|e| fail(format!("map BAR0: {e:?}")))?;
    let env = KernelEnv { io, pci: f.clone() };
    let caps = Caps::parse(|o| env.read32(o), env.bar_len()).map_err(|e| fail(String::from(e)))?;
    let ext = walk_ext_caps(|o| env.read32(o), caps.xecp, env.bar_len(), caps.max_ports);
    let quirks = quirks::for_id(f.vendor, f.device);
    let mut list = CONTROLLERS.lock();
    let name = format!("xhci{}", list.len());
    klog::event!(Info, "usb", "xhci.probe", ctrl = name.as_str(), function = function.as_str(),
        vendor = klog::Hex(f.vendor as u64), device = klog::Hex(f.device as u64), bar = klog::Hex(phys),
        bar_len = klog::Hex(size), version = klog::Hex(caps.version as u64), slots = caps.max_slots,
        ports = caps.max_ports, interrupters = caps.max_intrs, context_bytes = caps.csz.bytes(),
        scratchpads = caps.scratchpads, erst_max = caps.erst_max, legacy = ext.legacy.is_some(),
        protocols = ext.protocols.len(), bad_protocols = ext.bad_protocols, trust_tx_length = quirks.trust_tx_length);
    if ext.bad_protocols > 0 {
        klog::event!(Warn, "usb", "xhci.bad_protocol_caps", ctrl = name.as_str(), count = ext.bad_protocols;
            "supported-protocol capabilities with a wrong name or ports outside the controller were ignored");
    }
    for p in &ext.protocols {
        let psi: Vec<String> = p.psi.iter().map(|d| format!("{d:#x}")).collect();
        klog::event!(Debug, "usb", "xhci.protocol", ctrl = name.as_str(), usb = format!("{}.{:x}", p.major, p.minor),
            first_port = p.first_port, ports = p.ports, slot_type = p.slot_type, psic = p.psi.len(), psi = psi.join(" "));
    }
    let c = Arc::new(Controller::new(name.clone(), env, caps, ext).with_quirks(quirks));
    list.push(c.clone());
    drop(list);
    devmodel::register_quiesce(c.clone());
    // B4 (US-ev): the event ring and pipe reports (input) on KernelRt; transfer ends (usb-msd's callbacks) on a
    // Normal thread of their own, so input never waits behind storage and storage never runs at KernelRt.
    let t = c.clone();
    let events_class = match sched::spawn(&format!("{name}-done"), sched::Class::Normal, move || completion_thread(t)) {
        Ok(_) => {
            c.set_done_alive();
            sched::Class::KernelRt
        }
        // Without it the event thread hands transfer ends on itself (as before US-ev). usb-msd's callbacks must
        // not run at KernelRt (B4), so the event thread then keeps the class it had before the split.
        Err(e) => {
            klog::event!(Error, "usb", "xhci.thread_failed", ctrl = name.as_str(), thread = "done", error = format!("{e:?}"));
            klog::event!(Warn, "usb", "xhci.done_fallback", ctrl = name.as_str(), events_class = "Interactive";
                "transfer ends are delivered by the event thread, which runs Interactive instead of KernelRt");
            sched::Class::Interactive
        }
    };
    let t = c.clone();
    match sched::spawn(&format!("{name}-events"), events_class, move || event_thread(t)) {
        Ok(_) => c.set_thread_alive(),
        // Without the thread, command waiters drain the event ring themselves (slower, still correct).
        Err(e) => klog::event!(Error, "usb", "xhci.thread_failed", ctrl = name.as_str(), error = format!("{e:?}")),
    }
    POLLER.get_or_init(|| hal::vectors::register_poller("xhci", poll_all));
    // Bring-up plus the boot No-Op; a controller failing either is parked with its reason.
    let r = c.start_checked();
    let h = c.clone();
    if let Err(e) = sched::spawn(&format!("{name}-hotplug"), sched::Class::Normal, move || hotplug_thread(h)) {
        klog::event!(Error, "usb", "xhci.thread_failed", ctrl = name.as_str(), thread = "hotplug", error = format!("{e:?}"));
    }
    r
}

fn hotplug_thread(c: Arc<Xhci>) {
    let mut hp = crate::hotplug::Hotplug::new();
    loop {
        let next = hp.round(&c);
        let now = time::now_ns();
        let wait = next.map(|t| t.saturating_sub(now)).unwrap_or(HOTPLUG_TICK_NS).clamp(MS, HOTPLUG_TICK_NS);
        c.hotplug_wq.wait_until_timeout(|| c.take_kick(), wait);
    }
}

fn event_thread(c: Arc<Xhci>) {
    loop {
        c.ev_wq.wait_until_timeout(|| c.event_pending() || c.reports.ready(), THREAD_TICK_NS);
        c.thread_round();
    }
}

fn completion_thread(c: Arc<Xhci>) {
    loop {
        c.done_wq.wait_until_timeout(|| c.completions.ready(), THREAD_TICK_NS);
        c.completion_round();
    }
}

/// The poller: wakes the event thread of every controller with an event waiting. Runs from the idle loop, so it
/// does not allocate: it walks the list under `try_lock` and skips a round instead of waiting when a probe holds
/// it. Lock order CONTROLLERS -> xhci.ev (try) -> the wait queue; nothing takes CONTROLLERS under those.
fn poll_all() {
    let Some(list) = CONTROLLERS.try_lock() else { return };
    for c in list.iter() {
        if c.event_pending() {
            c.interrupt();
        }
    }
}
