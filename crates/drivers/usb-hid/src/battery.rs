//! GM-6p, L7-HW: a HID interface's batteries (linux-7.3.1/drivers/hid/hid-input.c:542 `hidinput_setup_battery`,
//! :665 `hidinput_update_battery`, :535 `hidinput_cleanup_battery`): one `console::power_supply` battery per
//! report ID of the interface's descriptor that has a battery strength usage, updated from its input reports,
//! removed at the unplug.
//!
//! The name is Linux's `hid-<uniq>-battery-<report id>`, and as V2 reads no string descriptors (the DragonRise
//! trap) uniq is always empty, so Linux's fallback `dev_name` of the HID device: `BBBB:VVVV:PPPP.NNNN` with bus
//! 0003 (USB) and a running number per boot (Linux counts every HID device, V2 every interface with a battery:
//! unique all the same). All batteries of one interface share it, as all of one HID device do in Linux.

use crate::class;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use console::power_supply::{self, Battery};
use core::sync::atomic::{AtomicU32, Ordering};
use hidparse::{BatteryMap, ReportDescriptor};
use usb_core::Device;

/// Linux `BUS_USB`.
const BUS_USB: u16 = 0x0003;
static SEQ: AtomicU32 = AtomicU32::new(0);

pub struct PadBattery {
    /// One per report ID, in Linux's `dev->batteries` order.
    cells: Vec<(BatteryMap, Arc<Battery>)>,
    device: String,
    iface: u8,
}

/// The interface's batteries, None when its descriptor has none (Georg's wired DragonRise) or every name is taken.
pub fn probe(dev: &Device, iface: u8, desc: &ReportDescriptor) -> Option<PadBattery> {
    let maps = BatteryMap::find_all(desc);
    if maps.is_empty() {
        return None;
    }
    let seq = SEQ.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let model = class::device_name(dev.vendor(), dev.product(), 0, false);
    // The evdev phys of the interface's input devices (iface.rs register_inputs): the key to the pad.
    let phys = format!("{}/input{iface}", dev.path());
    let mut cells = Vec::new();
    for map in maps {
        let name = format!(
            "hid-{BUS_USB:04X}:{:04X}:{:04X}.{seq:04X}-battery-{}",
            dev.vendor(),
            dev.product(),
            map.report_id
        );
        let Some(ps) = power_supply::register(name.clone(), model.clone(), phys.clone(), dev.vendor(), dev.product())
        else {
            klog::event!(Warn, "input", "hid.battery_refused", device = dev.path(), interface = iface, name = name.as_str();
                "a power supply of that name exists already");
            continue;
        };
        klog::event!(Info, "input", "hid.battery_added", device = dev.path(), interface = iface, name = name.as_str(),
            report = map.report_id, min = map.min, max = map.max, values = map.strength.len(),
            charging = map.charging.len());
        cells.push((map, ps));
    }
    if cells.is_empty() {
        return None;
    }
    Some(PadBattery { cells, device: dev.path(), iface })
}

impl PadBattery {
    /// The directory names, in report order.
    pub fn names(&self) -> Vec<&str> {
        self.cells.iter().map(|(_, ps)| ps.name.as_str()).collect()
    }

    /// One input report (the controller's event thread: atomics only; an event only when a read changes).
    pub fn on_report(&self, report: &[u8]) {
        for (map, ps) in &self.cells {
            let r = map.read(report);
            let charge = r.charging.is_some_and(|c| ps.set_charging(c));
            let cap = r.capacity.is_some_and(|c| ps.set_capacity(c));
            if charge || cap {
                let s = ps.snapshot();
                klog::event!(Debug, "input", "hid.battery_changed", device = self.device.as_str(), interface = self.iface,
                    name = ps.name.as_str(), capacity = s.capacity, status = s.status);
            }
        }
    }

    /// The unplug: the directories go.
    pub fn remove(&self) {
        for (_, ps) in &self.cells {
            power_supply::unregister(ps);
            klog::event!(Info, "input", "hid.battery_removed", device = self.device.as_str(), interface = self.iface,
                name = ps.name.as_str());
        }
    }
}
