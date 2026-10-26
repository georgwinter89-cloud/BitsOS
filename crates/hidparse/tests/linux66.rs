//! Behaviour checked against the Linux 6.6 source in IN-2a (linux-6.6/drivers/hid/hid-input.c and
//! include/linux/hid.h), for the points IN-1 had written from memory or left out.

mod common;

use hidparse::codes::*;
use hidparse::{Event, InputMap, InputState, ReportDescriptor, led_reports, parse, put_bits};

fn desc(s: &str) -> ReportDescriptor {
    parse(&common::json::hex(&s.split_whitespace().collect::<String>())).unwrap()
}

fn events(d: &ReportDescriptor, reports: &[&[u8]]) -> Vec<Vec<(usize, u16, u16, i32)>> {
    let map = InputMap::new(d);
    let mut state = InputState::new(d, &map, 64);
    reports
        .iter()
        .map(|r| {
            let mut out: Vec<Event> = Vec::new();
            state.process(d, &map, r, &mut out).unwrap();
            out.into_iter().map(|e| (e.group, e.ev, e.code, e.value)).collect()
        })
        .collect()
}

fn syn() -> (usize, u16, u16, i32) {
    (0, EV_SYN, SYN_REPORT, 0)
}

#[test]
fn without_an_input_application_there_is_no_input_device() {
    // hid-input.c:2183-2193: a vendor collection only (and no collection at all) gives nothing.
    let vendor = desc("06 00 ff 09 01 a1 01 15 00 26 ff 00 75 08 95 08 09 01 81 02 c0");
    assert!(InputMap::new(&vendor).groups.is_empty());
    let bare = desc("05 07 19 04 29 05 15 00 25 01 75 01 95 02 81 02");
    assert!(InputMap::new(&bare).groups.is_empty());
    // A physical Pointer collection is enough (IS_INPUT_APPLICATION also looks at physical ones).
    let phys = desc("05 01 09 01 a1 00 05 09 19 01 29 01 15 00 25 01 75 01 95 01 81 02 95 07 81 01 c0");
    assert_eq!(InputMap::new(&phys).groups.len(), 1);
}

#[test]
fn system_and_consumer_control_join_the_keyboard_but_not_a_mouse() {
    // Keyboard, Mouse, System Control, Consumer Control in one interface (a receiver's mouse
    // interface often looks like this): three input devices, the two controls on the keyboard.
    let d = desc(
        "05 01 09 06 a1 01 85 01 05 07 19 04 29 05 15 00 25 01 75 01 95 02 81 02 95 06 81 01 c0
         05 01 09 02 a1 01 85 02 09 01 a1 00 05 09 19 01 29 02 75 01 95 02 81 02 95 06 81 01 c0 c0
         05 01 09 80 a1 01 85 03 19 81 29 83 15 00 25 01 75 01 95 03 81 02 95 05 81 01 c0
         05 0c 09 01 a1 01 85 04 15 00 26 ff 03 19 00 2a ff 03 75 10 95 01 81 00 c0",
    );
    let map = InputMap::new(&d);
    let apps: Vec<u32> = map.groups.iter().map(|g| g.application).collect();
    assert_eq!(apps, [0x10006, 0x10002]);
    assert!(map.groups[0].keys.contains(&KEY_POWER) && map.groups[0].keys.contains(&KEY_VOLUMEUP));
    // Mouse first: the controls get their own devices (only a keyboard takes them in).
    let d = desc(
        "05 01 09 02 a1 01 85 02 09 01 a1 00 05 09 19 01 29 02 75 01 95 02 81 02 95 06 81 01 c0 c0
         05 01 09 80 a1 01 85 03 19 81 29 83 15 00 25 01 75 01 95 03 81 02 95 05 81 01 c0",
    );
    let apps: Vec<u32> = InputMap::new(&d).groups.iter().map(|g| g.application).collect();
    assert_eq!(apps, [0x10002, 0x10080]);
}

#[test]
fn system_control_ignores_lazy_usages_and_a0_has_only_micmute() {
    // A System Control application declaring X and Y (hid-input.c:850-857) and usages 0xA9/0xAA.
    let d = desc(
        "05 01 09 80 a1 01 09 81 09 30 09 31 09 a9 09 aa 15 00 25 01 75 01 95 05 81 02 95 03 81 01 c0",
    );
    let g = &InputMap::new(&d).groups[0];
    assert!(g.abs.is_empty() && g.rel.is_empty(), "no axes from System Control");
    assert_eq!(g.keys.iter().copied().collect::<Vec<_>>(), [KEY_POWER, KEY_MICMUTE]);
}

#[test]
fn consumer_table_is_linux_66() {
    let d = desc("05 0c 09 01 a1 01 15 00 26 ff 03 19 00 2a ff 03 75 10 95 01 81 00 c0");
    let map = InputMap::new(&d);
    let code = |u: u64| map.target(&d, 0, 0, u).map(|t| t.code);
    for (u, k) in [
        (0x035, KEY_KBDILLUMTOGGLE),
        (0x036, BTN_MISC),
        (0x065, KEY_CAMERA),
        (0x084, KEY_ENTER),
        (0x0E5, KEY_BASSBOOST),
        (0x1CB, KEY_ASSISTANT),
        (0x22D, KEY_ZOOMIN),
        (0x26A, KEY_DELETE),
        (0x29F, KEY_SCALE),
        (0x2CC, KEY_KBDINPUTASSIST_CANCEL),
        (0x123, KEY_UNKNOWN),
    ] {
        assert_eq!(code(u), Some(k), "usage {u:#x}");
    }
    assert_eq!(code(0), None);
}

#[test]
fn relative_volume_knob_presses_volume_keys() {
    // Consumer Volume (0xE0) as a relative 8-bit value (hid-input.c:1442-1446, 1688-1702).
    let d = desc("05 0c 09 01 a1 01 09 e0 15 81 25 7f 75 08 95 01 81 06 c0");
    let g = &InputMap::new(&d).groups[0];
    assert!(g.abs.contains_key(&ABS_VOLUME) && g.keys.contains(&KEY_VOLUMEUP) && g.keys.contains(&KEY_VOLUMEDOWN));
    // The report's closing SYN_REPORT follows an empty frame here; the input core drops it (evdev layer).
    let ev = events(&d, &[&[2], &[0xFF], &[0x81]]);
    let press = |k| [(0, EV_KEY, k, 1), syn(), (0, EV_KEY, k, 0), syn()];
    assert_eq!(ev[0], [&press(KEY_VOLUMEUP)[..], &press(KEY_VOLUMEUP), &[syn()]].concat());
    assert_eq!(ev[1], [&press(KEY_VOLUMEDOWN)[..], &[syn()]].concat());
    assert_eq!(ev[2].len(), 4 * 64 + 1, "-127 steps cut at VOLUME_STEPS_MAX");
    // An absolute volume control is an axis.
    let d = desc("05 0c 09 01 a1 01 09 e0 15 00 25 64 75 08 95 01 81 02 c0");
    assert_eq!(events(&d, &[&[40]])[0], [(0, EV_ABS, ABS_VOLUME, 40), syn()]);
}

#[test]
fn rfkill_button_is_pressed_and_released_in_one_report() {
    // Wireless Radio Controls with the RF-kill button (hid-input.c:903-911).
    let d = desc("05 01 09 0c a1 01 09 c6 15 00 25 01 75 01 95 01 81 02 95 07 81 01 c0");
    let ev = events(&d, &[&[1], &[1], &[0]]);
    let pulse = [(0, EV_MSC, MSC_SCAN, 0x100C6), (0, EV_KEY, KEY_RFKILL, 1), syn(), (0, EV_KEY, KEY_RFKILL, 0), syn()];
    // (the second SYN_REPORT closes the report, as hid-input's input_sync)
    assert_eq!(ev[0], pulse);
    assert_eq!(ev[1], pulse, "held again: another press (the field counts as relative)");
    // Released: hid-input hands the 0 on (the input core drops it, the key is not down).
    assert_eq!(ev[2], [(0, EV_KEY, KEY_RFKILL, 0), syn()]);
}

#[test]
fn programmable_buttons_become_macro_keys() {
    // Consumer Control > Named Array "Programmable Buttons" with Buttons 1-32 (hid-input.c:776-786).
    let d = desc(
        "05 0c 09 01 a1 01 09 03 a1 04 05 09 19 01 29 20 15 00 25 01 75 01 95 20 81 02 c0 c0",
    );
    let g = &InputMap::new(&d).groups[0];
    assert!(g.keys.contains(&KEY_MACRO1) && g.keys.contains(&KEY_MACRO30));
    assert!(g.keys.contains(&BTN_TRIGGER_HAPPY1) && g.keys.contains(&BTN_TRIGGER_HAPPY2));
    // The same buttons outside the Named Array are BTN_MISC + n.
    let d = desc("05 0c 09 01 a1 01 05 09 19 01 29 02 15 00 25 01 75 01 95 02 81 02 95 06 81 01 c0");
    assert_eq!(InputMap::new(&d).groups[0].keys.iter().copied().collect::<Vec<_>>(), [BTN_0, BTN_1]);
}

#[test]
fn pages_linux_handles_differently_from_in1() {
    // Page 0 is ignored; Power (0x84) takes the unknown default; Battery state of charge and
    // charging are ignored (EV_PWR), its other usages unknown; Camera; Telephony A-D; HP keys.
    let d = desc(
        "05 01 09 06 a1 01 15 00 25 01 75 01
         95 01 0b 05 00 00 00 81 02
         95 01 0b 01 00 84 00 81 02
         95 01 0b 65 00 85 00 81 02
         95 01 0b 44 00 85 00 81 02
         95 01 0b 66 00 85 00 81 02
         95 01 0b 21 00 90 00 81 02
         95 01 0b bc 00 0b 00 81 02
         95 01 0b 70 00 7f ff 81 02
         95 01 0b 04 00 01 ff 81 02
         c0",
    );
    let map = InputMap::new(&d);
    let target = |f: usize| map.target(&d, 0, f, 0).map(|t| (t.ev, t.code));
    assert_eq!(target(0), None, "page 0");
    assert_eq!(target(1), Some((EV_KEY, BTN_MISC)), "Power page: unknown default");
    assert_eq!(target(2), None, "battery state of charge");
    assert_eq!(target(3), None, "battery charging");
    assert_eq!(target(4), None, "another battery usage: unknown default, BTN_MISC taken by field 1");
    assert_eq!(target(5), Some((EV_KEY, KEY_CAMERA)));
    assert_eq!(target(6), Some((EV_KEY, KEY_NUMERIC_A)));
    assert_eq!(target(7), Some((EV_KEY, KEY_HP)));
    assert_eq!(target(8), Some((EV_KEY, KEY_BRIGHTNESSUP)));
}

#[test]
fn led_output_reports() {
    // Boot keyboard LEDs (5 bits + 3 padding) and a numbered variant with a vendor output after them.
    let d = hidparse::boot::boot_keyboard();
    let map = InputMap::new(&d);
    let r = led_reports(&d, &map, 0, |c| c == LED_CAPSL || c == LED_SCROLLL);
    assert_eq!(r.len(), 1);
    assert_eq!((r[0].id, r[0].bytes.clone()), (0, vec![0b0000_0110]));
    let d = desc(
        "05 01 09 06 a1 01 85 05 05 08 19 01 29 03 15 00 25 01 75 01 95 03 91 02 95 05 91 01
         06 00 ff 09 01 75 08 95 01 91 02 c0",
    );
    let map = InputMap::new(&d);
    let r = led_reports(&d, &map, 0, |c| c == LED_NUML);
    assert_eq!((r[0].id, r[0].bytes.clone()), (5, vec![5, 0b001, 0]));
    // No LEDs in the group: nothing to send.
    let mouse = hidparse::boot::boot_mouse();
    assert!(led_reports(&mouse, &InputMap::new(&mouse), 0, |_| true).is_empty());
    let mut b = [0xFFu8; 3];
    put_bits(&mut b, 4, 12, 0x0A5);
    assert_eq!(b, [0x5F, 0x0A, 0xFF]);
    put_bits(&mut b, 20, 8, 0xFF); // past the end: dropped, no panic
    put_bits(&mut b, u64::MAX - 2, 32, 1);
}
