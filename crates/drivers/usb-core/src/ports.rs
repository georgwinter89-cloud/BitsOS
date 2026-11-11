//! The downstream ports of one hub, root or external: what changed, debounce, reset, enumeration with a second
//! try, and detach (V1 `xhciport.asm` and `hubport.asm` share this logic; Linux `hub_port_connect_change`).
//! The port access itself (PORTSC or hub requests) is behind `Ports`. A `PortSet` is used by one hotplug worker
//! only; it returns the devices to detach instead of detaching them, so its owner can drop its lock first.

use crate::device::Device;
use crate::enumerate::{enumerate, Scheme};
use crate::host::{Host, UsbError};
use crate::place::Place;
use crate::reset::{self, Ended, ResetKind};
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
/// A port given up on is tried again after this long, then twice as long each time (US-R: a device that stalled
/// for a while must not need a replug). Only a pause: there is no last try. Linux has no such retry (a replug is
/// its way), but its device stays on the bus; here it would have cost the boot disk for good.
pub const RETRY_FIRST_NS: u64 = 5_000_000_000;
pub const RETRY_MAX_NS: u64 = 300_000_000_000;

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
    /// A reset of a port that has a device on it (US-R; `reset` may only wait for a USB 3 link that is coming up
    /// anyway). `warm`: a warm reset where the port has that (SuperSpeed); returns the speed like `reset`.
    fn reset_device(&self, port: u8, _warm: bool) -> Result<Speed, UsbError> {
        self.reset(port)
    }
    /// Switches the port's power off and on again and returns once it is back. False: this port cannot.
    fn power_cycle(&self, _port: u8) -> bool {
        false
    }
    /// Switches a port off whose device failed every try, so its address-0 device cannot answer for the next
    /// one (hubs; root ports are left alone: PED is never written).
    fn disable(&self, port: u8);
    /// Undoes `disable` where a look at the port would not find the device otherwise (a SuperSpeed hub port).
    fn enable_again(&self, _port: u8) {}
    fn place(&self, port: u8, speed: Speed) -> Result<Place, UsbError>;
}

#[derive(Default)]
struct PortState {
    child: Option<Arc<Device>>,
    /// A connect seen at this time waits for its debounce.
    since: Option<u64>,
    /// Never looked at since the hub came (or the controller restarted): whatever is there is new.
    fresh: bool,
    /// The port was given up on: when it is tried again, and the pause that was (`RETRY_FIRST_NS`).
    retry: Option<(u64, u64)>,
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
        // US-R: a reset is due or the device can be reset in place: its port's trouble costs it that reset, and the
        // reset finds out whether the same device is still there (Linux `port_event` for SS.Inactive).
        if let Some(c) = p.child.as_ref().filter(|_| st.connected) {
            if c.reset_pending() {
                return None;
            }
            let bound = !crate::registry::bindings_of(c.id).is_empty();
            if dropped && !st.connect_change && bound && c.can_reset_in_place() && c.queue_reset(ResetKind::Recover, true).is_ok() {
                klog::event!(Warn, "usb", "port.disabled_under_device", hub = label.as_str(), port = port, recover = true;
                    "the port lost its enable with a device on it; resetting that device in place");
                return None;
            }
        }
        // A port in its pause (`RETRY_FIRST_NS`) that shows nothing: switched off by us, or the device left, which
        // the next try finds out. Only a device that shows up ends the pause early.
        if p.retry.is_some() && !st.connected {
            return None;
        }
        let old = p.child.take();
        p.since = st.connected.then_some(now);
        p.retry = None;
        if st.connected {
            klog::event!(Debug, "usb", "port.connect", hub = label.as_str(), port = port, raw = klog::Hex(st.raw as u64),
                fresh = fresh, replaced = old.is_some());
        } else if old.is_some() {
            klog::event!(Debug, "usb", "port.disconnect", hub = label.as_str(), port = port, raw = klog::Hex(st.raw as u64));
        }
        if dropped {
            klog::event!(Warn, "usb", "port.disabled_under_device", hub = label.as_str(), port = port, recover = false;
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
        self.run_resets(host, ports, now, &mut out);
        let mut next: Option<u64> = None;
        for port in 1..=self.ports.len() as u8 {
            self.look_again(ports, port, host.now_ns());
            let Some(since) = self.ports[port as usize - 1].since else { continue };
            let due = since.saturating_add(DEBOUNCE_NS);
            if host.now_ns() < due {
                next = Some(next.map_or(due, |n| n.min(due)));
                continue;
            }
            let p = &mut self.ports[port as usize - 1];
            p.since = None;
            let pause = p.retry.map_or(0, |(_, pause)| pause);
            p.retry = None;
            // Asked before: `attach` may switch the port off, and a port in SS.Disabled shows no connection.
            let there = matches!(ports.status(port), Ok(st) if st.connected);
            p.child = attach(host, ports, port);
            if p.child.is_none() && there {
                let pause = if pause == 0 { RETRY_FIRST_NS } else { pause.saturating_mul(2).min(RETRY_MAX_NS) };
                p.retry = Some((host.now_ns().saturating_add(pause), pause));
            }
        }
        (out, next)
    }

    /// Resets asked of the devices on these ports (`reset.rs`): the worker's turn is the one moment no change of
    /// these ports is being looked at. A device that has to go goes through the ordinary detach and its port is
    /// looked at afresh.
    fn run_resets(&mut self, host: &Arc<dyn Host>, ports: &dyn Ports, now: u64, out: &mut Vec<Arc<Device>>) {
        for port in 1..=self.ports.len() as u8 {
            let i = port as usize - 1;
            let Some(job) = self.ports[i].child.as_ref().and_then(|c| c.take_reset()) else { continue };
            let Some(c) = self.ports[i].child.clone() else { continue };
            match reset::run(host, ports, port, &c, &job) {
                Ended::Kept(r) => job.finish(r),
                Ended::Lost => {
                    self.ports[i].child = None;
                    self.ports[i].since = matches!(ports.status(port), Ok(st) if st.connected).then_some(now);
                    out.push(c);
                    job.finish(Err(UsbError::Gone));
                }
            }
        }
    }

    /// A port given up on: its pause is over, its device gets another turn through the debounce.
    fn look_again(&mut self, ports: &dyn Ports, port: u8, now: u64) {
        let p = &mut self.ports[port as usize - 1];
        if !p.retry.is_some_and(|(at, _)| now >= at) || p.since.is_some() || p.child.is_some() {
            return;
        }
        ports.enable_again(port);
        klog::event!(Info, "usb", "port.retry", hub = ports.label().as_str(), port = port;
            "the pause after giving this port up is over; trying its device again");
        p.since = Some(now);
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
        "the device failed every try; the port is tried again after a pause");
    ports.disable(port);
    None
}

fn enum_code(e: UsbError) -> u8 {
    match e {
        UsbError::Transfer(c) | UsbError::Command(c) => c,
        _ => 0,
    }
}
