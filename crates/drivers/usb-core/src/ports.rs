//! The downstream ports of one hub, root or external: what changed, debounce, reset, enumeration with a second
//! try, and detach (V1 `xhciport.asm` and `hubport.asm` share this logic; Linux `hub_port_connect_change`).
//! The port access itself (PORTSC or hub requests) is behind `Ports`. A `PortSet` is used by one hotplug worker
//! only; it returns the devices to detach instead of detaching them, so its owner can drop its lock first.

use crate::device::Device;
use crate::enumerate::{enumerate, Scheme};
use crate::host::{Host, UsbError};
use crate::place::Place;
use crate::speed::Speed;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

klog::counter!(ENUM_FAILURES, "usb.enum_failures");
klog::counter!(PORTS_GIVEN_UP, "usb.ports_given_up");

/// From a connect to its reset (USB 2.0 TATTDB, V1 `XH_DEBOUNCE`).
pub const DEBOUNCE_NS: u64 = 100_000_000;
/// A device fresh from its reset may fail the first time (V1: each attach is tried twice).
pub const ATTACH_TRIES: usize = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortStatus {
    pub connected: bool,
    pub enabled: bool,
    pub powered: bool,
    pub over_current: bool,
    /// The speed the port reports (valid once enabled).
    pub speed: Option<Speed>,
    pub connect_change: bool,
    pub enable_change: bool,
    pub reset_change: bool,
    pub over_current_change: bool,
    /// Link state change, config error (SuperSpeed).
    pub link_change: bool,
    /// The register or wPortStatus/wPortChange as read, for logs and for clearing exactly what was seen.
    pub raw: u32,
}

pub trait Ports {
    /// "xhci0" or the hub's path, for logs.
    fn label(&self) -> String;
    fn count(&self) -> u8;
    /// False for a root port no supported-protocol capability covers (it is never touched).
    fn usable(&self, _port: u8) -> bool {
        true
    }
    fn status(&self, port: u8) -> Result<PortStatus, UsbError>;
    /// Clears the change bits `st` showed (RW1C for root ports, CLEAR_FEATURE for hub ports).
    fn clear_changes(&self, port: u8, st: &PortStatus);
    /// Resets the port (USB 2) or brings its link up (USB 3, warm reset if needed), waits the recovery time and
    /// returns the device's speed once the port is enabled.
    fn reset(&self, port: u8) -> Result<Speed, UsbError>;
    /// Switches a port off whose device failed every try, so its address-0 device cannot answer for the next
    /// one (hubs; root ports are left alone: PED is never written).
    fn disable(&self, port: u8);
    fn place(&self, port: u8, speed: Speed) -> Result<Place, UsbError>;
}

#[derive(Default)]
struct PortState {
    child: Option<Arc<Device>>,
    /// A connect seen at this time waits for its debounce.
    since: Option<u64>,
    /// Never looked at since the hub came (or the controller restarted): whatever is there is new.
    fresh: bool,
}

pub struct PortSet {
    ports: Vec<PortState>,
}

impl PortSet {
    /// `n` ports, all fresh.
    pub fn new(n: u8) -> PortSet {
        PortSet { ports: (0..n).map(|_| PortState { fresh: true, ..Default::default() }).collect() }
    }

    fn at(&mut self, port: u8) -> Option<&mut PortState> {
        self.ports.get_mut((port as usize).checked_sub(1)?)
    }

    pub fn child(&self, port: u8) -> Option<&Arc<Device>> {
        self.ports.get((port as usize).checked_sub(1)?)?.child.as_ref()
    }
    pub fn children(&self) -> impl Iterator<Item = (u8, &Arc<Device>)> {
        self.ports.iter().enumerate().filter_map(|(i, p)| p.child.as_ref().map(|c| (i as u8 + 1, c)))
    }
    pub fn take_children(&mut self) -> Vec<Arc<Device>> {
        self.ports.iter_mut().filter_map(|p| p.child.take()).collect()
    }
    /// A port waits for its debounce or was never looked at.
    pub fn busy(&self) -> bool {
        self.ports.iter().any(|p| p.since.is_some() || p.fresh)
    }

    /// A change was reported on `port`: changes cleared; a device gone (or another in its place, or its port
    /// disabled under it) is returned for detach; a connect waits for its debounce.
    pub fn check(&mut self, ports: &dyn Ports, port: u8, now: u64) -> Option<Arc<Device>> {
        if !ports.usable(port) {
            return None;
        }
        let label = ports.label();
        let st = match ports.status(port) {
            Ok(s) => s,
            Err(e) => {
                klog::event!(Warn, "usb", "port.status_failed", hub = label.as_str(), port = port, error = e.as_str());
                return None;
            }
        };
        ports.clear_changes(port, &st);
        if st.over_current_change || st.over_current {
            klog::event!(Warn, "usb", "port.over_current", hub = label.as_str(), port = port, active = st.over_current);
        }
        let p = self.at(port)?;
        let fresh = core::mem::replace(&mut p.fresh, false);
        let dropped = p.child.is_some() && st.connected && !st.enabled && st.enable_change;
        if !fresh && st.connected && !st.connect_change && !dropped {
            return None; // the same device: a change of another kind
        }
        let old = p.child.take();
        p.since = st.connected.then_some(now);
        if st.connected {
            klog::event!(Debug, "usb", "port.connect", hub = label.as_str(), port = port, raw = klog::Hex(st.raw as u64),
                fresh = fresh, replaced = old.is_some());
        } else if old.is_some() {
            klog::event!(Debug, "usb", "port.disconnect", hub = label.as_str(), port = port, raw = klog::Hex(st.raw as u64));
        }
        if dropped {
            klog::event!(Warn, "usb", "port.disabled_under_device", hub = label.as_str(), port = port;
                "the port lost its enable with a device on it; enumerating it afresh");
        }
        old
    }

    /// Looks at fresh ports, attaches the ones whose debounce is over. Returns devices to detach (a fresh look
    /// that found another device) and the time the next debounce ends.
    pub fn service(&mut self, host: &Arc<dyn Host>, ports: &dyn Ports, now: u64) -> (Vec<Arc<Device>>, Option<u64>) {
        let mut out = Vec::new();
        for port in 1..=self.ports.len() as u8 {
            if self.ports[port as usize - 1].fresh {
                if ports.usable(port) {
                    out.extend(self.check(ports, port, now));
                } else {
                    self.ports[port as usize - 1].fresh = false;
                }
            }
        }
        let mut next: Option<u64> = None;
        for port in 1..=self.ports.len() as u8 {
            let Some(since) = self.ports[port as usize - 1].since else { continue };
            let due = since.saturating_add(DEBOUNCE_NS);
            if host.now_ns() < due {
                next = Some(next.map_or(due, |n| n.min(due)));
                continue;
            }
            self.ports[port as usize - 1].since = None;
            self.ports[port as usize - 1].child = attach(host, ports, port);
        }
        (out, next)
    }
}

/// Reset and enumerate the debounced device on `port`, twice if needed; a port whose device fails every try is
/// switched off (V1 `xhci_port_attach`, `hub_port_attach`).
fn attach(host: &Arc<dyn Host>, ports: &dyn Ports, port: u8) -> Option<Arc<Device>> {
    let label = ports.label();
    let mut last = UsbError::Timeout;
    for try_no in 1..=ATTACH_TRIES {
        match ports.status(port) {
            Ok(st) if st.connected => {}
            Ok(_) => return None, // gone during the debounce
            Err(e) => {
                last = e;
                continue;
            }
        }
        let place = match ports.reset(port).and_then(|speed| ports.place(port, speed)) {
            Ok(p) => p,
            Err(e) => {
                klog::event!(Warn, "usb", "port.reset_failed", hub = label.as_str(), port = port, attempt = try_no, error = e.as_str());
                last = e;
                if e == UsbError::TooDeep {
                    break;
                }
                continue;
            }
        };
        // A retry holds SET_ADDRESS back until the descriptor was read at address 0 (Linux's xHCI order).
        let scheme = if try_no == 1 { Scheme::Direct } else { Scheme::Deferred };
        match enumerate(host, place, scheme) {
            Ok(d) => return Some(d),
            Err(e) => {
                ENUM_FAILURES.inc();
                klog::event!(Warn, "usb", "enum.failed", hub = label.as_str(), port = port, attempt = try_no,
                    speed = place.speed.as_str(), deferred = scheme == Scheme::Deferred, error = e.as_str(), code = enum_code(e));
                last = e;
            }
        }
    }
    PORTS_GIVEN_UP.inc();
    klog::event!(Error, "usb", "port.given_up", hub = label.as_str(), port = port, error = last.as_str(), code = enum_code(last);
        "the device failed every try; port left off until it is replugged");
    ports.disable(port);
    None
}

fn enum_code(e: UsbError) -> u8 {
    match e {
        UsbError::Transfer(c) | UsbError::Command(c) => c,
        _ => 0,
    }
}
