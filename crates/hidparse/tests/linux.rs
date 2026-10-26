//! Linux behaviour checked by hand against drivers/hid/hid-core.c and hid-input.c (v6.6).

mod common;

use common::descs;
use hidparse::boot::{boot_keyboard, boot_mouse};
use hidparse::codes::*;
use hidparse::{Event, InputMap, InputState, ParseErrorKind, ReportDescriptor, ReportKind, parse};

fn run(desc: &ReportDescriptor, reports: &[&[u8]]) -> Vec<Vec<(u16, u16, i32)>> {
    let map = InputMap::new(desc);
    let mut state = InputState::new(desc, &map, 64);
    reports
        .iter()
        .map(|r| {
            let mut out: Vec<Event> = Vec::new();
            state.process(desc, &map, r, &mut out).unwrap();
            out.into_iter().map(|e| (e.ev, e.code, e.value)).collect()
        })
        .collect()
}

const SYN: (u16, u16, i32) = (EV_SYN, SYN_REPORT, 0);

#[test]
fn boot_keyboard_keys_scan_codes_and_rollover() {
    let d = boot_keyboard();
    let map = InputMap::new(&d);
    assert_eq!(map.groups.len(), 1);
    let g = &map.groups[0];
    assert!(g.repeat);
    assert!(g.keys.contains(&KEY_A) && g.keys.contains(&KEY_LEFTSHIFT) && g.keys.contains(&KEY_RO));
    assert_eq!(
        g.leds.iter().copied().collect::<Vec<_>>(),
        [LED_NUML, LED_CAPSL, LED_SCROLLL, LED_COMPOSE, LED_KANA]
    );
    let ev = run(
        &d,
        &[
            &[0x02, 0, 0x04, 0, 0, 0, 0, 0],
            &[0x02, 0, 0x04, 0x05, 0, 0, 0, 0],
            &[0, 0, 1, 1, 1, 1, 1, 1],
            &[0; 8],
        ],
    );
    assert_eq!(
        ev[0],
        [
            (EV_MSC, MSC_SCAN, 0x700E1),
            (EV_KEY, KEY_LEFTSHIFT, 1),
            (EV_MSC, MSC_SCAN, 0x70004),
            (EV_KEY, KEY_A, 1),
            SYN
        ]
    );
    assert_eq!(
        ev[1],
        [(EV_MSC, MSC_SCAN, 0x70005), (EV_KEY, KEY_B, 1), SYN]
    );
    // ErrorRollOver: the key array keeps its old state; the modifier byte still counts.
    assert_eq!(
        ev[2],
        [(EV_MSC, MSC_SCAN, 0x700E1), (EV_KEY, KEY_LEFTSHIFT, 0), SYN]
    );
    assert_eq!(
        ev[3],
        [
            (EV_MSC, MSC_SCAN, 0x70004),
            (EV_KEY, KEY_A, 0),
            (EV_MSC, MSC_SCAN, 0x70005),
            (EV_KEY, KEY_B, 0),
            SYN
        ]
    );
}

#[test]
fn boot_mouse_buttons_moves_and_wheel() {
    let d = boot_mouse();
    let g = &InputMap::new(&d).groups[0];
    assert_eq!(
        g.keys.iter().copied().collect::<Vec<_>>(),
        [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA]
    );
    assert_eq!(
        g.rel.iter().copied().collect::<Vec<_>>(),
        [REL_X, REL_Y, REL_WHEEL, REL_WHEEL_HI_RES]
    );
    let ev = run(&d, &[&[0x01, 5, 0xFB, 1], &[0, 0, 0]]);
    assert_eq!(
        ev[0],
        [
            (EV_MSC, MSC_SCAN, 0x90001),
            (EV_KEY, BTN_LEFT, 1),
            (EV_REL, REL_X, 5),
            (EV_REL, REL_Y, -5),
            (EV_REL, REL_WHEEL, 1),
            (EV_REL, REL_WHEEL_HI_RES, 120),
            SYN
        ]
    );
    // A 3-byte boot report: the wheel byte reads 0 (Linux pads short reports with zeros).
    assert_eq!(
        ev[1],
        [
            (EV_MSC, MSC_SCAN, 0x90001),
            (EV_KEY, BTN_LEFT, 0),
            (EV_REL, REL_X, 0),
            (EV_REL, REL_Y, 0),
            SYN
        ]
    );
}

#[test]
fn dragonrise_as_hid_generic_maps_it() {
    let d = parse(&descs::dragonrise()).unwrap();
    let map = InputMap::new(&d);
    assert_eq!(map.groups.len(), 1);
    let g = &map.groups[0];
    assert_eq!(g.application, 0x10004);
    assert_eq!(
        g.abs.keys().copied().collect::<Vec<_>>(),
        [ABS_X, ABS_Y, ABS_Z, ABS_RZ, ABS_HAT0X, ABS_HAT0Y]
    );
    let x = g.abs[&ABS_X];
    assert_eq!((x.min, x.max, x.fuzz, x.flat), (0, 255, 0, 15));
    assert_eq!(g.abs[&ABS_HAT0X].min, -1);
    let buttons: Vec<u16> = (BTN_TRIGGER..BTN_TRIGGER + 12).collect();
    assert_eq!(
        g.keys.iter().copied().collect::<Vec<_>>(),
        buttons,
        "vendor page 0xFF00 bits are ignored"
    );
    let ev = run(
        &d,
        &[
            &[0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0x0F, 0x00, 0xC0],
            &[0, 0, 0x7F, 0x7F, 0x7F, 0x4F, 0x20, 0xC0],
        ],
    );
    assert_eq!(
        ev[0],
        [
            (EV_ABS, ABS_X, 127),
            (EV_ABS, ABS_Y, 127),
            (EV_ABS, ABS_Z, 127),
            (EV_ABS, ABS_Z, 127),
            (EV_ABS, ABS_RZ, 127),
            (EV_ABS, ABS_HAT0X, 0),
            (EV_ABS, ABS_HAT0Y, 0),
            SYN
        ]
    );
    // Stick left and up, B (button 3), Start (button 10); the hat stays centred (0xF) and, as in
    // hid-input, is sent again with every report.
    assert_eq!(
        ev[1],
        [
            (EV_ABS, ABS_X, 0),
            (EV_ABS, ABS_Y, 0),
            (EV_ABS, ABS_HAT0X, 0),
            (EV_ABS, ABS_HAT0Y, 0),
            (EV_MSC, MSC_SCAN, 0x90003),
            (EV_KEY, BTN_THUMB2, 1),
            (EV_MSC, MSC_SCAN, 0x9000A),
            (EV_KEY, BTN_BASE4, 1),
            SYN
        ]
    );
}

#[test]
fn snes_clone_every_x_reports_and_the_last_one_counts() {
    let d = parse(&descs::snes_clone()).unwrap();
    let map = InputMap::new(&d);
    let g = &map.groups[0];
    assert!(
        g.abs.contains_key(&ABS_X) && g.abs.contains_key(&ABS_Y) && !g.abs.contains_key(&ABS_Z)
    );
    // Idle, then D-pad left: only the fourth X changes, and unchanged values are not sent again,
    // so the last X event of the frame is the D-pad's.
    let ev = run(
        &d,
        &[
            &[0x80, 0x80, 0x80, 0x80, 0x80, 0, 0, 0],
            &[0x80, 0x80, 0x80, 0x00, 0x80, 0, 0, 0],
        ],
    );
    let xs = |n: usize| -> Vec<i32> {
        ev[n]
            .iter()
            .filter(|e| e.0 == EV_ABS && e.1 == ABS_X)
            .map(|e| e.2)
            .collect()
    };
    assert_eq!(xs(0), [0x80, 0x80, 0x80, 0x80]);
    assert_eq!(xs(1), [0]);
}

#[test]
fn dualshock4_is_a_gamepad_with_south_east_buttons() {
    let d = parse(&descs::dualshock4()).unwrap();
    assert!(d.numbered(ReportKind::Input));
    assert_eq!(d.report(ReportKind::Input, 1).unwrap().len_bytes(true), 64);
    let g = &InputMap::new(&d).groups[0];
    let buttons: Vec<u16> = (BTN_SOUTH..BTN_SOUTH + 14).collect();
    assert_eq!(g.keys.iter().copied().collect::<Vec<_>>(), buttons);
    assert_eq!(
        g.abs.keys().copied().collect::<Vec<_>>(),
        [
            ABS_X, ABS_Y, ABS_Z, ABS_RX, ABS_RY, ABS_RZ, ABS_HAT0X, ABS_HAT0Y
        ]
    );
}

#[test]
fn two_applications_two_devices() {
    let d = parse(&descs::keyboard_and_pad()).unwrap();
    let map = InputMap::new(&d);
    assert_eq!(
        map.groups.iter().map(|g| g.application).collect::<Vec<_>>(),
        [0x10006, 0x10005]
    );
    let mut state = InputState::new(&d, &map, 64);
    let mut out = Vec::new();
    state
        .process(&d, &map, &[2, 0x01, 0, 0, 0], &mut out)
        .unwrap();
    assert!(out.iter().all(|e| e.group == 1));
    assert!(out.contains(&Event {
        group: 1,
        ev: EV_KEY,
        code: BTN_SOUTH,
        value: 1
    }));
    assert_eq!(
        state.process(&d, &map, &[3, 0], &mut out),
        Err(hidparse::InputError::UnknownReport(3))
    );
}

#[test]
fn hats_four_ways_degrees_and_null_state() {
    // A 4-way hat 0-3 in 2 bits, an 8-way hat 0-315 degrees in 16 bits (V1 refused that one).
    let d = parse(&common::json::hex(
        "05010905a101093915002503750295018142750281030939150026 3b01 7510 95018142 05091901290415002501750195048102c0"
            .replace(' ', "")
            .as_str(),
    ))
    .unwrap();
    let ev = run(&d, &[&[0x01, 0x5A, 0x00, 0x01], &[0x02, 0x08, 0x00, 0x00]]);
    // The first hat takes ABS_HAT0X/Y; the second hat is a duplicate and ignored, as in Linux.
    assert_eq!(ev[0][..2], [(EV_ABS, ABS_HAT0X, 1), (EV_ABS, ABS_HAT0Y, 0)]);
    assert_eq!(ev[1][..2], [(EV_ABS, ABS_HAT0X, 0), (EV_ABS, ABS_HAT0Y, 1)]);
}

#[test]
fn parser_rules() {
    let p = |s: &str| parse(&common::json::hex(&s.replace(' ', "")));
    // Linux: a Usage Page after the usages re-pages them; one between usages does not.
    let d = p("05 09 09 01 05 01 09 30 75 08 95 02 81 02").unwrap();
    let u = &d.reports[0].fields[0].usages;
    assert_eq!((u.get(0), u.get(1)), (Some(0x90001), Some(0x10030)));
    let d = p("05 09 09 30 05 01 75 08 95 01 81 02").unwrap();
    assert_eq!(d.reports[0].fields[0].usage(0), Some(0x10030));
    // Logical maximum: unsigned while the minimum declared so far is not negative.
    let f = |s: &str| p(s).unwrap().reports[0].fields[0].clone();
    assert_eq!((f("15 00 25 ff 75 08 95 01 81 02").logical_max), 255);
    assert_eq!((f("15 ff 25 ff 75 08 95 01 81 02").logical_max), -1);
    let fl = f("25 ff 15 81 75 08 95 01 81 02");
    assert_eq!((fl.logical_min, fl.logical_max), (-127, 255));
    assert_eq!(f("55 0f 75 08 95 01 81 02").unit_exponent, -1);
    assert_eq!(f("55 fe 75 08 95 01 81 02").unit_exponent, -2);
    // Five Push deep, long items, reserved global tags: accepted (V1/Linux refuse some of them).
    let d = p("a4 a4 a4 a4 a4 b4 b4 b4 b4 b4 fe 02 10 aa bb c4 7c 75 01 95 01 81 02").unwrap();
    assert_eq!(d.max_push_depth, 5);
    assert_eq!(p("85 00").unwrap_err().kind, ParseErrorKind::BadReportId(0));
    assert_eq!(
        p("86 00 01").unwrap_err().kind,
        ParseErrorKind::BadReportId(256)
    );
    assert_eq!(p("b4").unwrap_err().kind, ParseErrorKind::PopWithoutPush);
    assert_eq!(
        p("c0").unwrap_err().kind,
        ParseErrorKind::EndWithoutCollection
    );
    assert_eq!(
        p("a1 01").unwrap_err().kind,
        ParseErrorKind::UnclosedCollection
    );
    assert_eq!(
        p("26 ff").unwrap_err(),
        hidparse::ParseError {
            offset: 0,
            kind: ParseErrorKind::Truncated
        }
    );
    assert_eq!(
        p("fe 05 00 01").unwrap_err().kind,
        ParseErrorKind::Truncated
    );
    assert_eq!(p("").unwrap(), ReportDescriptor::default());
}

#[test]
fn values_out_of_reports() {
    use hidparse::ExtractError;
    let d = parse(&common::json::hex(
        "1581257f7508950381029501750c16 00f8 26ff07 8102"
            .replace(' ', "")
            .as_str(),
    ))
    .unwrap();
    let r = &d.reports[0];
    let (a, b) = (&r.fields[0], &r.fields[1]);
    let data = [0xFF, 0x7F, 0x80, 0x00, 0xF8];
    assert_eq!(a.value(&data, 0), Ok(-1));
    assert_eq!(a.value(&data, 1), Ok(127));
    assert_eq!(a.value(&data, 2), Ok(-128));
    assert_eq!(a.value(&data, 3), Err(ExtractError::NoSuchValue));
    assert_eq!(b.bit_offset, 24);
    assert_eq!(b.value(&data, 0), Ok(-2048));
    assert_eq!(b.value(&data[..4], 0), Err(ExtractError::PastEnd));
    assert_eq!(
        hidparse::extract::bits(&data, 0, 33),
        Err(ExtractError::TooWide)
    );
}

#[test]
fn duplicates_follow_linux() {
    // Relative X twice (map_rel, not "clear"): the second is ignored. Absolute X twice: both report.
    let d = parse(&common::json::hex(
        "05010902a1010930093015812 57f7508950281060930093081 02c0"
            .replace(' ', "")
            .as_str(),
    ));
    let d = d.unwrap();
    let map = InputMap::new(&d);
    let ev = run(&d, &[&[1, 2, 3, 4]]);
    assert_eq!(
        ev[0],
        [
            (EV_REL, REL_X, 1),
            (EV_ABS, ABS_X, 3),
            (EV_ABS, ABS_X, 4),
            SYN
        ]
    );
    assert_eq!(
        map.groups[0].rel.iter().copied().collect::<Vec<_>>(),
        [REL_X]
    );
}

#[test]
fn abs_resolution_like_linux() {
    // X 0..4095 over 0..1000 hundredths of an inch: 4095 * 1000 / (1000 * 254) = 16 units/mm.
    let d = parse(&common::json::hex(
        "05010902a1010930150026ff0f350046e803651355 0e 7510950181 02c0"
            .replace(' ', "")
            .as_str(),
    ));
    let map = InputMap::new(&d.unwrap());
    assert_eq!(map.groups[0].abs[&ABS_X].resolution, 16);
}

#[test]
fn hostile_counts_stay_cheap() {
    // Report Count 0xFFFFFFFF over a usage range of 4 billion, in a keyboard array and a variable field.
    let desc = common::json::hex(
        "05070901a101150027ffffffff19002bffffffff75019700000100 8100 7520 97ffffffff 8102 c0"
            .replace(' ', "")
            .as_str(),
    );
    let t = std::time::Instant::now();
    let d = parse(&desc).unwrap();
    let map = InputMap::new(&d);
    let mut state = InputState::new(&d, &map, 64);
    let mut out = Vec::new();
    for i in 0..100u8 {
        state.process(&d, &map, &[i; 64], &mut out).unwrap();
    }
    // Zero-width values, four billion of them, in an array and a variable field.
    let d = parse(&common::json::hex(
        "0509190129ff7500 97ffffffff 8100 8102"
            .replace(' ', "")
            .as_str(),
    ))
    .unwrap();
    let map = InputMap::new(&d);
    let mut state = InputState::new(&d, &map, 64);
    state.process(&d, &map, &[0xFF; 8], &mut out).unwrap();
    assert!(t.elapsed().as_secs() < 5, "took {:?}", t.elapsed());
}

/// The DualShock 3 as hid-generic sees it (V2 has no hid-sony mapping yet, IN-5): a Joystick with 19 buttons
/// (BTN_JOYSTICK + 0..15, then BTN_TRIGGER_HAPPY1..3, hid-input.c:787-794), the sticks on X, Y, Z, Rz, the first
/// pressure byte on ABS_MISC (the unknown default, hid-input.c:1345-1360) and every later Pointer value ignored as
/// a duplicate (hid-input.c:1394-1402, `ignore:` zeroes the usage). Joystick axes get fuzz span>>8, flat span>>4.
#[test]
fn dualshock3_as_hid_generic_maps_it() {
    let raw = descs::dualshock3();
    assert_eq!(raw.len(), 148);
    let d = parse(&raw).unwrap();
    assert_eq!(d.report(ReportKind::Input, 1).unwrap().len_bytes(true), 49);
    let map = InputMap::new(&d);
    assert_eq!(map.groups.len(), 1);
    let g = &map.groups[0];
    assert_eq!(g.application, 0x10004);
    let mut buttons: Vec<u16> = (BTN_TRIGGER..BTN_TRIGGER + 16).collect();
    buttons.extend(BTN_TRIGGER_HAPPY1..BTN_TRIGGER_HAPPY1 + 3);
    assert_eq!(g.keys.iter().copied().collect::<Vec<_>>(), buttons);
    assert_eq!(g.abs.keys().copied().collect::<Vec<_>>(), [ABS_X, ABS_Y, ABS_Z, ABS_RZ, ABS_MISC]);
    assert!(g.abs.values().all(|a| (a.min, a.max, a.fuzz, a.flat) == (0, 255, 0, 15)));
    let mut idle = [0u8; 49];
    idle[0] = 1;
    idle[6..10].fill(0x80);
    // Select (button 1) and D-pad up (button 5), left stick left, first pressure byte 0x40, a motion value.
    let mut r = idle;
    r[2] = 0x11;
    r[6] = 0;
    r[10] = 0x40;
    r[41] = 0x02;
    r[11] = 0x33;
    let ev = run(&d, &[&idle, &r]);
    assert_eq!(ev[0], [(EV_ABS, ABS_X, 128), (EV_ABS, ABS_Y, 128), (EV_ABS, ABS_Z, 128), (EV_ABS, ABS_RZ, 128), SYN]);
    assert_eq!(
        ev[1],
        [
            (EV_MSC, MSC_SCAN, 0x90001),
            (EV_KEY, BTN_TRIGGER, 1),
            (EV_MSC, MSC_SCAN, 0x90005),
            (EV_KEY, BTN_TRIGGER + 4, 1), // BTN_TOP2
            (EV_ABS, ABS_X, 0),
            (EV_ABS, ABS_MISC, 0x40),
            SYN
        ]
    );
}
