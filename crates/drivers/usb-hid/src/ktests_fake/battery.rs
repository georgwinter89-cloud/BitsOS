//! GM-6p, L7-HW: a fake wireless pad with a battery strength usage: its `/sys/class/power_supply` directory
//! (`hid-<dev>-battery-<report id>`, linux-7.3.1/drivers/hid/hid-input.c:575) appears at the plug-in with Linux's
//! attributes, the capacity and status follow the reports (scaled from the logical range, out-of-range values
//! dropped, 0 kept as 0 %), and the directory goes at the unplug. Charging counts only in the report of its
//! battery. Georg's wired DragonRise has no battery usage and gets no directory. A battery-only pad (State Of
//! Charge, nothing else) is polled and its percentage counts as 0..100; two identical pads get two directories
//! whose `device/uevent` HID_PHYS names the pad's evdev phys, and unplugging one leaves the other; a pad with
//! batteries in two reports gets two directories.

use super::{DRAGONRISE, Fake, Shape, VENDOR, with_device, with_device_at};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use console::power_supply;
use ktest::{TestResult, kassert, ktest};
use usb_core::Speed;

/// A gamepad: report 1 with X, Y, 8 buttons and Battery Strength 0..200 (hidparse tests/battery.rs PAD);
/// report 2 with Battery System Charging and padding.
const BATTERY_PAD: &[u8] = &[0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xff,
    0x00, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02, 0x05, 0x09, 0x19, 0x01, 0x29, 0x08, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95,
    0x08, 0x81, 0x02, 0x05, 0x06, 0x09, 0x20, 0x15, 0x00, 0x26, 0xc8, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, 0x85, 0x02,
    0x05, 0x85, 0x09, 0x44, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x75, 0x07, 0x95, 0x01, 0x81, 0x03,
    0xc0];

/// Report 3: X, Battery Strength 0..100 and Charging together (hidparse tests/battery.rs); report 4: State Of
/// Charge 0..255 (a second battery).
const TWO_BATTERIES: &[u8] = &[0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x03, 0x09, 0x30, 0x15, 0x00, 0x26, 0xff, 0x00,
    0x75, 0x08, 0x95, 0x01, 0x81, 0x02, 0x05, 0x06, 0x09, 0x20, 0x15, 0x00, 0x25, 0x64, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02,
    0x05, 0x85, 0x09, 0x44, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x75, 0x07, 0x95, 0x01, 0x81, 0x03,
    0x85, 0x04, 0x09, 0x65, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, 0xc0];

/// A gamepad application with only Absolute State Of Charge 0..255 (a percentage: Linux counts 0..100).
const CHARGE_ONLY: &[u8] = &[0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x05, 0x85, 0x09, 0x65, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75,
    0x08, 0x95, 0x01, 0x81, 0x02, 0xc0];

const BATTERY_SHAPE: Shape = Shape { vendor: VENDOR, product: 0x00B1, subclass: 0, protocol: 0, mps0: 64, mps: 8, interval: 4, out: false };

fn read(path: &str) -> Result<String, String> {
    let b = vfs::root_ns().read_file(path, 4096).map_err(|e| format!("{path}: errno {e}"))?;
    Ok(String::from_utf8_lossy(&b).into_owned())
}

fn want(dir: &str, attr: &str, value: &str) -> TestResult {
    let got = read(&format!("{dir}/{attr}"))?;
    kassert!(got == format!("{value}\n"), "{attr} = {got:?}, want {value:?}");
    Ok(())
}

fn listed(name: &str) -> Result<bool, String> {
    let all = vfs::root_ns().list_dir("/sys/class/power_supply").map_err(|e| format!("list: errno {e}"))?;
    Ok(all.iter().any(|(n, _)| n == name.as_bytes()))
}

/// The bound interface's battery directories in report order, their HID_PHYS checked against the pad's evdev
/// phys.
fn nodes(h: &Option<alloc::sync::Arc<crate::iface::HidIface>>) -> Result<Vec<String>, String> {
    let h = h.as_ref().ok_or("battery pad not bound")?;
    let b = h.battery.as_ref().ok_or("no battery on a pad with a battery usage")?;
    let phys = format!("{}/input{}", h.dev.path(), h.iface);
    let mut out = Vec::new();
    for name in b.names() {
        kassert!(listed(name)?, "{name} not listed");
        let hid = read(&format!("/sys/class/power_supply/{name}/device/uevent"))?;
        let want = format!("HID_ID=0003:0000B175:000000B1\nHID_NAME=USB HID b175:00b1\nHID_PHYS={phys}\nHID_UNIQ=\n");
        kassert!(hid == want, "device/uevent {hid:?}, want {want:?}");
        out.push(format!("/sys/class/power_supply/{name}"));
    }
    for d in h.inputs.iter().flatten() {
        kassert!(d.info.phys == phys, "evdev phys {:?} is not the battery's {phys:?}", d.info.phys);
    }
    Ok(out)
}

/// The one battery directory of a pad with one battery report.
fn node(h: &Option<alloc::sync::Arc<crate::iface::HidIface>>) -> Result<String, String> {
    let mut all = nodes(h)?;
    kassert!(all.len() == 1, "batteries {all:?}, want one");
    Ok(all.remove(0))
}

fn battery() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    let pad = Fake::shaped("battery", BATTERY_SHAPE, BATTERY_PAD, 0);
    let mut dir = String::new();
    let mut name = String::new();
    with_device(&pad, 212, |_, h| {
        dir = node(&h)?;
        name = String::from(dir.trim_start_matches("/sys/class/power_supply/"));
        kassert!(name.starts_with("hid-0003:B175:00B1.") && name.ends_with("-battery-1"), "name {name:?}");
        let link = vfs::root_ns().read_link(&format!("{dir}/subsystem")).map_err(|e| format!("subsystem: errno {e}"))?;
        kassert!(link == b"../../../class/power_supply", "subsystem -> {:?}", String::from_utf8_lossy(&link));
        want(&dir, "type", "Battery")?;
        want(&dir, "scope", "Device")?;
        want(&dir, "present", "1")?;
        want(&dir, "online", "1")?;
        want(&dir, "model_name", "USB HID b175:00b1")?;
        // No value yet: Linux's status before the first report (V2 never queries the device).
        want(&dir, "status", "Unknown")?;
        pad.feed(&[1, 0x80, 0x80, 0, 100]);
        want(&dir, "capacity", "50")?;
        want(&dir, "status", "Discharging")?;
        pad.feed(&[1, 0x80, 0x80, 0, 160]);
        want(&dir, "capacity", "80")?;
        // Values above the logical maximum are dropped; 0 is a real 0 % (Linux 7, hid-input.c:681).
        pad.feed(&[1, 0x80, 0x80, 0, 201]);
        want(&dir, "capacity", "80")?;
        pad.feed(&[1, 0x80, 0x80, 0, 0]);
        want(&dir, "capacity", "0")?;
        // Charging comes in report 2, which has no battery: dropped (hidinput_find_battery finds none).
        pad.feed(&[2, 1]);
        want(&dir, "status", "Discharging")?;
        let ev = read(&format!("{dir}/uevent"))?;
        let want_ev = format!(
            "DEVTYPE=power_supply\nPOWER_SUPPLY_NAME={name}\nPOWER_SUPPLY_TYPE=Battery\nPOWER_SUPPLY_PRESENT=1\n\
             POWER_SUPPLY_ONLINE=1\nPOWER_SUPPLY_CAPACITY=0\nPOWER_SUPPLY_MODEL_NAME=USB HID b175:00b1\n\
             POWER_SUPPLY_STATUS=Discharging\nPOWER_SUPPLY_SCOPE=Device\n"
        );
        kassert!(ev == want_ev, "uevent {ev:?}, want {want_ev:?}");
        Ok(())
    })?;
    kassert!(!listed(&name)?, "{name} still listed after the unplug");
    kassert!(vfs::root_ns().read_file(&format!("{dir}/capacity"), 64).is_err(), "{dir} left after the unplug");
    kassert!(power_supply::all().iter().all(|b| !dir.ends_with(b.name.as_str())), "battery left in the registry");

    // Georg's wired DragonRise: no battery usage, no directory.
    let before = power_supply::all().len();
    let shape = Shape { vendor: 0x0079, product: 0x0006, subclass: 0, protocol: 0, mps0: 8, mps: 8, interval: 10, out: true };
    let wired = Fake::shaped("dragonrise-nobat", shape, DRAGONRISE, 0);
    with_device_at(&wired, 213, Speed::Low, |_, h| {
        let h = h.ok_or("DragonRise not bound")?;
        kassert!(h.battery.is_none() && power_supply::all().len() == before, "a battery on the wired DragonRise");
        Ok(())
    })
}
ktest!("in.battery.fake_pad", QemuOnly, battery);

/// A pad whose only input is State Of Charge: bound and polled (no event comes from it), the range counts as
/// 0..100 whatever the descriptor says, values above 100 are dropped.
fn charge_only() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    let pad = Fake::shaped("battery-only", BATTERY_SHAPE, CHARGE_ONLY, 0);
    let mut name = String::new();
    with_device(&pad, 214, |_, h| {
        let dir = node(&h)?;
        name = String::from(dir.trim_start_matches("/sys/class/power_supply/"));
        kassert!(dir.ends_with("-battery-0"), "unnumbered reports: battery {dir:?}, want report ID 0");
        want(&dir, "status", "Unknown")?;
        pad.feed(&[80]);
        want(&dir, "capacity", "80")?;
        want(&dir, "status", "Discharging")?;
        pad.feed(&[200]);
        want(&dir, "capacity", "80")?;
        pad.feed(&[35]);
        want(&dir, "capacity", "35")
    })?;
    kassert!(!listed(&name)?, "{name} still listed after the unplug");
    Ok(())
}
ktest!("in.battery.charge_only", QemuOnly, charge_only);

/// Batteries in reports 3 and 4: two directories, `-battery-3` with its charging status and `-battery-4` with the
/// State Of Charge, each fed only by its own report; both go at the unplug.
fn two_reports() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    let pad = Fake::shaped("battery-two", BATTERY_SHAPE, TWO_BATTERIES, 0);
    let mut dirs = Vec::new();
    with_device(&pad, 217, |_, h| {
        dirs = nodes(&h)?;
        kassert!(dirs.len() == 2 && dirs[0].ends_with("-battery-3") && dirs[1].ends_with("-battery-4"), "dirs {dirs:?}");
        kassert!(
            dirs[0].trim_end_matches("-3") == dirs[1].trim_end_matches("-4"),
            "two device names in {dirs:?}"
        );
        pad.feed(&[3, 0x80, 64, 1]);
        want(&dirs[0], "capacity", "64")?;
        want(&dirs[0], "status", "Charging")?;
        want(&dirs[1], "status", "Unknown")?;
        pad.feed(&[4, 30]);
        want(&dirs[1], "capacity", "30")?;
        want(&dirs[1], "status", "Discharging")?;
        want(&dirs[0], "capacity", "64")?;
        pad.feed(&[3, 0x80, 0, 0]);
        want(&dirs[0], "capacity", "0")?;
        want(&dirs[0], "status", "Discharging")
    })?;
    for d in &dirs {
        kassert!(!listed(d.trim_start_matches("/sys/class/power_supply/"))?, "{d} still listed after the unplug");
    }
    Ok(())
}
ktest!("in.battery.two_reports", QemuOnly, two_reports);

/// Two identical wireless pads: two directories, each with its own pad's phys; unplugging one leaves the other
/// with its value.
fn two_pads() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    let a = Fake::shaped("battery-a", BATTERY_SHAPE, BATTERY_PAD, 0);
    let b = Fake::shaped("battery-b", BATTERY_SHAPE, BATTERY_PAD, 0);
    with_device(&a, 215, |_, ha| {
        let da = node(&ha)?;
        let mut db = String::new();
        with_device(&b, 216, |_, hb| {
            db = node(&hb)?;
            kassert!(da != db, "both pads got {da}");
            a.feed(&[1, 0x80, 0x80, 0, 40]);
            b.feed(&[1, 0x80, 0x80, 0, 120]);
            want(&da, "capacity", "20")?;
            want(&db, "capacity", "60")
        })?;
        kassert!(!listed(db.trim_start_matches("/sys/class/power_supply/"))?, "{db} still listed after its unplug");
        kassert!(listed(da.trim_start_matches("/sys/class/power_supply/"))?, "{da} went with the other pad");
        want(&da, "capacity", "20")
    })
}
ktest!("in.battery.two_pads", QemuOnly, two_pads);
