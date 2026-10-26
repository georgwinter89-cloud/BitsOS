//! Keyboard page 7 against the real Linux table: `hid_keyboard[256]` of drivers/hid/hid-input.c,
//! read byte for byte out of /usr/src/linux-cachyos/vmlinux (7.2.9-2-cachyos, 14.10.2026; the table
//! is the same as in v6.6: compared entry for entry with linux-6.6/drivers/hid/hid-input.c:27-44 in
//! IN-2a). 240 = KEY_UNKNOWN (`unk`), 0 = usage ignored.

use hidparse::codes::KEY_UNKNOWN;
use hidparse::{InputMap, parse};

#[rustfmt::skip]
const LINUX_HID_KEYBOARD: [u16; 256] = [
    0,0,0,0,30,48,46,32,18,33,34,35,23,36,37,38,50,49,24,25,16,19,31,20,22,47,17,45,21,44,2,3,
    4,5,6,7,8,9,10,11,28,1,14,15,57,12,13,26,27,43,43,39,40,41,51,52,53,58,59,60,61,62,63,64,
    65,66,67,68,87,88,99,70,119,110,102,104,111,107,109,106,105,108,103,69,98,55,74,78,96,79,80,81,75,76,77,71,
    72,73,82,83,86,127,116,117,183,184,185,186,187,188,189,190,191,192,193,194,134,138,130,132,128,129,131,137,133,135,136,113,
    115,114,240,240,240,121,240,89,93,124,92,94,95,240,240,240,122,123,90,91,85,240,240,240,240,240,240,240,111,240,240,240,
    240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,179,180,240,240,240,240,240,240,240,240,
    240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,111,240,240,240,240,240,240,240,
    29,42,56,125,97,54,100,126,164,166,165,163,161,115,114,113,150,158,159,128,136,177,178,176,142,152,173,140,240,240,240,240,
];

#[test]
fn every_keyboard_usage_maps_like_linux() {
    assert_eq!(KEY_UNKNOWN, 240);
    // A key array over usages 0x00-0x1FF of page 7 (0x100 and up: Linux map_key(KEY_UNKNOWN)), in a
    // Keyboard application (without an input application Linux makes no input device at all).
    let d = parse(&[
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0x00, 0x2A, 0xFF, 0x01, 0x15, 0x00, 0x26,
        0xFF, 0x01, 0x75, 0x10, 0x95, 0x01, 0x81, 0x00, 0xC0,
    ])
    .unwrap();
    let map = InputMap::new(&d);
    for (n, &want) in LINUX_HID_KEYBOARD.iter().enumerate() {
        let got = map.target(&d, 0, 0, n as u64).map(|t| t.code);
        let want = (want != 0).then_some(want);
        assert_eq!(got, want, "usage 0x{n:02X}");
    }
    // The first usage above the table takes KEY_UNKNOWN unless an earlier one has it ("unk"
    // entries are map_key_clear, so KEY_UNKNOWN is already taken here and 0x100 is ignored).
    assert_eq!(map.target(&d, 0, 0, 0x100), None);
    let keys = &map.groups[0].keys;
    assert_eq!(
        keys.len(),
        1 + LINUX_HID_KEYBOARD
            .iter()
            .filter(|&&c| c != 0 && c != 240)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    );
}
