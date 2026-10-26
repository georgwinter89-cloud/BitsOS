//! The hub class driver (USB 2.0 §11, USB 3.2 §10; V1 `hub.asm`, `hubport.asm`; Linux `drivers/usb/core/hub.c`):
//! hub descriptor, SET_HUB_DEPTH for SuperSpeed hubs, one TT per port when the hub offers it (alternate setting 1),
//! the slot made a hub's, every port powered and given its power-good time, the status change endpoint polled.
//! Its ports are a `PortSet` served by the hotplug worker of the hub's controller (`service`).

use crate::desc::{EpKind, HubDescriptor, Interface};
use crate::device::Device;
use crate::host::{Host, HubParams, PipeEvent, UsbError};
use crate::place::{HubAt, Place, PlaceError, MAX_DEPTH};
use crate::ports::{PortSet, PortStatus, Ports};
use crate::registry::{self, UsbDriver, UsbMatch};
use crate::setup::{feature, Setup, LINK_SS_DISABLED};
use crate::speed::Speed;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::{Mutex, SpinLock};

const MS: u64 = 1_000_000;
/// A hub port reset is done (C_PORT_RESET) within this time (V1 `XH_PORT_MS`).
pub const PORT_RESET_NS: u64 = 500 * MS;
const RESET_POLL_NS: u64 = 10 * MS;
/// After a hub port reset, before the address (10 ms TRSTRCY plus Linux's margin; V1 `XH_RECOVERY`).
pub const RECOVERY_NS: u64 = 50 * MS;
/// Power-good wait at least this long, whatever bPwrOn2PwrGood says (Linux `hub_power_on`).
pub const POWER_MIN_NS: u64 = 100 * MS;
/// With its status pipe dead, a hub's ports are looked at this often.
pub const FALLBACK_POLL_NS: u64 = 1000 * MS;

klog::counter!(HUBS_BOUND, "usb.hubs_bound");
klog::counter!(PIPE_ERRORS, "usb.hub_pipe_errors");

static MATCHES: [UsbMatch; 1] = [UsbMatch::Interface { class: 9, subclass: None, protocol: None }];
pub static DRIVER: UsbDriver = UsbDriver { name: "hub", matches: &MATCHES, probe, disconnect };

pub struct Hub {
    pub dev: Arc<Device>,
    pub desc: HubDescriptor,
    pub multi_tt: bool,
    ports: Mutex<PortSet>,
    /// Ports (0 = the hub itself) its status pipe reported, not yet looked at.
    changes: SpinLock<Vec<u8>>,
    alive: AtomicBool,
    pipe_dead: AtomicBool,
    last_poll: AtomicU64,
}

static HUBS: SpinLock<Vec<Arc<Hub>>> = SpinLock::named("usb.hubs", Vec::new());

/// Every bound hub (tools, ktests).
pub fn hubs() -> Vec<Arc<Hub>> {
    HUBS.lock().clone()
}

impl Hub {
    pub fn ports(&self) -> u8 {
        self.desc.ports
    }
    pub fn child(&self, port: u8) -> Option<Arc<Device>> {
        self.ports.lock().child(port).cloned()
    }
    pub(crate) fn take_children(&self) -> Vec<Arc<Device>> {
        self.ports.lock().take_children()
    }
    fn note(&self, report: &[u8]) {
        let mut c = self.changes.lock();
        for bit in 0..=self.desc.ports as usize {
            if report.get(bit / 8).is_some_and(|b| b & (1 << (bit % 8)) != 0) && !c.contains(&(bit as u8)) {
                c.push(bit as u8);
            }
        }
    }
    fn at(&self) -> HubAt {
        HubAt { id: self.dev.id, place: self.dev.place, slot: self.dev.slot, multi_tt: self.multi_tt }
    }
}

fn probe(dev: &Arc<Device>, f: &Interface) -> Result<(), String> {
    let e = |what: &str, err: UsbError| alloc::format!("{what}: {}", err.as_str());
    if dev.place.depth >= MAX_DEPTH {
        return Err("hub nested too deep: its ports would need a sixth route string tier".into());
    }
    let ss = dev.speed().is_superspeed();
    let mut b = [0u8; 71];
    let len = if ss { 12 } else { b.len() as u16 };
    let n = dev.control_in(Setup::hub_descriptor(ss, len), &mut b[..len as usize]).map_err(|x| e("hub descriptor", x))?;
    let desc = HubDescriptor::parse(&b[..n], ss).map_err(|_| String::from("hub descriptor short or of the wrong type"))?;
    if desc.ports == 0 {
        return Err("hub with no ports".into());
    }
    if ss {
        dev.control_out(Setup::set_hub_depth(dev.place.depth), &[]).map_err(|x| e("SET_HUB_DEPTH", x))?;
    }
    let mtt_alt = dev.config.as_ref().and_then(|c| c.interface(f.number, 1)).is_some_and(|a| a.protocol == 2);
    let multi_tt = dev.speed() == Speed::High
        && dev.desc.protocol == 2
        && mtt_alt
        && dev.control_out(Setup::set_interface(f.number, 1), &[]).is_ok();
    let think = if dev.speed() == Speed::High { desc.tt_think_time() } else { 0 };
    dev.host().set_hub(dev.slot, HubParams { ports: desc.ports, tt_think_time: think, multi_tt }).map_err(|x| e("hub slot", x))?;
    for p in 1..=desc.ports {
        if let Err(x) = dev.control_out(Setup::set_port_feature(p, feature::PORT_POWER, 0), &[]) {
            klog::event!(Warn, "usb", "hub.power_failed", hub = dev.path(), port = p, error = x.as_str());
        }
    }
    dev.host().sleep_ns((desc.power_on_ms as u64 * MS).max(POWER_MIN_NS));
    let ep = f
        .endpoints
        .iter()
        .find(|ep| ep.is_in() && ep.kind() == EpKind::Interrupt)
        .ok_or_else(|| String::from("hub without a status change endpoint"))?;
    let hub = Arc::new(Hub {
        dev: dev.clone(),
        desc,
        multi_tt,
        ports: Mutex::named("usb.hub_ports", PortSet::new(desc.ports)),
        changes: SpinLock::named("usb.hub_changes", Vec::new()),
        alive: AtomicBool::new(true),
        pipe_dead: AtomicBool::new(false),
        last_poll: AtomicU64::new(dev.host().now_ns()),
    });
    HUBS.lock().push(hub.clone());
    let weak: Weak<Hub> = Arc::downgrade(&hub);
    let host = dev.host().clone();
    let sink = Arc::new(move |ev: PipeEvent<'_>| {
        let Some(h) = weak.upgrade() else { return };
        match ev {
            PipeEvent::Data(r) => h.note(r),
            PipeEvent::Error(_) => PIPE_ERRORS.inc(),
            PipeEvent::Stopped(_) => h.pipe_dead.store(true, Ordering::Release),
        }
        host.kick();
    });
    // One packet per transfer (Linux `hub_configure`): a longer one would wait for a second packet.
    let len = (ep.max_packet() as usize).max(1);
    if let Err(x) = dev.interrupt_in(ep.address, len, sink) {
        remove(dev.id);
        return Err(e("status change endpoint", x));
    }
    HUBS_BOUND.inc();
    klog::event!(Info, "usb", "hub.bound", hub = dev.path(), ports = desc.ports, superspeed = ss, depth = dev.place.depth,
        multi_tt = multi_tt, tt_think = think, power_on_ms = desc.power_on_ms);
    dev.host().kick();
    Ok(())
}

fn disconnect(dev: &Arc<Device>, _interface: u8) {
    remove(dev.id);
}

/// Takes a hub out of service (its device is detached); its children are still in its port set.
pub(crate) fn remove(id: u64) -> Option<Arc<Hub>> {
    let mut l = HUBS.lock();
    let i = l.iter().position(|h| h.dev.id == id)?;
    let h = l.remove(i);
    h.alive.store(false, Ordering::Release);
    Some(h)
}

/// Serves every hub on `host` (called by its hotplug worker): reported changes, fresh ports, debounces,
/// enumeration. Returns when the next debounce ends and whether any hub still has work.
pub fn service(host: &Arc<dyn Host>) -> (Option<u64>, bool) {
    let list: Vec<Arc<Hub>> = HUBS.lock().iter().filter(|h| h.dev.host_name() == host.name()).cloned().collect();
    let mut next: Option<u64> = None;
    let mut busy = false;
    for h in list {
        if !h.alive.load(Ordering::Acquire) {
            continue;
        }
        let now = host.now_ns();
        if h.pipe_dead.load(Ordering::Acquire) && now.saturating_sub(h.last_poll.load(Ordering::Relaxed)) >= FALLBACK_POLL_NS {
            h.last_poll.store(now, Ordering::Relaxed);
            let mut c = h.changes.lock();
            c.clear();
            c.extend(0..=h.desc.ports);
        }
        let changes = core::mem::take(&mut *h.changes.lock());
        let mut detach = Vec::new();
        {
            let mut ps = h.ports.lock();
            let hp = HubPorts { hub: &h };
            for &p in &changes {
                if p == 0 {
                    own_status(&h);
                } else if p <= h.desc.ports {
                    detach.extend(ps.check(&hp, p, now));
                }
            }
            let (d, n) = ps.service(host, &hp, now);
            detach.extend(d);
            if let Some(n) = n {
                next = Some(next.map_or(n, |x| x.min(n)));
            }
            busy |= ps.busy();
        }
        if h.pipe_dead.load(Ordering::Acquire) {
            let due = h.last_poll.load(Ordering::Relaxed).saturating_add(FALLBACK_POLL_NS);
            next = Some(next.map_or(due, |x| x.min(due)));
        }
        busy |= !h.changes.lock().is_empty();
        for d in detach {
            registry::detach(&d);
        }
    }
    (next, busy)
}

/// The hub's own change: local power or over-current, cleared and logged.
fn own_status(h: &Hub) {
    let mut b = [0u8; 4];
    if h.dev.control_in(Setup::hub_status(), &mut b).ok() != Some(4) {
        return;
    }
    let (status, change) = (u16::from_le_bytes([b[0], b[1]]), u16::from_le_bytes([b[2], b[3]]));
    for (bit, feat) in [(0, feature::C_HUB_LOCAL_POWER), (1, feature::C_HUB_OVER_CURRENT)] {
        if change & (1 << bit) != 0 {
            let _ = h.dev.control_out(Setup::clear_hub_feature(feat), &[]);
        }
    }
    if change & 2 != 0 {
        klog::event!(Warn, "usb", "hub.over_current", hub = h.dev.path(), active = status & 2 != 0);
    }
}

struct HubPorts<'a> {
    hub: &'a Hub,
}

impl HubPorts<'_> {
    fn ss(&self) -> bool {
        self.hub.desc.superspeed
    }
    fn send(&self, s: Setup) -> Result<usize, UsbError> {
        self.hub.dev.control_out(s, &[])
    }
}

/// wPortStatus / wPortChange (USB 2.0 Table 11-21/11-22, USB 3.2 Table 10-13/10-14) to a `PortStatus`.
pub fn port_status(raw: u32, superspeed: bool) -> PortStatus {
    let (s, c) = (raw & 0xFFFF, raw >> 16);
    let mut st = PortStatus {
        connected: s & 1 != 0,
        enabled: s & 2 != 0,
        over_current: s & 8 != 0,
        connect_change: c & 1 != 0,
        over_current_change: c & 8 != 0,
        raw,
        ..Default::default()
    };
    if superspeed {
        st.powered = s & (1 << 9) != 0;
        st.speed = Some(Speed::Super);
        st.reset_change = c & 0x30 != 0;
        st.link_change = c & 0xC0 != 0;
    } else {
        st.powered = s & (1 << 8) != 0;
        st.speed = Some(if s & (1 << 9) != 0 { Speed::Low } else if s & (1 << 10) != 0 { Speed::High } else { Speed::Full });
        st.enable_change = c & 2 != 0;
        st.reset_change = c & 0x10 != 0;
        st.link_change = c & 4 != 0;
    }
    st
}

/// The CLEAR_FEATURE selectors for the change bits set in `raw` (bit n of wPortChange).
pub fn change_features(raw: u32, superspeed: bool) -> Vec<u16> {
    let c = raw >> 16;
    let map: &[(u32, u16)] = if superspeed {
        &[(0, 16), (3, 19), (4, 20), (5, 29), (6, 25), (7, 26)]
    } else {
        &[(0, 16), (1, 17), (2, 18), (3, 19), (4, 20)]
    };
    map.iter().filter(|(bit, _)| c & (1 << bit) != 0).map(|&(_, f)| f).collect()
}

impl Ports for HubPorts<'_> {
    fn label(&self) -> String {
        self.hub.dev.path()
    }
    fn count(&self) -> u8 {
        self.hub.desc.ports
    }
    fn status(&self, port: u8) -> Result<PortStatus, UsbError> {
        let mut b = [0u8; 4];
        if self.hub.dev.control_in(Setup::port_status(port), &mut b)? < 4 {
            return Err(UsbError::Descriptor);
        }
        Ok(port_status(u32::from_le_bytes(b), self.ss()))
    }
    fn clear_changes(&self, port: u8, st: &PortStatus) {
        for f in change_features(st.raw, self.ss()) {
            let _ = self.send(Setup::clear_port_feature(port, f));
        }
    }
    fn reset(&self, port: u8) -> Result<Speed, UsbError> {
        // A SuperSpeed link stuck in SS.Inactive (6) or Compliance (10) needs a warm reset (Linux `hub_port_reset`).
        let link = (self.status(port)?.raw >> 5) & 0xF;
        let warm = self.ss() && (link == 6 || link == 10);
        self.send(Setup::set_port_feature(port, if warm { feature::BH_PORT_RESET } else { feature::PORT_RESET }, 0))?;
        let host = self.hub.dev.host();
        let deadline = host.now_ns().saturating_add(PORT_RESET_NS);
        let st = loop {
            host.sleep_ns(RESET_POLL_NS);
            let st = self.status(port)?;
            if st.reset_change {
                break st;
            }
            if !st.connected {
                return Err(UsbError::Gone);
            }
            if host.now_ns() >= deadline {
                return Err(UsbError::Timeout);
            }
        };
        // C_PORT_RESET, and C_PORT_ENABLE which some hubs add (V1 `hub_clear`).
        self.clear_changes(port, &st);
        if !st.enabled {
            return Err(UsbError::Invalid("port not enabled after its reset"));
        }
        host.sleep_ns(RECOVERY_NS);
        st.speed.ok_or(UsbError::Invalid("no speed"))
    }
    fn disable(&self, port: u8) {
        let r = if self.ss() {
            self.send(Setup::set_port_feature(port, feature::PORT_LINK_STATE, LINK_SS_DISABLED as u8))
        } else {
            self.send(Setup::clear_port_feature(port, feature::PORT_ENABLE))
        };
        klog::event!(Warn, "usb", "hub.port_off", hub = self.hub.dev.path(), port = port, ok = r.is_ok());
    }
    fn place(&self, port: u8, speed: Speed) -> Result<Place, UsbError> {
        Place::child(&self.hub.at(), port, speed).map_err(|e| match e {
            PlaceError::TooDeep => UsbError::TooDeep,
            PlaceError::BadPort => UsbError::Invalid("port 0"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usb2_port_status_and_changes() {
        // Connected, enabled, powered, high speed; C_CONNECTION and C_RESET.
        let st = port_status(0x0013_0503, false);
        assert!(st.connected && st.enabled && st.powered && st.connect_change && st.reset_change && st.enable_change);
        assert_eq!(st.speed, Some(Speed::High));
        assert_eq!(port_status(0x0000_0303, false).speed, Some(Speed::Low));
        assert_eq!(port_status(0x0000_0103, false).speed, Some(Speed::Full));
        assert_eq!(change_features(0x0013_0000, false), alloc::vec![16, 17, 20]);
        assert!(change_features(0x0000_0503, false).is_empty());
    }

    #[test]
    fn superspeed_port_status_and_changes() {
        // Connected, enabled, power at bit 9; C_CONNECTION, C_BH_PORT_RESET, C_PORT_LINK_STATE, C_CONFIG_ERROR.
        let st = port_status(0x00E1_0203, true);
        assert!(st.connected && st.enabled && st.powered && st.connect_change && st.reset_change && st.link_change);
        assert!(!st.enable_change, "no C_PORT_ENABLE on SuperSpeed hubs");
        assert_eq!(st.speed, Some(Speed::Super));
        assert_eq!(change_features(0x00E1_0000, true), alloc::vec![16, 29, 25, 26]);
        // Bit 1 of wPortChange is reserved on SuperSpeed hubs: never cleared as C_PORT_ENABLE.
        assert!(change_features(0x0002_0000, true).is_empty());
    }
}
