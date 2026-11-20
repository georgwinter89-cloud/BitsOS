//! GM-6p, L7-HW: battery values as linux-7.3.1/drivers/hid/hid-input.c:542-698 reads them (one battery per
//! report ID, setup, scaling, drops, charging).

mod common;

use hidparse::battery::Reading;
use hidparse::{BatteryMap, ReportDescriptor, parse};

fn desc(s: &str) -> ReportDescriptor {
    parse(&common::json::hex(&s.split_whitespace().collect::<String>())).unwrap()
}

/// A gamepad: report 1 with X, Y, 8 buttons and Battery Strength 0..200; report 2 with Charging and padding.
const PAD: &str = "05 01 09 05 a1 01 85 01 09 30 09 31 15 00 26 ff 00 75 08 95 02 81 02
    05 09 19 01 29 08 15 00 25 01 75 01 95 08 81 02
    05 06 09 20 15 00 26 c8 00 75 08 95 01 81 02
    85 02 05 85 09 44 15 00 25 01 75 01 95 01 81 02 75 07 95 01 81 03 c0";

fn one(d: &ReportDescriptor) -> BatteryMap {
    let all = BatteryMap::find_all(d);
    assert_eq!(all.len(), 1, "batteries {all:?}");
    all.into_iter().next().unwrap()
}

#[test]
fn strength_scales_from_the_logical_range() {
    let b = one(&desc(PAD));
    assert_eq!((b.report_id, b.min, b.max, b.strength.len()), (1, 0, 200, 1));
    assert_eq!(b.read(&[1, 0x80, 0x80, 0, 100]).capacity, Some(50));
    assert_eq!(b.read(&[1, 0x80, 0x80, 0, 160]).capacity, Some(80));
    assert_eq!(b.read(&[1, 0x80, 0x80, 0, 200]).capacity, Some(100));
    // 0 is a real 0 % since Linux 7 (dropped only on the Digitizer page, hid-input.c:681); values outside the
    // range are dropped; a short report has no value.
    assert_eq!(b.read(&[1, 0x80, 0x80, 0, 0]).capacity, Some(0));
    assert_eq!(b.read(&[1, 0x80, 0x80, 0, 201]), Reading::default());
    assert_eq!(b.read(&[1, 0x80, 0x80]), Reading::default());
    assert_eq!(b.read(&[]), Reading::default());
    // Charging comes in report 2, which has no battery: Linux drops it (hidinput_find_battery finds none).
    assert!(b.charging.is_empty());
    assert_eq!(b.read(&[2, 1]), Reading::default());
    assert_eq!(b.read(&[9, 1, 1, 1, 1]), Reading::default());
}

#[test]
fn charging_counts_in_the_report_of_its_battery() {
    // Report 3: Battery Strength 0..100 and Charging, as a DualShock 4 style pad puts them together.
    let d = desc("05 01 09 05 a1 01 85 03 09 30 15 00 26 ff 00 75 08 95 01 81 02
        05 06 09 20 15 00 25 64 75 08 95 01 81 02
        05 85 09 44 15 00 25 01 75 01 95 01 81 02 75 07 95 01 81 03 c0");
    let b = one(&d);
    assert_eq!((b.report_id, b.strength.len(), b.charging.len()), (3, 1, 1));
    assert_eq!(b.read(&[3, 0x80, 64, 1]), Reading { capacity: Some(64), charging: Some(true) });
    assert_eq!(b.read(&[3, 0x80, 0, 0]), Reading { capacity: Some(0), charging: Some(false) });
}

#[test]
fn one_battery_per_report_id() {
    // Report 1: Battery Strength 0..200; report 2: X only; report 4: State Of Charge (declared 0..255) and a
    // second strength field, which only updates report 4's battery. Two batteries, in report order, each with
    // its own range (Linux 6.6 had one per device, ranged by the first field).
    let d = desc("05 01 09 05 a1 01 85 01 05 06 09 20 15 00 26 c8 00 75 08 95 01 81 02
        85 02 05 01 09 30 15 00 26 ff 00 75 08 95 01 81 02
        85 04 05 85 09 65 15 00 26 ff 00 75 08 95 01 81 02
        05 06 09 20 15 00 26 c8 00 75 08 95 01 81 02 c0");
    let all = BatteryMap::find_all(&d);
    let ids: Vec<(u8, i32, i32, usize)> = all.iter().map(|b| (b.report_id, b.min, b.max, b.strength.len())).collect();
    assert_eq!(ids, [(1, 0, 200, 1), (4, 0, 100, 2)]);
    assert_eq!(all[0].read(&[1, 50]).capacity, Some(25));
    assert_eq!(all[1].read(&[1, 50]).capacity, None);
    // Report 4: the State Of Charge 42 counts, then the second value 100 (in 0..100: 100 %) wins as Linux's
    // later usage does.
    assert_eq!(all[1].read(&[4, 42, 100]).capacity, Some(100));
    assert_eq!(all[1].read(&[4, 42, 101]).capacity, Some(42));
}

#[test]
fn state_of_charge_is_a_percentage() {
    // Absolute State Of Charge declared 0..255: Linux takes 0..100 (is_percentage), 255 is out of range.
    let d = desc("05 01 09 04 a1 01 09 30 15 00 26 ff 00 75 08 95 01 81 02
        05 85 09 65 81 02 c0");
    let b = one(&d);
    assert_eq!((b.report_id, b.min, b.max), (0, 0, 100));
    assert_eq!(b.read(&[0x80, 42]).capacity, Some(42));
    assert_eq!(b.read(&[0x80, 0]).capacity, Some(0));
    assert_eq!(b.read(&[0x80, 255]).capacity, None);
}

#[test]
fn no_battery_without_strength_or_input_application() {
    // Georg's DragonRise layout (sticks and buttons only).
    let pad = desc("05 01 09 04 a1 01 09 30 15 00 26 ff 00 75 08 95 01 81 02 c0");
    assert_eq!(BatteryMap::find_all(&pad), []);
    // Charging alone registers nothing in Linux.
    let charging = desc("05 01 09 05 a1 01 05 85 09 44 15 00 25 01 75 01 95 01 81 02 75 07 81 03 c0");
    assert_eq!(BatteryMap::find_all(&charging), []);
    // A vendor application: hidinput_connect stops before looking at usages.
    let vendor = desc("06 00 ff 09 01 a1 01 05 06 09 20 15 00 26 ff 00 75 08 95 01 81 02 c0");
    assert_eq!(BatteryMap::find_all(&vendor), []);
    // A constant field and a feature report do not count.
    let constant = desc("05 01 09 05 a1 01 05 06 09 20 15 00 26 ff 00 75 08 95 01 81 03 b1 02 c0");
    assert_eq!(BatteryMap::find_all(&constant), []);
}

#[test]
fn usage_ranges_and_repeated_usages() {
    // Usage Minimum 0 .. Maximum 0xFFFF on page 6, 64 one-bit values: Battery Strength is value 0x20 (bit 32).
    let d = desc("05 01 09 05 a1 01 05 06 19 00 2a ff ff 15 00 25 01 75 01 95 40 81 02 c0");
    let b = one(&d);
    assert_eq!((b.strength.len(), b.strength[0].bit_offset), (1, 32));
    assert_eq!(b.read(&[0, 0, 0, 0, 1, 0, 0, 0]).capacity, Some(100));
    // One usage, Report Count 0xFFFF: the usage repeats; the search costs the runs, not the values.
    let d = desc("05 01 09 05 a1 01 05 06 09 20 15 00 26 ff 00 75 08 96 ff ff 81 02 c0");
    let b = one(&d);
    assert_eq!((b.strength.len(), b.strength[0].bit_offset), (1, 0));
}
