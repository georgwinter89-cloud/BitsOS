//! Intel I226-V / 82576. Owner: NW. Item NW-2: the Mini-PC's cable cards (two I226-V, 8086:125C) and QEMU's
//! `-device igb` (82576, 8086:10C9), their stand-in in tests, behind `devmodel::net::NetDevice` and `Quiesce`.
//!
//! Re-implemented from the documented behaviour (spec `net.md` §2, V1 `net/igc*.asm` with file:line at each
//! value, Linux `igc`), not translated. Layout:
//! - `regs`: the register map, per-model offsets (IMC/ICR/IMS, PHY address);
//! - `reset`, `phy`, `ring`, `irq`: the register sequences as functions over `hw::Hw`, without klog;
//! - `device::Card<H>`: bring-up order, data path, link watch, restart rules, Quiesce; logs (B6);
//! - `mmio_hw::MmioHw`: the kernel's `Hw`; `fake` (tests only): a register-level fake of the card.
//!
//! NW-4: receive and transmit completions arrive by MSI-X (two vectors: link and queue 0, Linux igc's layout),
//! else MSI (V1's measured EICR/EIMS rules), through PC-3's `alloc_irqs`; the hard-IRQ hook wakes the stack's
//! poll thread, which reads the ring and re-arms the vector (NAPI). Without vectors (or with CONFIG `net_irq=off`)
//! the card is polled by the stack as at M0 (logged). Housekeeping (link look, health, restarts) stays on one
//! poller for every card. Restarts (seconds in the worst case: PHY semaphore, resets) run on the `igc.restart`
//! worker thread (class KernelRt) with sleeping waits, not in the poller, which the idle thread shares with every
//! driver and the timers (B4). Bus master on bridges is PC-2's `enable_device`: on the Mini-PC this driver needs
//! it (root port 00:02.1 has bus master off).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod device;
#[cfg(test)]
mod fake;
pub mod hw;
pub mod irq;
mod ktests;
pub mod mem;
mod mmio_hw;
pub mod phy;
pub mod regs;
pub mod reset;
pub mod ring;
#[cfg(test)]
mod tests;

pub use device::{BringUpError, Card, Config, HwStats, State};
pub use mmio_hw::MmioHw;

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use devmodel::net::{LinkState, MacAddr, NetDevice, NetStats};
use devmodel::{DevError, Quiesce};
use ksync::{SpinLock, WaitQueue};
use regs::Model;

/// A card in the kernel.
pub type Igc = Card<MmioHw>;

static MATCHES: [pci::Match; 2] = [
    pci::Match::Id { vendor: regs::VENDOR_INTEL, device: regs::DEV_I226_V },
    pci::Match::Id { vendor: regs::VENDOR_INTEL, device: regs::DEV_82576 },
];

pci::pci_driver!(pci::DriverEntry { name: "igc", matches: &MATCHES, opt_in_key: None, probe });

struct Entry {
    card: Arc<Igc>,
    /// Registered with `net::netdev` (after its first successful bring-up, when the MAC is known).
    registered: bool,
}

static DEVICES: SpinLock<Vec<Entry>> = SpinLock::named("igc.devices", Vec::new());
static POLLER: ksync::Once<()> = ksync::Once::new();
/// Whether the restart worker runs (false: it could not be started, restarts run in the poller).
static WORKER: ksync::Once<bool> = ksync::Once::new();
/// The poller saw a queued restart; the worker sleeps on `WAKE` until then (or `WORKER_IDLE_NS`).
static DUE: AtomicBool = AtomicBool::new(false);
static WAKE: WaitQueue = WaitQueue::new();
/// The worker looks at the cards at least this often even without a wake-up (bounded wait, B4).
const WORKER_IDLE_NS: u64 = 1_000_000_000;

/// True for a function this driver drives.
pub fn is_igc(f: &pci::PciFunction) -> bool {
    f.vendor == regs::VENDOR_INTEL && Model::from_device(f.device).is_some()
}

/// Every card this driver kept, in probe order (bus order: the first card is the first on the bus).
pub fn devices() -> Vec<Arc<Igc>> {
    DEVICES.lock().iter().map(|e| e.card.clone()).collect()
}

fn probe(f: &pci::PciFunction) -> Result<(), String> {
    let Some(model) = Model::from_device(f.device) else { return Err("not an I226-V or 82576".into()) };
    let function = format!("{}", f.addr);
    // MSI and bus master off before anything else, also before mapping (PC-2 `disable_device`: a running ring
    // must write nowhere). Memory decode is switched on by the bring-up (`enable_device`), after it checked
    // the BARs.
    let _ = f.disable_device();
    klog::event!(Info, "net", "igc.probe", function = function.as_str(), model = model.name(), device = klog::Hex(f.device as u64));
    let fail = |e: String| {
        klog::event!(Error, "net", "igc.probe_failed", function = function.as_str(), error = e.as_str());
        e
    };
    let hw = MmioHw::map(f).map_err(fail)?;
    let index = DEVICES.lock().len();
    let card = Arc::new(Card::new(format!("igc{index}"), model, hw, config()).map_err(fail)?);
    card.set_restart_worker(*WORKER.get_or_init(start_worker));
    // Quiesce from here on, even for a card that failed: a retry may bring it up later.
    devmodel::register_quiesce(card.clone());
    match card.start() {
        Ok(()) => {}
        Err(e) if e.fatal() => return Err(e.text()),
        // Kept as Failed: the poller retries (10 s x 6, then 60 s); the cable may be the only way in.
        Err(_) => {}
    }
    DEVICES.lock().push(Entry { card, registered: false });
    POLLER.get_or_init(|| hal::vectors::register_poller("igc", poll_all));
    register_new();
    Ok(())
}

/// CONFIG: `net_irq=off` keeps every card polled (a way back if MSI-X misbehaves on a machine);
/// `igc_itr_us=<n>` sets the queue vector's gap between interrupts (default 49 µs, about 20,000 a second).
fn config() -> Config {
    let mut c = Config::default();
    if net::config("net_irq").is_some_and(|v| v.trim() == "off") {
        c.irqs = false;
    }
    if let Some(us) = net::config("igc_itr_us").and_then(|v| v.trim().parse::<u32>().ok()) {
        // EITR's interval field holds up to 8191 µs; 0 would turn moderation off (an interrupt per frame).
        c.itr_us = us.clamp(1, 8191);
    }
    c
}

/// Cards that came up for the first time join the stack's device list. False if the list was busy (the next
/// poll tries again: the decision comes from the list, not from a one-shot flag).
fn register_new() -> bool {
    let fresh: Vec<Arc<Igc>> = {
        let Some(mut l) = DEVICES.try_lock() else { return false };
        l.iter_mut()
            .filter(|e| !e.registered && e.card.state() == State::Running)
            .map(|e| {
                e.registered = true;
                e.card.clone()
            })
            .collect()
    };
    for c in fresh {
        klog::event!(Info, "net", "igc.registered", ifname = c.name());
        net::netdev::register(c);
    }
    true
}

/// The poller: housekeeping for every card, outside the list lock; skips a round instead of waiting if a
/// probe holds the list. Restarts are queued for the worker.
fn poll_all() {
    // By index, one card at a time: the idle loop calls this all the time and the G0 heap never reuses memory,
    // so a copy of the list per call would eat RAM (NW-3 found ~70 MB/s).
    let Some(n) = DEVICES.try_lock().map(|l| l.len()) else { return };
    let (mut unregistered_up, mut due) = (false, false);
    for i in 0..n {
        let Some((c, registered)) = DEVICES.try_lock().and_then(|l| l.get(i).map(|e| (e.card.clone(), e.registered))) else { continue };
        c.poll();
        unregistered_up |= !registered && c.state() == State::Running;
        due |= c.restart_due();
    }
    if unregistered_up && !register_new() {
        klog::event!(Debug, "net", "igc.register_deferred"; "device list busy, next poll");
    }
    if due {
        DUE.store(true, Ordering::Release);
        WAKE.wake_one();
    }
}

fn start_worker() -> bool {
    match sched::spawn("igc.restart", sched::Class::KernelRt, worker) {
        Ok(_) => {
            klog::event!(Info, "net", "igc.worker", class = "kernel_rt");
            true
        }
        Err(e) => {
            let e = format!("{e:?}");
            klog::event!(Warn, "net", "igc.worker_failed", error = e.as_str(); "restarts run in the poller");
            false
        }
    }
}

/// The restart worker: sleeps until the poller queued a restart (at most `WORKER_IDLE_NS`), then runs every
/// queued one with sleeping waits.
fn worker() {
    loop {
        WAKE.wait_until_timeout(|| DUE.load(Ordering::Acquire), WORKER_IDLE_NS);
        DUE.store(false, Ordering::Release);
        for c in devices() {
            c.run_due_restart();
        }
    }
}

impl<H: hw::Hw> NetDevice for Card<H> {
    fn name(&self) -> &str {
        Card::name(self)
    }
    fn mac(&self) -> MacAddr {
        Card::mac(self)
    }
    fn link(&self) -> LinkState {
        Card::link(self)
    }
    fn transmit(&self, frame: &[u8]) -> Result<(), DevError> {
        Card::transmit(self, frame)
    }
    fn receive(&self, buf: &mut [u8]) -> Option<usize> {
        Card::receive(self, buf)
    }
    fn poll_tx(&self, frame: &[u8]) -> bool {
        Card::poll_tx(self, frame)
    }
    fn poll(&self) {
        Card::poll(self)
    }
    fn stats(&self) -> NetStats {
        Card::stats(self)
    }
}

impl<H: hw::Hw> Quiesce for Card<H> {
    fn name(&self) -> &str {
        Card::name(self)
    }
    fn quiesce(&self) {
        Card::quiesce(self)
    }
}
