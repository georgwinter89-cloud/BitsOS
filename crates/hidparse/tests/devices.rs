//! Typical report-protocol devices beyond the boot layouts and the pads, with the event lists
//! Linux hid-input hands to the input core (checked by hand against hid-input.c, v6.6).

mod common;

use hidparse::codes::*;
use hidparse::{Event, InputMap, InputState, ReportDescriptor, parse};

fn desc(s: &str) -> ReportDescriptor {
    parse(&common::json::hex(
        &s.split_whitespace().collect::<String>(),
    ))
    .unwrap()
}

fn run(d: &ReportDescriptor, reports: &[&[u8]]) -> Vec<Vec<(usize, u16, u16, i32)>> {
    let map = InputMap::new(d);
    let mut state = InputState::new(d, &map, 64);
    let mut each = |r: &&[u8]| {
        let mut out: Vec<Event> = Vec::new();
        state.process(d, &map, r, &mut out).unwrap();
        out.into_iter()
            .map(|e| (e.group, e.ev, e.code, e.value))
            .collect()
    };
    reports.iter().map(&mut each).collect()
}

const SYN: (u16, u16, i32) = (EV_SYN, SYN_REPORT, 0);

fn syn(g: usize) -> (usize, u16, u16, i32) {
    (g, SYN.0, SYN.1, SYN.2)
}

#[test]
fn five_button_mouse_with_wheel_and_ac_pan() {
    // Report ID 2: 5 buttons, X/Y 12 bits each, wheel, AC Pan (a typical office mouse).
    let d = desc(
        "05 01 09 02 a1 01 85 02 09 01 a1 00
         05 09 19 01 29 05 15 00 25 01 95 05 75 01 81 02 95 01 75 03 81 01
         05 01 16 01 f8 26 ff 07 75 0c 95 02 09 30 09 31 81 06
         15 81 25 7f 75 08 95 01 09 38 81 06
         05 0c 0a 38 02 95 01 81 06 c0 c0",
    );
    let map = InputMap::new(&d);
    assert_eq!(map.groups.len(), 1);
    let g = &map.groups[0];
    assert_eq!(
        g.keys.iter().copied().collect::<Vec<_>>(),
        [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA]
    );
    assert_eq!(
        g.rel.iter().copied().collect::<Vec<_>>(),
        [
            REL_X,
            REL_Y,
            REL_HWHEEL,
            REL_WHEEL,
            REL_WHEEL_HI_RES,
            REL_HWHEEL_HI_RES
        ]
    );
    assert!(g.abs.is_empty() && !g.repeat);
    // Left button, X +5, Y -1 (0xFFF), one notch up, one notch left.
    let ev = run(
        &d,
        &[
            &[2, 0x01, 0x05, 0xF0, 0xFF, 0x01, 0xFF],
            &[2, 0, 0, 0, 0, 0, 0],
        ],
    );
    assert_eq!(
        ev[0],
        [
            (0, EV_MSC, MSC_SCAN, 0x90001),
            (0, EV_KEY, BTN_LEFT, 1),
            (0, EV_REL, REL_X, 5),
            (0, EV_REL, REL_Y, -1),
            (0, EV_REL, REL_WHEEL, 1),
            (0, EV_REL, REL_WHEEL_HI_RES, 120),
            (0, EV_REL, REL_HWHEEL, -1),
            (0, EV_REL, REL_HWHEEL_HI_RES, -120),
            syn(0)
        ]
    );
    // Zero moves are handed on (the input core drops them); zero wheels are not (hid-input).
    assert_eq!(
        ev[1],
        [
            (0, EV_MSC, MSC_SCAN, 0x90001),
            (0, EV_KEY, BTN_LEFT, 0),
            (0, EV_REL, REL_X, 0),
            (0, EV_REL, REL_Y, 0),
            syn(0)
        ]
    );
}

#[test]
fn six_key_keyboard_with_leds_and_consumer_keys() {
    // Report ID 1: modifiers, reserved byte, 5 LEDs (output), 6 keys 0x00-0xA4.
    // Report ID 3: a consumer control array 0x000-0x3FF (volume keys and the like).
    let d = desc(
        "05 01 09 06 a1 01 85 01
         05 07 19 e0 29 e7 15 00 25 01 75 01 95 08 81 02 95 01 75 08 81 01
         95 05 75 01 05 08 19 01 29 05 91 02 95 01 75 03 91 01
         95 06 75 08 15 00 26 a4 00 05 07 19 00 2a a4 00 81 00 c0
         05 0c 09 01 a1 01 85 03 15 00 26 ff 03 19 00 2a ff 03 75 10 95 01 81 00 c0",
    );
    let map = InputMap::new(&d);
    // Two applications, but Consumer Control joins the keyboard's input device
    // (hidinput_match_application, linux-6.6/drivers/hid/hid-input.c:2187-2206).
    assert_eq!(map.groups.len(), 1, "consumer keys on the keyboard's device");
    let (kb, cc) = (&map.groups[0], &map.groups[0]);
    assert_eq!(kb.application, 0x10006);
    assert!(kb.repeat);
    assert_eq!(
        kb.leds.iter().copied().collect::<Vec<_>>(),
        [LED_NUML, LED_CAPSL, LED_SCROLLL, LED_COMPOSE, LED_KANA]
    );
    for k in [
        KEY_A,
        KEY_ESC,
        KEY_LEFTSHIFT,
        KEY_RIGHTMETA,
        KEY_102ND,
        KEY_KPCOMMA,
        KEY_UNKNOWN,
    ] {
        assert!(kb.keys.contains(&k), "{k}");
    }
    for k in [
        KEY_VOLUMEUP,
        KEY_MUTE,
        KEY_PLAYPAUSE,
        KEY_BRIGHTNESSUP,
        KEY_UNKNOWN,
    ] {
        assert!(cc.keys.contains(&k), "{k}");
    }
    let ev = run(
        &d,
        &[
            &[1, 0x02, 0, 0x04, 0x29, 0, 0, 0, 0],
            &[1, 0x00, 0, 0x29, 0, 0, 0, 0, 0],
            &[3, 0xE9, 0x00],
            &[3, 0x00, 0x00],
        ],
    );
    assert_eq!(
        ev[0],
        [
            (0, EV_MSC, MSC_SCAN, 0x700E1),
            (0, EV_KEY, KEY_LEFTSHIFT, 1),
            (0, EV_MSC, MSC_SCAN, 0x70004),
            (0, EV_KEY, KEY_A, 1),
            (0, EV_MSC, MSC_SCAN, 0x70029),
            (0, EV_KEY, KEY_ESC, 1),
            syn(0)
        ]
    );
    // ESC moved from slot 2 to slot 1: still held, only A and Shift are released.
    assert_eq!(
        ev[1],
        [
            (0, EV_MSC, MSC_SCAN, 0x700E1),
            (0, EV_KEY, KEY_LEFTSHIFT, 0),
            (0, EV_MSC, MSC_SCAN, 0x70004),
            (0, EV_KEY, KEY_A, 0),
            syn(0)
        ]
    );
    assert_eq!(
        ev[2],
        [
            (0, EV_MSC, MSC_SCAN, 0xC00E9),
            (0, EV_KEY, KEY_VOLUMEUP, 1),
            syn(0)
        ]
    );
    assert_eq!(
        ev[3],
        [
            (0, EV_MSC, MSC_SCAN, 0xC00E9),
            (0, EV_KEY, KEY_VOLUMEUP, 0),
            syn(0)
        ]
    );
}

#[test]
fn absolute_pointer_like_qemu_usb_tablet() {
    // QEMU's usb-tablet: 3 buttons, absolute X/Y 0..32767, relative wheel.
    let d = desc(
        "05 01 09 02 a1 01 09 01 a1 00
         05 09 19 01 29 03 15 00 25 01 95 03 75 01 81 02 95 01 75 05 81 01
         05 01 09 30 09 31 15 00 26 ff 7f 35 00 46 ff 7f 75 10 95 02 81 02
         05 01 09 38 15 81 25 7f 35 00 45 00 75 08 95 01 81 06 c0 c0",
    );
    let map = InputMap::new(&d);
    let g = &map.groups[0];
    assert_eq!(
        g.keys.iter().copied().collect::<Vec<_>>(),
        [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE]
    );
    assert_eq!(g.abs.keys().copied().collect::<Vec<_>>(), [ABS_X, ABS_Y]);
    let x = g.abs[&ABS_X];
    // Not a joystick: no fuzz or flat; no unit: resolution unknown.
    assert_eq!(
        (x.min, x.max, x.fuzz, x.flat, x.resolution),
        (0, 32767, 0, 0, 0)
    );
    assert_eq!(
        g.rel.iter().copied().collect::<Vec<_>>(),
        [REL_WHEEL, REL_WHEEL_HI_RES]
    );
    let r: &[u8] = &[0x02, 0x00, 0x40, 0xFF, 0x7F, 0x00];
    let ev = run(&d, &[r, r]);
    assert_eq!(
        ev[0],
        [
            (0, EV_MSC, MSC_SCAN, 0x90002),
            (0, EV_KEY, BTN_RIGHT, 1),
            (0, EV_ABS, ABS_X, 16384),
            (0, EV_ABS, ABS_Y, 32767),
            syn(0)
        ]
    );
    assert!(ev[1].is_empty(), "unchanged absolute values send nothing");
}

#[test]
fn buttons_outside_mouse_and_joystick_applications_follow_the_physical_collection() {
    // Application: Multi-axis Controller. Linux's default case looks at field->physical:
    // Joystick -> BTN_JOYSTICK.., none -> BTN_MISC.. (hidinput_configure_usage, HID_UP_BUTTON).
    let d = desc(
        "05 01 09 08 a1 01
         09 04 a1 00 05 09 19 01 29 02 15 00 25 01 75 01 95 02 81 02 c0
         05 09 19 01 29 02 95 02 81 02 95 04 81 01 c0",
    );
    let g = &InputMap::new(&d).groups[0];
    assert_eq!(
        g.keys.iter().copied().collect::<Vec<_>>(),
        [BTN_0, BTN_1, BTN_TRIGGER, BTN_THUMB]
    );
}
