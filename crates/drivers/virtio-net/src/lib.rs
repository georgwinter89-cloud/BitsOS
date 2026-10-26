//! virtio-net (QEMU). Owner: NW. Item NW-1: the network card QEMU gives `xtask qemu --net user|virtio`, so the
//! stack (NW-3), the MCP server and the mirror can be tested before the I226 driver (NW-2) exists.
//!
//! Own small transport instead of the `virtio-drivers` crate: the modern virtio-pci interface (capabilities,
//! common configuration, split rings) is a few hundred lines, needs no `unsafe` here (registers through
//! `hal::mmio::IoMem`, ring memory through `mm::dma::DmaBuffer`'s typed accessors), and fits the polled,
//! lock-light shape `poll_tx` needs. Polled operation: the G0 interrupt stub has no sources yet; one poller
//! (`hal::vectors::register_poller`) does link watch, TX reclaim and restarts for every card.
//!
//! Matches 1AF4:1000 (transitional; QEMU's default on the q35 root bus) and 1AF4:1041 (modern only); both
//! are driven through the modern interface. A legacy-only device (`disable-modern=on`) is refused with an event.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod caps;
mod device;
/// The raw test frames moved to `net::frames` (NW-2 shares them); re-exported for the ktests.
pub use net::frames;
mod ktests;
mod mem;
pub mod queue;
mod transport;

pub use device::{State, VirtioNet, BUF_SIZE, DEFAULT_RING, NET_HDR};

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;

pub const VENDOR: u16 = 0x1AF4;
pub const DEV_TRANSITIONAL: u16 = 0x1000;
pub const DEV_MODERN: u16 = 0x1041;

static MATCHES: [pci::Match; 2] = [
    pci::Match::Id { vendor: VENDOR, device: DEV_TRANSITIONAL },
    pci::Match::Id { vendor: VENDOR, device: DEV_MODERN },
];

pci::pci_driver!(pci::DriverEntry { name: "virtio-net", matches: &MATCHES, opt_in_key: None, probe });

static DEVICES: SpinLock<Vec<Arc<VirtioNet>>> = SpinLock::named("virtio-net.devices", Vec::new());
static POLLER: ksync::Once<()> = ksync::Once::new();

/// True for a function this driver drives (used by tests to know whether a card should be there).
pub fn is_virtio_net(f: &pci::PciFunction) -> bool {
    f.vendor == VENDOR && (f.device == DEV_TRANSITIONAL || f.device == DEV_MODERN)
}

/// Every card this driver brought up, in probe order.
pub fn devices() -> Vec<Arc<VirtioNet>> {
    DEVICES.lock().clone()
}

fn probe(f: &pci::PciFunction) -> Result<(), String> {
    let function = format!("{}", f.addr);
    let caps = caps::parse(&f.capabilities(), |o| f.read8(o), |o| f.read32(o));
    let caps = match caps {
        Ok(c) => c,
        Err(e) => {
            klog::event!(Warn, "net", "virtio.unsupported", function = function.as_str(), device = klog::Hex(f.device as u64), reason = e);
            return Err(String::from(e));
        }
    };
    klog::event!(Info, "net", "virtio.probe", function = function.as_str(), device = klog::Hex(f.device as u64),
        common_bar = caps.common.bar, notify_mult = caps.notify_mult);
    let index = DEVICES.lock().len();
    let dev = match VirtioNet::probe(f, &caps, index) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            // Leave the function quiet: no MSI-X and no bus master for a card we do not drive (logged).
            let _ = f.disable_device_logged();
            klog::event!(Error, "net", "virtio.probe_failed", function = function.as_str(), error = e.as_str());
            return Err(e);
        }
    };
    devmodel::register_quiesce(dev.clone());
    net::netdev::register(dev.clone());
    DEVICES.lock().push(dev);
    POLLER.get_or_init(|| hal::vectors::register_poller("virtio-net", poll_all));
    Ok(())
}

/// The poller: housekeeping for every card. Skips a round instead of waiting if a probe holds the list, and
/// polls outside the list lock (a poll may restart a card, which takes up to two resets of spinning).
fn poll_all() {
    use devmodel::net::NetDevice;
    // By index, one card at a time: the idle loop calls this all the time and the G0 heap never reuses memory,
    // so a copy of the list per call would eat RAM (NW-3 found ~70 MB/s).
    let Some(n) = DEVICES.try_lock().map(|l| l.len()) else { return };
    for i in 0..n {
        if let Some(d) = DEVICES.try_lock().and_then(|l| l.get(i).cloned()) {
            d.poll();
        }
    }
}
