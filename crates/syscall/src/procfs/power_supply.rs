//! GM-6p: `/sys/class/power_supply/<name>` for the batteries in `console::power_supply` (linux-7.3.1
//! drivers/power/supply/power_supply_sysfs.c:155: the attribute names, the value words, mode 0444, `uevent`). The
//! HID battery has hid-input's properties (`hidinput_battery_props`: present, online, capacity, model_name,
//! status, scope) plus `type`, which every power supply has. Linux's class entry is a link into the device's
//! directory; V2 has no USB device tree under /sys, so the entry is the directory itself, and `device` is a
//! directory holding only the HID device's `uevent` (hid-core.c `hid_uevent`: HID_ID, HID_NAME, HID_PHYS,
//! HID_UNIQ), the lines a reader of Linux's `device/uevent` finds. HID_PHYS is the evdev devices' phys
//! (`EVIOCGPHYS`), so a shell tells which pad a battery belongs to. Values are read at open; a battery that
//! went answers ENODEV.

use super::attr::{Live, PAGE};
use super::sys_net::uevent_store;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use console::power_supply::{self, Battery};
use uapi::errno::ENODEV;
use vfs::Errno;
use vfs::synth::{DirSource, SynthEntry};

/// Attributes in Linux's `power_supply_attrs` order.
const ATTRS: &[(&str, fn(&Battery) -> String)] = &[
    ("type", |_| String::from("Battery\n")),
    ("status", |b| format!("{}\n", b.snapshot().status)),
    ("present", |_| String::from("1\n")),
    ("online", |_| String::from("1\n")),
    ("capacity", |b| format!("{}\n", b.snapshot().capacity)),
    ("scope", |_| String::from("Device\n")),
    ("model_name", |b| format!("{}\n", b.model)),
];

/// `power_supply_uevent` after the device type: the name, the type, then the properties in hid-input's order.
fn uevent(b: &Battery) -> String {
    let s = b.snapshot();
    format!(
        "DEVTYPE=power_supply\nPOWER_SUPPLY_NAME={}\nPOWER_SUPPLY_TYPE=Battery\nPOWER_SUPPLY_PRESENT=1\nPOWER_SUPPLY_ONLINE=1\n\
         POWER_SUPPLY_CAPACITY={}\nPOWER_SUPPLY_MODEL_NAME={}\nPOWER_SUPPLY_STATUS={}\nPOWER_SUPPLY_SCOPE=Device\n",
        b.name, s.capacity, b.model, s.status
    )
}

/// hid-core.c `hid_uevent` without DRIVER and MODALIAS (V2 has no HID driver names or groups to give).
fn hid_uevent(b: &Battery) -> String {
    format!(
        "HID_ID=0003:{:08X}:{:08X}\nHID_NAME={}\nHID_PHYS={}\nHID_UNIQ=\n",
        b.vendor, b.product, b.model, b.phys
    )
}

fn attr(name: &str, f: fn(&Battery) -> String) -> Live {
    let n = String::from(name);
    Live::new(PAGE, move || power_supply::by_name(n.as_bytes()).map(|b| f(&b)).ok_or(ENODEV))
}

struct SupplyDir(String);

/// `<name>/device`: the HID device, its `uevent` only.
struct HidDir(String);

impl DirSource for HidDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let mut e = SynthEntry::file("uevent", 0o644, Arc::new(attr(&self.0, hid_uevent).writable(uevent_store)));
        e.cookie = 1;
        Ok(alloc::vec![e])
    }
}

impl DirSource for SupplyDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let mut v: Vec<SynthEntry> = ATTRS.iter().map(|(n, f)| SynthEntry::file(n, 0o444, Arc::new(attr(&self.0, *f)))).collect();
        v.push(SynthEntry::dir("device", Arc::new(HidDir(self.0.clone()))).with_perm(0o755));
        v.push(SynthEntry::link("subsystem", "../../../class/power_supply"));
        v.push(SynthEntry::file("uevent", 0o644, Arc::new(attr(&self.0, uevent).writable(uevent_store))));
        for (i, e) in v.iter_mut().enumerate() {
            e.cookie = i as u64 + 1;
        }
        Ok(v)
    }
}

fn entry(i: usize, b: &Battery) -> SynthEntry {
    SynthEntry::dir(&b.name, Arc::new(SupplyDir(b.name.clone()))).with_perm(0o755).cookie(i as u64 + 1)
}

/// `/sys/class/power_supply`: one directory per battery, as they come and go.
struct Class;

impl DirSource for Class {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok(power_supply::all().iter().enumerate().map(|(i, b)| entry(i, b)).collect())
    }
    fn lookup(&self, name: &[u8]) -> Result<Option<SynthEntry>, Errno> {
        Ok(power_supply::by_name(name).map(|b| entry(0, &b)))
    }
}

pub(super) fn class() -> Arc<dyn DirSource> {
    Arc::new(Class)
}
