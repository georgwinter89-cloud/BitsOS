//! Realtek RTL8126A, the Gaming-PC's cable card (PCI 10ec:8126, Linux `RTL_GIGA_MAC_VER_70`). Owner: NW (lane GPN,
//! item GP-NET). The driver binds to that one PCI id, which the Mini-PC does not have: nothing here runs there.
//!
//! Transcribed from Linux 7.3.1; `M:` below means `drivers/net/ethernet/realtek/r8169_main.c`, `F:` means
//! `r8169_firmware.c`, `P:` means `r8169_phy_config.c`; `phy` names its phylib sources. The table of every register
//! write next to its Linux line is `plan/spec/gaming-pc.md` section 6 with the Linux lines in the code. Layout:
//! - pure, no kernel: `regs` (offsets, bits, the list of everything the driver may write), `chip` (accepted chips and
//!   their firmware), `hw` (register access: 8, 16 and 32 bits are not interchangeable on this card), `ocp` (the MAC
//!   and PHY windows), `fwscript` (the firmware file, a program of register writes), `seq` (the register sequences
//!   as tables), `phy`, `desc` (descriptors and ring rules), `irq`, `bringup` (the order);
//! - over memory and a machine: `mem`, `ring`, `machine` (what the driver needs beyond registers), `stage` (the
//!   self-check's lines), `device::Card` (bring-up, data path, link watch, restart rules, Quiesce);
//! - the kernel's side: `mmio_hw`, and the glue below (probe, poller, worker thread, the stack's view).
//!
//! The first bring-up and every restart run on the `rtl8126.worker` thread: the PHY's reset steps and the firmware
//! script take about a second, and the firmware file may have to wait for the stick. The same thread walks the
//! self-check's later stages (link, first frame, address, router, interrupt).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod bringup;
pub mod chip;
pub mod desc;
pub mod device;
#[cfg(test)]
mod fakecard;
pub mod fwscript;
pub mod hw;
pub mod irq;
mod ktests;
pub mod machine;
pub mod mem;
mod mmio_hw;
pub mod ocp;
pub mod phy;
pub mod regs;
pub mod ring;
pub mod seq;
pub mod stage;

#[cfg(test)]
mod tests;

pub use device::{BringUpError, Card, Config, State, Uplink};
pub use mmio_hw::MmioHw;

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use devmodel::net::{LinkState, MacAddr, NetDevice, NetStats};
use devmodel::{DevError, Quiesce};
use ksync::{SpinLock, WaitQueue};

/// A card in the kernel.
pub type Rtl = Card<MmioHw>;

static MATCHES: [pci::Match; 1] = [pci::Match::Id { vendor: regs::VENDOR_REALTEK, device: regs::DEVICE_8126 }];

pci::pci_driver!(pci::DriverEntry { name: "rtl8126", matches: &MATCHES, probe });

struct Entry {
    card: Arc<Rtl>,
    /// Its first bring-up was handed to the worker (or ran in probe).
    started: bool,
    /// Registered with `net::netdev` (after its first successful bring-up, when the MAC is known).
    registered: bool,
}

static DEVICES: SpinLock<Vec<Entry>> = SpinLock::named("rtl8126.devices", Vec::new());
static POLLER: ksync::Once<()> = ksync::Once::new();
/// Whether the worker runs (false: it could not be started; bring-up and restarts then run where they arise).
static WORKER: ksync::Once<bool> = ksync::Once::new();
static DUE: AtomicBool = AtomicBool::new(false);
static WAKE: WaitQueue = WaitQueue::new();
/// The worker looks at the cards this often without a wake-up: the pace of the self-check's stages.
const WORKER_TICK_NS: u64 = 250_000_000;

/// True for a function this driver drives.
pub fn is_rtl8126(f: &pci::PciFunction) -> bool {
    f.vendor == regs::VENDOR_REALTEK && f.device == regs::DEVICE_8126
}

/// Every card this driver kept, in probe order.
pub fn devices() -> Vec<Arc<Rtl>> {
    DEVICES.lock().iter().map(|e| e.card.clone()).collect()
}

/// Cards that failed before they were a `Card`.
static EARLY: stage::Early = stage::Early::new();

/// The self-check's lines of every card, for the Gaming-PC's report page: plain German, one per stage reached.
/// A card that failed before any stage could run (no register window, no memory) has its one line first.
pub fn report_lines() -> Vec<String> {
    let mut lines = EARLY.lines();
    lines.extend(devices().iter().flat_map(|c| c.report().lines()));
    lines
}

/// Stage 1 failed before the card existed: said as every stage is, and kept for the report page.
fn early_failure(ifname: &str, slot: &str, reason: &str, error: &str) {
    let text = stage::text::probe_early(reason, slot);
    klog::event!(Error, "net", "rtl8126.probe_failed", function = slot, error = error);
    klog::event!(Error, "net", "rtl8126.stage", ifname = ifname, stage = stage::Stage::Probe.name(), verdict = stage::Verdict::Failed.name();
        "{}", text.as_str());
    EARLY.set(slot, stage::numbered(stage::Stage::Probe, &text));
}

/// `Card::new` (the ring memory), its failure said as stage 1.
pub(crate) fn new_card<M: machine::Machine>(name: String, slot: String, hw: M, cfg: Config) -> Result<Card<M>, String> {
    Card::new(name.clone(), slot.clone(), hw, cfg).inspect_err(|e| early_failure(&name, &slot, stage::text::NO_RING_MEMORY, e))
}

fn probe(f: &pci::PciFunction) -> Result<(), String> {
    let function = format!("{}", f.addr);
    // MSI and bus master off before anything else, also before mapping: a ring left running must write nowhere.
    let _ = f.disable_device();
    klog::event!(Info, "net", "rtl8126.probe", function = function.as_str(), device = klog::Hex(f.device as u64),
        subsystem = klog::Hex(((f.subsystem.0 as u64) << 16) | f.subsystem.1 as u64));
    let name = format!("rtl{}", DEVICES.lock().len());
    let hw = MmioHw::map(f).map_err(|e| {
        early_failure(&name, &function, e.reason(), &e.text());
        e.text()
    })?;
    let card = Arc::new(new_card(name, function.clone(), hw, config())?);
    let worker = *WORKER.get_or_init(start_worker);
    card.set_restart_worker(worker);
    // Quiesce from here on, even for a card that fails: a retry may bring it up later.
    devmodel::register_quiesce(card.clone());
    klog::shutdown::register_tx_done(card.clone());
    DEVICES.lock().push(Entry { card: card.clone(), started: !worker, registered: false });
    POLLER.get_or_init(|| hal::vectors::register_poller("rtl8126", poll_all));
    if worker {
        DUE.store(true, Ordering::Release);
        WAKE.wake_one();
    } else {
        // No thread to sleep on: one try with spinning waits and without waiting for the firmware file.
        let _ = card.start(false);
        register_new();
    }
    Ok(())
}

/// CONFIG, read through the PCI core's lookup (the stack's own is not installed yet when drivers probe):
/// `rtl8126_max_mbps=<n>` caps the advertised speed.
fn config() -> Config {
    let get = |k: &str| pci::registry::config_value(k);
    let mut c = Config::default();
    if let Some(m) = get("rtl8126_max_mbps").and_then(|v| v.trim().parse::<u32>().ok()) {
        c.max_mbps = m.max(10);
    }
    c
}

/// Cards that came up for the first time join the stack's device list. False if the list was busy.
fn register_new() -> bool {
    let fresh: Vec<Arc<Rtl>> = {
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
        klog::event!(Info, "net", "rtl8126.registered", ifname = c.name());
        net::netdev::register(c);
    }
    true
}

fn card_at(i: usize) -> Option<(Arc<Rtl>, bool)> {
    DEVICES.try_lock().and_then(|l| l.get(i).map(|e| (e.card.clone(), e.registered)))
}

/// The poller: housekeeping for every card, outside the list lock, by index (no copy of the list per call: the idle
/// loop calls this all the time). Restarts are queued for the worker.
fn poll_all() {
    let Some(n) = DEVICES.try_lock().map(|l| l.len()) else { return };
    let (mut unregistered_up, mut due) = (false, false);
    for i in 0..n {
        let Some((c, registered)) = card_at(i) else { continue };
        c.poll();
        unregistered_up |= !registered && c.state() == State::Running;
        due |= c.restart_due();
    }
    if unregistered_up {
        register_new();
    }
    if due {
        DUE.store(true, Ordering::Release);
        WAKE.wake_one();
    }
}

fn start_worker() -> bool {
    match sched::spawn("rtl8126.worker", sched::Class::KernelRt, worker) {
        Ok(_) => {
            klog::event!(Info, "net", "rtl8126.worker", class = "kernel_rt");
            true
        }
        Err(e) => {
            klog::event!(Warn, "net", "rtl8126.worker_failed", error = format!("{e:?}"); "bring-up and restarts run without sleeping");
            false
        }
    }
}

/// The worker: first bring-ups, queued restarts and the self-check's stages, with sleeping waits.
fn worker() {
    loop {
        WAKE.wait_until_timeout(|| DUE.load(Ordering::Acquire), WORKER_TICK_NS);
        DUE.store(false, Ordering::Release);
        let n = DEVICES.lock().len();
        for i in 0..n {
            let first = {
                let mut l = DEVICES.lock();
                l.get_mut(i).map(|e| (e.card.clone(), !core::mem::replace(&mut e.started, true)))
            };
            let Some((c, first)) = first else { continue };
            if first {
                let _ = c.start(true);
            }
            c.run_due_restart();
            if c.checking().is_some() {
                c.advance(&StackUplink);
            }
        }
        register_new();
    }
}

/// The self-check's questions, answered by the kernel's stack.
struct StackUplink;

impl Uplink for StackUplink {
    fn lease(&self, ifname: &str) -> Option<([u8; 4], Option<[u8; 4]>)> {
        let card = net::stack::kernel()?.interfaces().into_iter().find(|i| i.name == ifname)?;
        card.info.addr.map(|(a, _)| (a.0, card.info.router.map(|r| r.0)))
    }
    fn ping(&self, dst: [u8; 4], timeout_ns: u64) -> Option<u64> {
        net::stack::kernel()?.ping(net::Ipv4(dst), timeout_ns).ok()
    }
}

impl<M: machine::Machine> NetDevice for Card<M> {
    fn name(&self) -> &str {
        Card::name(self)
    }
    fn mac(&self) -> MacAddr {
        Card::mac(self)
    }
    fn mtu(&self) -> usize {
        seq::MTU as usize
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

impl<M: machine::Machine> klog::shutdown::TxDone for Card<M> {
    fn name(&self) -> &str {
        Card::name(self)
    }
    fn tx_done(&self) -> Option<bool> {
        Card::tx_done(self)
    }
}

impl<M: machine::Machine> Quiesce for Card<M> {
    fn name(&self) -> &str {
        Card::name(self)
    }
    fn quiesce(&self) {
        Card::quiesce(self)
    }
}
