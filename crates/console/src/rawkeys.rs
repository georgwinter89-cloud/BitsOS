//! What the VT sends to the terminal in the raw keyboard modes (IN-4, KDSKBMODE; linux-6.6/drivers/tty/vt/
//! keyboard.c `kbd_keycode` and `emulate_raw`): K_MEDIUMRAW sends the key code with bit 7 set on release
//! (codes above 127 as three bytes), K_RAW the PC/AT set-1 scancodes an AT keyboard would send (the
//! `x86_keycodes` table: the scancode, 0x100 = an 0xE0 prefix).

const KEY_PAUSE: u16 = 119;
const KEY_HANGEUL: u16 = 122;
const KEY_HANJA: u16 = 123;
const KEY_SYSRQ: u16 = 99;

/// `x86_keycodes[256]` (keyboard.c:1290-1305): key code -> set-1 scancode; codes 240..=255 are 0 there.
#[rustfmt::skip]
const X86_KEYCODES: [u16; 240] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
    16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
    32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47,
    48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63,
    64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79,
    80, 81, 82, 83, 84, 118, 86, 87, 88, 115, 120, 119, 121, 112, 123, 92,
    284, 285, 309, 0, 312, 91, 327, 328, 329, 331, 333, 335, 336, 337, 338, 339,
    367, 288, 302, 304, 350, 89, 334, 326, 267, 126, 268, 269, 125, 347, 348, 349,
    360, 261, 262, 263, 268, 376, 100, 101, 321, 316, 373, 286, 289, 102, 351, 355,
    103, 104, 105, 275, 287, 279, 258, 106, 274, 107, 294, 364, 358, 363, 362, 361,
    291, 108, 381, 281, 290, 272, 292, 305, 280, 99, 112, 257, 306, 359, 113, 114,
    264, 117, 271, 374, 379, 265, 266, 93, 94, 95, 85, 259, 375, 260, 90, 116,
    377, 109, 111, 277, 278, 282, 283, 295, 296, 297, 299, 300, 301, 293, 303, 307,
    308, 310, 313, 314, 315, 317, 318, 319, 320, 357, 322, 323, 324, 325, 276, 330,
    332, 340, 365, 342, 343, 344, 345, 346, 356, 270, 341, 368, 369, 370, 371, 372,
];

/// K_MEDIUMRAW bytes for one key edge.
pub fn mediumraw(code: u16, down: bool, out: &mut [u8; 8]) -> usize {
    let up: u8 = if down { 0 } else { 0x80 };
    if code < 128 {
        out[0] = code as u8 | up;
        1
    } else {
        out[0] = up;
        out[1] = (code >> 7) as u8 | 0x80;
        out[2] = code as u8 | 0x80;
        3
    }
}

/// K_RAW bytes for one key edge (`emulate_raw`); 0 when the key has no scancode. `alt`: an Alt key is held
/// (SysRq is then 0x54).
pub fn raw(code: u16, down: bool, alt: bool, out: &mut [u8; 8]) -> usize {
    let up: u8 = if down { 0 } else { 0x80 };
    let bytes: &[u8] = match code {
        KEY_PAUSE => &[0xE1, 0x1D | up, 0x45 | up],
        KEY_HANGEUL if down => &[0xF2],
        KEY_HANJA if down => &[0xF1],
        KEY_HANGEUL | KEY_HANJA => &[],
        KEY_SYSRQ if alt => &[0x54 | up],
        KEY_SYSRQ => &[0xE0, 0x2A | up, 0xE0, 0x37 | up],
        c => {
            let sc = X86_KEYCODES.get(c as usize).copied().unwrap_or(0);
            if sc == 0 {
                return 0;
            }
            let n = if sc & 0x100 != 0 {
                out[0] = 0xE0;
                out[1] = (sc & 0x7F) as u8 | up;
                2
            } else {
                out[0] = (sc & 0x7F) as u8 | up;
                1
            };
            return n;
        }
    };
    out[..bytes.len()].copy_from_slice(bytes);
    bytes.len()
}
