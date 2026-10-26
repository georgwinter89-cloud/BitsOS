//! The hotplug worker's logic (one worker thread per controller, class Normal; `driver.rs` runs the loop). Each
//! round: a controller that stopped loses its devices; a new bring-up (epoch) starts with every port fresh after
//! the ports' power-good time (PPC) and the late-device wait; port changes the event thread noted are looked at; debounced ports reset and
//! enumerated; halted pipes repaired; then every hub on this controller served (`usb_core::hub::service`).
//! V1 did this only when the kernel idled (`usb_service`); here it is its own thread, woken by events.

use crate::hc::{Controller, Env, State, MS};
use crate::port::RootPorts;
use crate::regs::portsc;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use usb_core::{registry, Host, PortSet};

/// Ports that showed a device when the driver took over get this long to show it again after the reset (a USB 2
/// PHY re-detects late, a USB 3 link trains late; V1 `xhci_boot_ports`).
pub const LATE_DEVICE_NS: u64 = 100 * MS;
/// With port power switching (PPC) the ports were just powered at start: their power-good time before the first
/// look (Linux `hub_power_on` waits at least 100 ms, root hubs included).
pub const POWER_GOOD_NS: u64 = 100 * MS;
const LATE_POLL_NS: u64 = 5 * MS;

pub struct Hotplug {
    root: Option<PortSet>,
    epoch: u32,
    up_at: u64,
}

impl Default for Hotplug {
    fn default() -> Self {
        Self::new()
    }
}

impl Hotplug {
    pub fn new() -> Self {
        Hotplug { root: None, epoch: 0, up_at: 0 }
    }

    fn detach_root(&mut self, c: &str) {
        if let Some(mut r) = self.root.take() {
            for d in r.take_children() {
                registry::detach(&d);
            }
        }
        registry::detach_host(c);
    }

    /// One round. Returns when the next round is due (a debounce ending); `None` = when woken.
    pub fn round<E: Env>(&mut self, c: &Arc<Controller<E>>) -> Option<u64> {
        let host: Arc<dyn Host> = c.clone();
        if c.state() != State::Running {
            if self.root.is_some() {
                klog::event!(Info, "usb", "xhci.devices_dropped", ctrl = c.name(), state = c.state().as_str());
                self.detach_root(c.name());
            }
            c.settled.store(true, Ordering::Release);
            return None;
        }
        let now = c.env().now_ns();
        if self.root.is_none() || c.epoch() != self.epoch {
            self.detach_root(c.name());
            self.epoch = c.epoch();
            self.up_at = now;
            self.root = Some(PortSet::new(c.caps().max_ports));
            c.take_port_changes();
            klog::event!(Debug, "usb", "xhci.port_scan", ctrl = c.name(), epoch = self.epoch);
        }
        let powered_at = self.up_at.saturating_add(if c.caps().ppc { POWER_GOOD_NS } else { 0 });
        if now < powered_at {
            c.settled.store(false, Ordering::Release);
            return Some(powered_at);
        }
        let late = now < self.up_at.saturating_add(LATE_DEVICE_NS)
            && c.ports_at_takeover().iter().any(|&p| c.rd(c.caps().portsc(p)) & portsc::CCS == 0);
        if late {
            c.settled.store(false, Ordering::Release);
            return Some(now.saturating_add(LATE_POLL_NS).min(self.up_at.saturating_add(LATE_DEVICE_NS)));
        }
        let ports = RootPorts { c: c.as_ref() };
        let root = self.root.as_mut()?;
        let mut gone: Vec<_> = c.take_port_changes().into_iter().filter_map(|p| root.check(&ports, p, now)).collect();
        let (more, next) = root.service(&host, &ports, now);
        gone.extend(more);
        let busy_root = root.busy();
        for d in gone {
            registry::detach(&d);
        }
        c.repair_pipes();
        let (hub_next, hub_busy) = usb_core::hub::service(&host);
        let busy = busy_root || hub_busy || c.has_port_changes();
        c.settled.store(!busy, Ordering::Release);
        match (next, hub_next) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}
