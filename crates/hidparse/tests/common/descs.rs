//! Descriptors from V1's tests (usbredir-pad.mjs, hidparse-test.mjs), as bytes.

use super::json::hex;

fn h(s: &str) -> Vec<u8> {
    hex(&s.split_whitespace().collect::<String>())
}

/// Georg's DragonRise 0079:0006 (usbredir-pad.mjs DRAGONRISE).
pub fn dragonrise() -> Vec<u8> {
    h(
        "05010904a101a10275089505150026ff00350046ff0009300931093209320935
       8102750495012507463b0165140939814265007501950c250145010509190129
       0c81020600ff750195082501450109018102c0a1027508950746ff0026ff0009
       029102c0c0",
    )
}

/// A Retrolink-style SNES pad (0079:0011 clones): X four times, then Y (hidparse-test.mjs).
pub fn snes_clone() -> Vec<u8> {
    h("05 01 09 04 a1 01 a1 02
       75 08 95 05 15 00 26 ff 00 35 00 46 ff 00 09 30 09 30 09 30 09 30 09 31 81 02
       75 04 95 01 25 07 46 3b 01 65 14 09 00 81 42
       65 00 75 01 95 0a 25 01 45 01 05 09 19 01 29 0a 81 02
       06 00 ff 75 01 95 0a 25 01 45 01 09 01 81 02 c0
       a1 02 75 08 95 04 46 ff 00 26 ff 00 09 02 91 02 c0 c0")
}

/// DualShock 4 input report 1, first reports (hidparse-test.mjs DS4).
pub fn dualshock4() -> Vec<u8> {
    h("05 01 09 05 a1 01 85 01
       09 30 09 31 09 32 09 35 15 00 26 ff 00 75 08 95 04 81 02
       09 39 15 00 25 07 35 00 46 3b 01 65 14 75 04 95 01 81 42
       65 00 05 09 19 01 29 0e 15 00 25 01 75 01 95 0e 81 02
       06 00 ff 09 20 75 06 95 01 15 00 25 7f 81 02
       05 01 09 33 09 34 15 00 26 ff 00 75 08 95 02 81 02
       06 00 ff 09 21 95 36 81 02
       85 05 09 22 95 1f 91 02 85 04 09 23 95 24 b1 02 c0")
}

/// A Sony DualShock 3 (054C:0268) as it describes itself, 148 bytes (usbredir-pad.mjs DUALSHOCK3): report ID 1,
/// 49 bytes, 19 buttons from byte 2, the sticks in bytes 6-9, 19 pressure bytes and four 16-bit motion values
/// on Generic Desktop Pointer, feature reports 2, 0xEE, 0xEF.
pub fn dualshock3() -> Vec<u8> {
    h("05 01 09 04 a1 01 a1 02 85 01 75 08 95 01 15 00 26 ff 00 81 03
       75 01 95 13 15 00 25 01 35 00 45 01 05 09 19 01 29 13 81 02 75 01 95 0d 06 00 ff 81 03
       15 00 26 ff 00 05 01 09 01 a1 00 75 08 95 04 35 00 46 ff 00 09 30 09 31 09 32 09 35 81 02 c0
       05 01 95 13 09 01 81 02 95 0c 81 01 75 10 95 04 26 ff 03 46 ff 03 09 01 81 02 c0
       a1 02 85 02 75 08 95 30 09 01 b1 02 c0 a1 02 85 ee 75 08 95 30 09 01 b1 02 c0
       a1 02 85 ef 75 08 95 30 09 01 b1 02 c0 c0")
}

/// A keyboard (report ID 1) and a gamepad (report ID 2) in one interface (hidparse-test.mjs).
pub fn keyboard_and_pad() -> Vec<u8> {
    h("05 01 09 06 a1 01 85 01 05 07 19 e0 29 e7 15 00 25 01 75 01 95 08 81 02 95 06 75 08 15 00 25 65 19 00 29 65 81 00 c0
       05 01 09 05 a1 01 85 02 05 09 19 01 29 0a 15 00 25 01 75 01 95 0a 81 02 75 06 95 01 81 03
       05 01 09 30 09 31 15 00 26 ff 00 75 08 95 02 81 02 c0")
}
