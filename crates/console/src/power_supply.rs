//! GM-6p, L7-HW: the batteries of input devices, what `/sys/class/power_supply` shows
//! (linux-7.3.1/drivers/power/supply/power_supply_core.c and the properties of
//! linux-7.3.1/drivers/hid/hid-input.c:344 `hidinput_battery_props`). usb-hid registers one per report ID with a
//! battery usage (hid-input.c:542, several per HID interface) and updates it from the reports; syscall's sysfs
//! reads it. Kept here because both sides depend on the console crate already.
//!
//! Locks: `power.batteries` is a leaf (nothing is taken under it, nothing logs under it); the values are
//! atomics, so the controller's event thread updates them without a lock and without allocating.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
use ksync::SpinLock;

const DISCHARGING: u8 = 0;
const CHARGING: u8 = 1;

pub struct Battery {
    /// The directory name, `hid-<uniq or device name>-battery-<report id>` (hid-input.c:575).
    pub name: String,
    /// `model_name`: the HID device's name.
    pub model: String,
    /// The HID device's `HID_PHYS`, the same string as its evdev devices' phys (`EVIOCGPHYS`): how a reader
    /// tells which pad a battery belongs to.
    pub phys: String,
    pub vendor: u16,
    pub product: u16,
    /// Linux `bat->capacity`, 0 until the first value (V2 never queries, as Linux with `avoid_query`).
    capacity: AtomicI32,
    /// Linux `bat->status == HID_BATTERY_REPORTED`: a capacity came; the status is "Unknown" before.
    reported: AtomicBool,
    /// Linux `bat->charge_status`: Discharging until a Charging usage says otherwise.
    charge: AtomicU8,
}

/// What one battery shows at a read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub capacity: i32,
    /// `power_supply_status_text`: "Unknown", "Charging" or "Discharging".
    pub status: &'static str,
}

impl Battery {
    /// A new capacity in percent (already scaled and checked by hidparse). True when it changed what a read
    /// shows (the first value always does).
    pub fn set_capacity(&self, capacity: i32) -> bool {
        let old = self.capacity.swap(capacity, Ordering::AcqRel);
        let was = self.reported.swap(true, Ordering::AcqRel);
        old != capacity || !was
    }

    /// Linux `hidinput_update_battery_charge_status` (hid-input.c:651). True when it changed what a read shows.
    pub fn set_charging(&self, charging: bool) -> bool {
        let charge = if charging { CHARGING } else { DISCHARGING };
        let old = self.charge.swap(charge, Ordering::AcqRel);
        old != charge && self.reported.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> Snapshot {
        let status = match (self.reported.load(Ordering::Acquire), self.charge.load(Ordering::Acquire)) {
            (false, _) => "Unknown",
            (true, CHARGING) => "Charging",
            (true, _) => "Discharging",
        };
        Snapshot { capacity: self.capacity.load(Ordering::Acquire), status }
    }
}

static BATTERIES: SpinLock<Vec<Arc<Battery>>> = SpinLock::named("power.batteries", Vec::new());

/// Adds a battery (`power_supply_register`); a name already taken gets none (Linux's sysfs refuses the second
/// directory the same way).
pub fn register(name: String, model: String, phys: String, vendor: u16, product: u16) -> Option<Arc<Battery>> {
    let b = Arc::new(Battery {
        name,
        model,
        phys,
        vendor,
        product,
        capacity: AtomicI32::new(0), reported: AtomicBool::new(false), charge: AtomicU8::new(DISCHARGING),
    });
    let mut all = BATTERIES.lock();
    if all.iter().any(|o| o.name == b.name) {
        return None;
    }
    all.push(b.clone());
    Some(b)
}

/// Removes the battery (`power_supply_unregister`); its directory is gone from the next lookup on.
pub fn unregister(b: &Arc<Battery>) {
    BATTERIES.lock().retain(|o| !Arc::ptr_eq(o, b));
}

/// Every battery, in the order they came.
pub fn all() -> Vec<Arc<Battery>> {
    BATTERIES.lock().clone()
}

pub fn by_name(name: &[u8]) -> Option<Arc<Battery>> {
    BATTERIES.lock().iter().find(|b| b.name.as_bytes() == name).cloned()
}
