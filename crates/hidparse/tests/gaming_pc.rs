//! GP-BUS (plan/spec/gaming-pc.md §5): the report descriptors of the Gaming-PC's keyboard (Turtle Beach Vulcan II,
//! 10f5:501b, four HID interfaces) and mouse (Logitech G502, 046d:c08b, two HID interfaces), read from the running
//! Linux on 14.11.2026 (`/sys/bus/hid/devices/*/report_descriptor`; descriptor bytes only). Both sit on root ports of
//! the chipset's xHCI at full speed. What this proves: every interface parses, the typing and pointing interfaces map
//! to a keyboard and a mouse, and reports in the shape the descriptors define give the expected key and motion
//! events. What it cannot prove is what the devices really send; the first round at the machine shows that.

mod common;

use hidparse::codes::*;
use hidparse::{Event, InputMap, InputState, ReportDescriptor, parse};

fn desc(s: &str) -> ReportDescriptor {
    parse(&common::json::hex(&s.split_whitespace().collect::<String>())).unwrap()
}

fn run(d: &ReportDescriptor, reports: &[&[u8]]) -> Vec<Vec<(usize, u16, u16, i32)>> {
    let map = InputMap::new(d);
    let mut state = InputState::new(d, &map, 64);
    let mut each = |r: &&[u8]| {
        let mut out: Vec<Event> = Vec::new();
        state.process(d, &map, r, &mut out).unwrap();
        out.into_iter().map(|e| (e.group, e.ev, e.code, e.value)).collect()
    };
    reports.iter().map(&mut each).collect()
}

fn syn(g: usize) -> (usize, u16, u16, i32) {
    (g, EV_SYN, SYN_REPORT, 0)
}

/// Interface 0 (subclass 1, protocol 1, 8-byte endpoint): exactly the boot keyboard layout, no report ID.
const VULCAN_BOOT_KEYBOARD: &str = "05 01 09 06 a1 01 05 07 19 e0 29 e7 15 00 25 01 75 01 95 08 81 02 81 01
    75 08 95 06 15 00 26 a4 00 19 00 29 a4 81 00 05 08 19 01 29 05 15 00 25 01 75 01 95 05 91 02 95 03 91 01 c0";

/// Interface 1 (64-byte endpoint): mouse (ID 1), system control (ID 2), consumer control (ID 0x42), vendor pages.
const VULCAN_EXTRAS: &str = "05 01 09 02 a1 01 85 01 09 01 a1 00 05 09 19 01 29 05 15 00 25 01 95 05 75 01 81 02
    95 01 75 03 81 01 05 01 09 30 09 31 16 00 80 26 ff 7f 95 02 75 10 81 06 09 38 15 81 25 7f 95 01 75 08 81 06
    c0 c0 05 01 09 80 a1 01 85 02 19 81 29 83 15 00 25 01 75 01 95 03 81 02 95 05 81 01 c0 05 0c 09 01 a1 01 85
    42 19 00 2a ff 02 15 00 26 ff 02 75 10 95 01 81 00 c0 06 02 ff 09 01 a1 01 85 03 15 00 26 ff 00 09 2f 75 08
    95 07 81 00 c0 06 01 ff 09 01 a1 01 85 04 19 00 29 7f 15 00 25 7f 75 08 95 03 b1 01 85 05 95 03 b1 01 85 06
    95 85 b1 01 85 07 95 0a b1 01 85 08 96 13 04 b1 01 85 09 95 08 b1 01 85 0a 95 bc b1 01 85 0b 95 34 b1 01 85
    0c 95 58 b1 01 85 0d 95 0f b1 01 85 0e 95 04 b1 01 85 0f 95 7f b1 01 85 10 95 0f b1 01 85 11 96 78 01 b1 01
    85 12 95 0d b1 01 85 13 95 7a b1 01 c0";

/// Interface 2 (24-byte endpoint): a second keyboard, 24 key codes per report (the keys past six at once).
const VULCAN_WIDE_KEYBOARD: &str = "05 01 09 06 a1 01 05 07 05 07 19 00 29 91 15 00 26 ff 00 75 08 95 18 81 00 c0";

/// Interface 3: vendor page only (lighting); no input device may come of it.
const VULCAN_VENDOR: &str = "06 00 ff 09 01 a1 01 05 07 09 00 15 00 26 ff 00 75 08 95 40 81 02 05 08 09 00 15 00 26 ff 00
    75 08 95 40 91 02 c0";

/// Interface 0 (subclass 1, protocol 2, 8-byte endpoint): 16 buttons, X and Y 16 bits, wheel, AC Pan; no report ID.
const G502_MOUSE: &str = "05 01 09 02 a1 01 09 01 a1 00 05 09 19 01 29 10 15 00 25 01 95 10 75 01 81 02 05 01 16 01 80
    26 ff 7f 75 10 95 02 09 30 09 31 81 06 15 81 25 7f 75 08 95 01 09 38 81 06 05 0c 0a 38 02 95 01 81 06 c0 c0";

/// Interface 1 (20-byte endpoint): keyboard (ID 1), consumer (ID 3), system control (ID 4), vendor (0x10, 0x11).
const G502_EXTRAS: &str = "05 01 09 06 a1 01 85 01 05 07 19 e0 29 e7 15 00 25 01 75 01 95 08 81 02 81 03 95 06 75 08 15
    00 26 a4 00 19 00 2a a4 00 81 00 c0 05 0c 09 01 a1 01 85 03 75 10 95 02 15 01 26 8c 02 19 01 2a 8c 02 81 00 c0
    05 01 09 80 a1 01 85 04 75 02 95 01 15 01 25 03 09 82 09 81 09 83 81 60 75 06 81 03 c0 06 00 ff 09 01 a1 01 85
    10 75 08 95 06 15 00 26 ff 00 09 01 81 00 09 01 91 00 c0 06 00 ff 09 02 a1 01 85 11 75 08 95 13 15 00 26 ff 00
    09 02 81 00 09 02 91 00 c0";

#[test]
fn every_interface_of_both_devices_parses() {
    for (name, d, bytes) in [
        ("keyboard 0", VULCAN_BOOT_KEYBOARD, 60),
        ("keyboard 1", VULCAN_EXTRAS, 257),
        ("keyboard 2", VULCAN_WIDE_KEYBOARD, 26),
        ("keyboard 3", VULCAN_VENDOR, 38),
        ("mouse 0", G502_MOUSE, 67),
        ("mouse 1", G502_EXTRAS, 151),
    ] {
        let raw = common::json::hex(&d.split_whitespace().collect::<String>());
        assert_eq!(raw.len(), bytes, "{name}: typed length");
        assert!(parse(&raw).is_ok(), "{name}");
    }
}

#[test]
fn the_keyboards_typing_interface_is_the_boot_layout_and_types() {
    let d = desc(VULCAN_BOOT_KEYBOARD);
    let map = InputMap::new(&d);
    assert_eq!(map.groups.len(), 1);
    let g = &map.groups[0];
    assert!(g.repeat, "held keys repeat");
    for k in [KEY_A, KEY_LEFTSHIFT, KEY_ENTER, KEY_ESC, KEY_SPACE, KEY_F11] {
        assert!(g.keys.contains(&k), "key {k:#x}");
    }
    assert_eq!(g.leds.len(), 5, "Num, Caps, Scroll, Compose, Kana");
    // Shift down, then Shift+a, then all up: byte 0 modifiers, byte 1 constant, bytes 2-7 key codes.
    let r = run(&d, &[&[0x02, 0, 0, 0, 0, 0, 0, 0], &[0x02, 0, 0x04, 0, 0, 0, 0, 0], &[0; 8]]);
    assert_eq!(r[0], [(0, EV_MSC, MSC_SCAN, 0x700e1), (0, EV_KEY, KEY_LEFTSHIFT, 1), syn(0)]);
    assert_eq!(r[1], [(0, EV_MSC, MSC_SCAN, 0x70004), (0, EV_KEY, KEY_A, 1), syn(0)]);
    assert!(r[2].contains(&(0, EV_KEY, KEY_A, 0)) && r[2].contains(&(0, EV_KEY, KEY_LEFTSHIFT, 0)));
    // A report shorter or with an ID in front would not be this interface's: 8 bytes, no ID.
    assert!(!d.reports.iter().any(|x| x.id != 0));
}

#[test]
fn the_wide_keyboard_interface_gives_the_same_keys() {
    let d = desc(VULCAN_WIDE_KEYBOARD);
    let map = InputMap::new(&d);
    assert_eq!(map.groups.len(), 1);
    assert!(map.groups[0].keys.contains(&KEY_A) && map.groups[0].keys.contains(&KEY_Z));
    let mut seven = [0u8; 24];
    seven[..7].copy_from_slice(&[0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a]);
    let r = run(&d, &[&seven, &[0; 24]]);
    let down: Vec<u16> = r[0].iter().filter(|e| e.1 == EV_KEY && e.3 == 1).map(|e| e.2).collect();
    assert_eq!(down, [KEY_A, KEY_B, KEY_C, KEY_D, KEY_E, KEY_F, KEY_G], "seven keys at once");
    assert_eq!(r[1].iter().filter(|e| e.1 == EV_KEY && e.3 == 0).count(), 7);
}

#[test]
fn the_keyboards_extras_do_not_break_and_its_vendor_interface_is_no_input_device() {
    let d = desc(VULCAN_EXTRAS);
    let map = InputMap::new(&d);
    // Mouse, system control and consumer control are input applications; the two vendor pages are not.
    assert!(map.groups.iter().any(|g| g.rel.contains(&REL_X)), "the mouse collection");
    assert!(map.groups.iter().any(|g| g.keys.contains(&KEY_VOLUMEUP)), "the media keys");
    // Volume up on report 0x42 (16-bit usage 0x00e9), then released.
    let r = run(&d, &[&[0x42, 0xe9, 0x00], &[0x42, 0x00, 0x00]]);
    assert!(r[0].iter().any(|e| (e.1, e.2, e.3) == (EV_KEY, KEY_VOLUMEUP, 1)), "{:?}", r[0]);
    assert!(r[1].iter().any(|e| (e.1, e.2, e.3) == (EV_KEY, KEY_VOLUMEUP, 0)));
    // The vendor report 3 is accepted and presses no key and moves nothing (hid-input files such bytes under
    // ABS_MISC, as Linux does for a usage page it does not know).
    let v = run(&d, &[&[0x03, 1, 2, 3, 4, 5, 6, 7]]);
    assert!(!v[0].iter().any(|e| e.1 == EV_KEY || e.1 == EV_REL), "{:?}", v[0]);
    let v = desc(VULCAN_VENDOR);
    assert!(InputMap::new(&v).groups.iter().all(|g| g.is_empty()));
}

#[test]
fn the_mouse_moves_clicks_and_scrolls() {
    let d = desc(G502_MOUSE);
    let map = InputMap::new(&d);
    assert_eq!(map.groups.len(), 1);
    let g = &map.groups[0];
    for b in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
        assert!(g.keys.contains(&b));
    }
    assert!(g.rel.contains(&REL_X) && g.rel.contains(&REL_Y) && g.rel.contains(&REL_WHEEL) && g.rel.contains(&REL_HWHEEL));
    // Bytes 0-1 buttons, 2-3 X, 4-5 Y (little-endian, signed), 6 wheel, 7 pan: left button, 5 right, 3 up, wheel -1.
    let r = run(&d, &[&[0x01, 0x00, 0x05, 0x00, 0xfd, 0xff, 0xff, 0x00], &[0; 8]]);
    let has = |v: &[(usize, u16, u16, i32)], ev, code, value| v.iter().any(|e| (e.1, e.2, e.3) == (ev, code, value));
    assert!(has(&r[0], EV_KEY, BTN_LEFT, 1), "{:?}", r[0]);
    assert!(has(&r[0], EV_REL, REL_X, 5) && has(&r[0], EV_REL, REL_Y, -3) && has(&r[0], EV_REL, REL_WHEEL, -1));
    assert!(has(&r[1], EV_KEY, BTN_LEFT, 0));
    assert!(!r[1].iter().any(|e| e.1 == EV_REL && e.3 != 0), "no motion in a still report: {:?}", r[1]);
}

#[test]
fn the_mouses_second_interface_parses_into_a_keyboard_and_media_keys() {
    let d = desc(G502_EXTRAS);
    let map = InputMap::new(&d);
    assert!(map.groups.iter().any(|g| g.keys.contains(&KEY_A)), "macro keys type through report 1");
    // Report 1: ID, modifiers, a constant byte, six key codes.
    let r = run(&d, &[&[0x01, 0x00, 0x00, 0x04, 0, 0, 0, 0, 0]]);
    assert!(r[0].iter().any(|e| (e.1, e.2, e.3) == (EV_KEY, KEY_A, 1)), "{:?}", r[0]);
    // The vendor reports 0x10 and 0x11 (Logitech's own protocol) are accepted and press no key.
    let v = run(&d, &[&[0x10, 0, 0, 0, 0, 0, 0]]);
    assert!(!v[0].iter().any(|e| e.1 == EV_KEY || e.1 == EV_REL), "{:?}", v[0]);
}
