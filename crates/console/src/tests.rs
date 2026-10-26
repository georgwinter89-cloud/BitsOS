//! Host tests of IN-3: rendered text pixel-exact against an independent model, every German key with Shift and
//! AltGr, the repeat timer, the text model, the line discipline and the monitor's parsing and refusals.

extern crate std;

use crate::font::{Face, Font};
use crate::keymap::{self, Key, Mods, code};
use crate::render;
use crate::repeat::Repeat;
use crate::screen::{self, Cell, Screen};
use crate::tty::Tty;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use display::RamFrame;

// ---- rendering: pixel-exact against a model

/// The model's glyph coverage, from the font data directly (not through `Font::coverage`).
fn model_coverage(face: Face, font: &Font, c: char, x: u32, y: u32) -> u8 {
    use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster};
    let euro: [u8; 16] = [0, 0, 0x3C, 0x66, 0xC0, 0xF8, 0xC0, 0xF8, 0xC0, 0xC0, 0x66, 0x3C, 0, 0, 0, 0];
    let cube: [u8; 16] = [0, 0x70, 0xD8, 0x30, 0x18, 0xD8, 0x70, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let acute: [u8; 16] = [0, 0x0C, 0x18, 0x30, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let own = |c: char| match c {
        '€' => Some(euro),
        '³' => Some(cube),
        '´' => Some(acute),
        _ => None,
    };
    match face {
        Face::Vga => {
            let code = match c {
                'ü' => Some(0x81u8),
                'ä' => Some(0x84),
                'ö' => Some(0x94),
                'Ä' => Some(0x8E),
                'Ö' => Some(0x99),
                'Ü' => Some(0x9A),
                'ß' => Some(0xE1),
                'µ' => Some(0xE6),
                '²' => Some(0xFD),
                '°' => Some(0xF8),
                '§' => Some(0x15),
                '€' | '³' | '´' => None,
                c if (' '..='~').contains(&c) => Some(c as u8),
                _ => Some(b'?'),
            };
            let s = font.scale;
            let bits = match code {
                Some(b) => display::font::glyph(b)[(y / s) as usize],
                None => own(c).unwrap()[(y / s) as usize],
            };
            if bits & (0x80 >> (x / s)) != 0 { 255 } else { 0 }
        }
        Face::Noto => {
            let size = match font.h {
                32 => RasterHeight::Size32,
                24 => RasterHeight::Size24,
                _ => RasterHeight::Size16,
            };
            match get_raster(c, FontWeight::Regular, size) {
                Some(r) => r.raster()[y as usize][x as usize],
                None if own(c).is_some() => {
                    if own(c).unwrap()[(y * 16 / font.h) as usize] & (0x80 >> (x * 8 / font.w)) != 0 { 255 } else { 0 }
                }
                None => get_raster('?', FontWeight::Regular, size).unwrap().raster()[y as usize][x as usize],
            }
        }
    }
}

fn model_blend(fg: u32, bg: u32, a: u8) -> u32 {
    let mix = |s: u32| -> u32 {
        let (f, b) = ((fg >> s) & 255, (bg >> s) & 255);
        // Rounded to nearest: (f*a + b*(255-a)) / 255.
        let num = f * a as u32 + b * (255 - a as u32);
        ((num * 2 + 255) / 510) << s
    };
    mix(0) | mix(8) | mix(16)
}

/// The whole expected picture of a screen showing `lines` (colour attribute per line) with the cursor at `cursor`.
fn model_picture(face: Face, w: u32, h: u32, lines: &[(&str, u8)], cursor: (u32, u32)) -> Vec<u32> {
    let grid: Vec<Vec<(char, u8)>> = lines.iter().map(|(l, a)| l.chars().map(|c| (c, *a)).collect()).collect();
    model_grid(face, w, h, &grid, cursor)
}

/// The expected picture of a grid of (character, attribute) cells.
fn model_grid(face: Face, w: u32, h: u32, grid: &[Vec<(char, u8)>], cursor: (u32, u32)) -> Vec<u32> {
    let font = Font::for_mode(face, w, h);
    let (cols, rows) = (w / font.w, h / font.h);
    let bg0 = screen::PALETTE[0];
    let mut px = vec![bg0; (w * h) as usize];
    for y in 0..rows * font.h {
        for x in 0..cols * font.w {
            let (col, row, fx, fy) = (x / font.w, y / font.h, x % font.w, y % font.h);
            let (ch, attr) = grid.get(row as usize).and_then(|l| l.get(col as usize).copied()).unwrap_or((' ', screen::FG_INDEX));
            let (fg, bg) = (screen::PALETTE[(attr & 15) as usize], screen::PALETTE[(attr >> 4) as usize]);
            let ch_h = (font.h / 8).max(2);
            px[(y * w + x) as usize] = if (col, row) == cursor && fy >= font.h - ch_h {
                fg
            } else {
                model_blend(fg, bg, model_coverage(face, &font, ch, fx, fy))
            };
        }
    }
    px
}

const SAMPLE: &str = "Grüße {@} [a] \\ ~ | € ÄÖÜ äöü ß µ ² ³ ´ ` ^ °§";

fn check_render(face: Face, w: u32, h: u32) {
    let font = Font::for_mode(face, w, h);
    let mut s = Screen::new(w / font.w, h / font.h);
    s.write("bits> echo hi\n");
    s.attr = screen::attr(3, 0);
    s.write(SAMPLE);
    s.attr = screen::DEFAULT_ATTR;
    s.write("\nend");
    let mut f = RamFrame::new(w, h);
    let mut last = None;
    render::render(&mut f, &font, &mut s, &mut last);
    let want = model_picture(face, w, h, &[("bits> echo hi", 7), (SAMPLE, 3), ("end", 7)], (3, 2));
    let got = f.pixels();
    let bad = (0..want.len()).find(|&i| got[i] != want[i]);
    if let Some(i) = bad {
        panic!("{face:?} {w}x{h}: pixel ({}, {}) is {:#08x}, model {:#08x}", i as u32 % w, i as u32 / w, got[i], want[i]);
    }
}

#[test]
fn rendering_is_pixel_exact_vga_and_noto() {
    for face in [Face::Vga, Face::Noto] {
        for (w, h) in [(2560, 1440), (1920, 1080), (1024, 768)] {
            check_render(face, w, h);
        }
    }
}

#[test]
fn fonts_sized_from_the_mode() {
    let v = Font::for_mode(Face::Vga, 2560, 1440);
    assert_eq!((v.w, v.h, 2560 / v.w, 1440 / v.h), (16, 32, 160, 45));
    let n = Font::for_mode(Face::Noto, 2560, 1440);
    assert_eq!((n.w, n.h, 1440 / n.h), (14, 32, 45));
    assert_eq!(Font::for_mode(Face::Noto, 1920, 1080).h, 24);
    assert_eq!(Font::for_mode(Face::Noto, 1024, 768).h, 16);
    assert_eq!(Font::for_mode(Face::Vga, 1024, 768).w, 8);
    // Every character the German keymap types has a glyph in both faces (never the '?' fallback).
    for face in [Face::Vga, Face::Noto] {
        let f = Font::for_mode(face, 2560, 1440);
        let q: Vec<u8> = (0..f.h).flat_map(|y| (0..f.w).map(move |x| (x, y))).map(|(x, y)| f.coverage('?', x, y)).collect();
        for c in "äöüÄÖÜß€@{}[]\\~|µ²³°§^´`<>".chars() {
            let g: Vec<u8> = (0..f.h).flat_map(|y| (0..f.w).map(move |x| (x, y))).map(|(x, y)| f.coverage(c, x, y)).collect();
            assert_ne!(g, q, "{face:?}: {c:?} falls back to '?'");
            assert!(g.iter().any(|&a| a > 0), "{face:?}: {c:?} is empty");
        }
    }
}

#[test]
fn scrolling_moves_pixels_and_equals_a_full_redraw() {
    let (w, h) = (640, 480);
    let font = Font::for_mode(Face::Vga, w, h);
    let mut s = Screen::new(w / font.w, h / font.h);
    let mut f = RamFrame::new(w, h);
    let mut last = None;
    for i in 0..200 {
        s.write(&alloc::format!("line {i} äöü\n"));
        if i % 7 == 0 {
            render::render(&mut f, &font, &mut s, &mut last);
        }
    }
    render::render(&mut f, &font, &mut s, &mut last);
    let mut fresh = RamFrame::new(w, h);
    s.full = true;
    let mut l2 = None;
    render::render(&mut fresh, &font, &mut s, &mut l2);
    assert!(f.pixels() == fresh.pixels(), "incremental scrolling differs from a full redraw");
}

#[test]
fn pointer_is_drawn_over_text_and_restored() {
    let (w, h) = (320, 200);
    let font = Font::for_mode(Face::Vga, w, h);
    let mut s = Screen::new(w / font.w, h / font.h);
    s.write("pointer test");
    let mut f = RamFrame::new(w, h);
    let mut last = None;
    render::render(&mut f, &font, &mut s, &mut last);
    let before = f.pixels().to_vec();
    let mut p = render::Pointer::new();
    p.visible = true;
    p.x = 5;
    p.y = 3;
    p.show(&mut f);
    assert_eq!(f.pixels()[(3 * w + 5) as usize], render::POINTER_OUTLINE);
    assert_eq!(f.pixels()[((3 + 3) * w + 5 + 1) as usize], render::POINTER_FILL);
    p.hide(&mut f);
    assert!(f.pixels() == &before[..], "the pixels under the pointer came back");
    // Clipped at the bottom-right corner: no panic, only the visible part drawn.
    p.x = w - 1;
    p.y = h - 1;
    p.show(&mut f);
    assert_eq!(f.pixels()[((h - 1) * w + w - 1) as usize], render::POINTER_OUTLINE);
    p.hide(&mut f);
    assert!(f.pixels() == &before[..]);
}

// ---- keymap: every German key, plain, Shift, AltGr

/// (code, plain, shift, AltGr) as on Georg's German keyboard (V1 keyboard.asm:658-678 plus the AltGr keys).
const GERMAN: &[(u16, &str, &str, &str)] = &[
    (2, "1", "!", ""), (3, "2", "\"", "²"), (4, "3", "§", "³"), (5, "4", "$", ""), (6, "5", "%", ""),
    (7, "6", "&", ""), (8, "7", "/", "{"), (9, "8", "(", "["), (10, "9", ")", "]"), (11, "0", "=", "}"),
    (12, "ß", "?", "\\"), (13, "´", "`", ""), (16, "q", "Q", "@"), (17, "w", "W", ""), (18, "e", "E", "€"),
    (19, "r", "R", ""), (20, "t", "T", ""), (21, "z", "Z", ""), (22, "u", "U", ""), (23, "i", "I", ""),
    (24, "o", "O", ""), (25, "p", "P", ""), (26, "ü", "Ü", ""), (27, "+", "*", "~"), (30, "a", "A", ""),
    (31, "s", "S", ""), (32, "d", "D", ""), (33, "f", "F", ""), (34, "g", "G", ""), (35, "h", "H", ""),
    (36, "j", "J", ""), (37, "k", "K", ""), (38, "l", "L", ""), (39, "ö", "Ö", ""), (40, "ä", "Ä", ""),
    (41, "^", "°", ""), (43, "#", "'", ""), (44, "y", "Y", ""), (45, "x", "X", ""), (46, "c", "C", ""),
    (47, "v", "V", ""), (48, "b", "B", ""), (49, "n", "N", ""), (50, "m", "M", "µ"), (51, ",", ";", ""),
    (52, ".", ":", ""), (53, "-", "_", ""), (57, " ", " ", ""), (86, "<", ">", "|"),
];

fn ch(k: Key) -> String {
    match k {
        Key::Char(c) => String::from(c),
        _ => String::new(),
    }
}

#[test]
fn every_german_key_plain_shift_altgr() {
    let plain = Mods::default();
    let shift = Mods { shift: true, ..plain };
    let altgr = Mods { altgr: true, ..plain };
    for &(c, p, s, a) in GERMAN {
        assert_eq!(ch(keymap::translate(c, &plain)), p, "key {c} plain");
        assert_eq!(ch(keymap::translate(c, &shift)), s, "key {c} shift");
        assert_eq!(ch(keymap::translate(c, &altgr)), a, "key {c} AltGr");
        // AltGr before Ctrl: Windows/QEMU send AltGr as Ctrl+Alt; Ctrl with right Alt, or Ctrl+left Alt, still
        // types the AltGr character.
        assert_eq!(ch(keymap::translate(c, &Mods { ctrl: true, altgr: true, ..plain })), a, "key {c} Ctrl+AltGr");
        assert_eq!(ch(keymap::translate(c, &Mods { ctrl: true, alt: true, ..plain })), a, "key {c} Ctrl+Alt");
    }
}

#[test]
fn caps_lock_ctrl_keypad_and_specials() {
    let caps = Mods { caps: true, ..Mods::default() };
    assert_eq!(keymap::translate(30, &caps), Key::Char('A'));
    assert_eq!(keymap::translate(40, &caps), Key::Char('Ä'), "umlauts follow Caps Lock");
    assert_eq!(keymap::translate(2, &caps), Key::Char('1'), "digits do not");
    assert_eq!(keymap::translate(12, &caps), Key::Char('ß'), "ß does not (its Shift level is ?)");
    assert_eq!(keymap::translate(12, &Mods { shift: true, ..caps }), Key::Char('?'));
    assert_eq!(keymap::translate(26, &caps), Key::Char('Ü'));
    assert_eq!(keymap::translate(39, &caps), Key::Char('Ö'));
    assert_eq!(keymap::translate(30, &Mods { shift: true, ..caps }), Key::Char('a'), "Shift with Caps Lock: lower case");
    let ctrl = Mods { ctrl: true, ..Mods::default() };
    assert_eq!(keymap::translate(46, &ctrl), Key::Ctrl(3), "Ctrl+C");
    assert_eq!(keymap::translate(32, &ctrl), Key::Ctrl(4), "Ctrl+D");
    assert_eq!(keymap::translate(21, &ctrl), Key::Ctrl(26), "Ctrl+Z is the German Z key");
    assert_eq!(keymap::translate(16, &ctrl), Key::Ctrl(17), "Ctrl+Q without Alt stays Ctrl");
    assert_eq!(keymap::translate(2, &ctrl), Key::None);
    let num = Mods { num: true, ..Mods::default() };
    for (code, d) in [(71, '7'), (72, '8'), (73, '9'), (75, '4'), (76, '5'), (77, '6'), (79, '1'), (80, '2'), (81, '3'), (82, '0'), (83, ',')] {
        assert_eq!(keymap::translate(code, &num), Key::Char(d), "keypad {code} with Num Lock");
    }
    assert_eq!(keymap::translate(72, &Mods::default()), Key::Up, "keypad 8 without Num Lock");
    assert_eq!(keymap::translate(83, &Mods::default()), Key::Delete);
    assert_eq!(keymap::translate(98, &Mods::default()), Key::Char('/'));
    assert_eq!(keymap::translate(96, &Mods::default()), Key::Enter);
    assert_eq!(keymap::translate(code::ENTER, &Mods::default()), Key::Enter);
    assert_eq!(keymap::translate(code::PAGEUP, &Mods::default()), Key::PageUp);
    assert_eq!(keymap::translate(88, &Mods::default()), Key::F(12));
    for m in [code::LEFTSHIFT, code::RIGHTALT, code::CAPSLOCK, code::LEFTCTRL, code::LEFTMETA] {
        assert_eq!(keymap::translate(m, &Mods::default()), Key::None);
        assert!(keymap::is_modifier(m));
    }
}

#[test]
fn reverse_keymap_round_trips() {
    for &(c, p, s, a) in GERMAN {
        for (text, m) in [(p, Mods::default()), (s, Mods { shift: true, ..Mods::default() }), (a, Mods { altgr: true, ..Mods::default() })] {
            let Some(chr) = text.chars().next() else { continue };
            let (code, bits) = keymap::reverse(chr).unwrap_or_else(|| panic!("{chr:?} has no key"));
            let mods = Mods { shift: bits & keymap::SHIFT != 0, altgr: bits & keymap::ALTGR != 0, ..Mods::default() };
            assert_eq!(keymap::translate(code, &mods), Key::Char(chr), "{chr:?} -> key {code} (from key {c} {m:?})");
        }
    }
    assert_eq!(keymap::reverse('\n'), Some((code::ENTER, 0)));
    assert_eq!(keymap::reverse('й'), None);
}

// ---- repeat

#[test]
fn repeat_500_then_33_newest_key_modifiers_never() {
    let mut r = Repeat::new();
    let ms = 1_000_000u64;
    r.press(30, false, 0);
    assert_eq!(r.due(499 * ms), None);
    assert_eq!(r.due(500 * ms), Some(30));
    assert_eq!(r.due(520 * ms), None);
    assert_eq!(r.due(533 * ms), Some(30));
    assert_eq!(r.deadline(), Some(566 * ms));
    r.press(code::LEFTSHIFT, false, 540 * ms);
    assert_eq!(r.key(), Some(30), "a modifier does not stop the repeat");
    r.press(31, false, 550 * ms);
    assert_eq!(r.due(600 * ms), None, "the newest key starts its own delay");
    r.release(30);
    assert_eq!(r.key(), Some(31), "releasing an older key does not stop the newest");
    assert_eq!(r.due(1050 * ms), Some(31));
    r.release(31);
    assert_eq!((r.deadline(), r.due(2000 * ms)), (None, None));
    r.press(30, true, 0);
    assert_eq!(r.due(10_000 * ms), None, "injected keys never repeat");
    r.press(code::CAPSLOCK, false, 0);
    assert_eq!(r.key(), None, "lock keys never repeat");
}

// ---- text model

#[test]
fn screen_wraps_scrolls_and_keeps_scrollback() {
    let mut s = Screen::new(10, 3);
    s.write("0123456789ab");
    assert_eq!(s.text_lines(), ["0123456789", "ab"]);
    s.take_dirty();
    s.write("\nc\nd\n");
    assert_eq!(s.text_lines(), ["c", "d"]);
    assert_eq!(s.scrollback_len(), 2);
    let (_, scrolled, _) = s.take_dirty();
    assert_eq!(scrolled, 2);
    assert!(s.scroll_view(5));
    assert_eq!(s.view, 2, "clamped to the scrollback");
    assert_eq!(s.visible_row(0)[0].ch, '0');
    assert_eq!(s.cursor(), None, "no cursor while looking back");
    s.write("e\n");
    assert_eq!(s.visible_row(0)[0].ch, '0', "the view stays while lines arrive");
    assert!(s.view_to_bottom());
    s.set_limit(1);
    assert_eq!(s.scrollback_len(), 1);
}

#[test]
fn screen_ansi_subset_and_control_characters() {
    let mut s = Screen::new(20, 4);
    s.write("\x1b[31mred\x1b[0m n\x1b[1;32mX\x1b[999;999;999;999;999;999;999;999;999;999;999;999;999;999;999;999;999;999mY");
    let row = s.visible_row(0);
    assert_eq!(row[0].attr & 15, 1);
    assert_eq!(row[4].attr, screen::DEFAULT_ATTR);
    assert_eq!(row[5].attr & 15, 10, "bold green = bright green");
    assert_eq!(s.text_lines()[0], "red nXY", "an over-long sequence is dropped, never printed");
    s.write("\rab\u{8}c\tz");
    assert_eq!(s.text_lines()[0], "acd nXY z");
    s.write("\x1b[2J");
    assert!(s.text_lines().is_empty());
    s.write("\x1b[2;3Hq\x1b[K");
    assert_eq!(s.text_lines(), ["", "  q"]);
    s.write("\u{7}\u{0}\u{7f}");
    assert_eq!(s.text_lines(), ["", "  q"]);
}

#[test]
fn screen_resize_keeps_text_and_cursor_row() {
    let mut s = Screen::new(10, 4);
    s.write("a\nb\nc");
    s.resize(6, 2);
    assert_eq!(s.text_lines(), ["b", "c"]);
    assert_eq!((s.row, s.col), (1, 1));
    s.resize(12, 6);
    assert_eq!(s.text_lines(), ["a", "b", "c"]);
    assert_eq!(s.row, 2);
}

// ---- line discipline and monitor

#[test]
fn tty_cooked_line_editing_and_flags() {
    let t = Tty::new();
    let none = || false;
    for c in "rebootx".chars() {
        t.key(Key::Char(c), c == 'b');
    }
    t.key(Key::Backspace, false);
    t.key(Key::Enter, false);
    let l = t.read_line(0).unwrap();
    assert_eq!((l.text.as_str(), l.injected, l.eof), ("reboot", true, false), "one injected key marks the line");
    t.key(Key::Char('a'), false);
    t.key(Key::Ctrl(21), false);
    assert_eq!(t.editing(), "", "Ctrl+U erases the line");
    t.key(Key::Char('q'), false);
    let n = t.interrupts();
    assert_eq!(t.key(Key::Ctrl(3), false), "^C", "ISIG echoes ^C (the newline is the shell's)");
    assert_eq!(t.interrupts(), n + 1);
    assert_eq!(t.editing(), "", "Ctrl+C flushes the line");
    let mut b = [0u8; 8];
    assert_eq!(t.read(&mut b, true, &none, false), Err(uapi::errno::EAGAIN), "nothing to read after Ctrl+C");
    t.key(Key::Char('h'), false);
    t.key(Key::Ctrl(4), false);
    assert_eq!(t.read(&mut b, true, &none, false), Ok(1), "Ctrl+D hands over the line without a newline");
    t.key(Key::Ctrl(4), false);
    assert_eq!(t.read(&mut b, true, &none, false), Ok(0), "Ctrl+D on an empty line is end of file");
    for _ in 0..crate::tty::BUF_SIZE + 10 {
        t.key(Key::Char('z'), false);
    }
    assert_eq!(t.editing().len(), crate::tty::BUF_SIZE, "a line stops growing at the read buffer (n_tty: the newest byte is replaced)");
    t.key(Key::Enter, false);
    let l = t.read_line(0).unwrap();
    assert_eq!(l.text.len(), crate::tty::BUF_SIZE - 1, "Enter still ends the full line");
    t.flush();
    // Finished lines nobody reads fill the 4096-byte buffer; the rest is dropped; reading makes room again.
    for _ in 0..3000 {
        t.key(Key::Char('l'), false);
        t.key(Key::Enter, false);
    }
    let mut big = alloc::vec![0u8; 3];
    let mut got = 0;
    while t.read(&mut big, true, &none, false).is_ok() {
        got += 1;
    }
    assert_eq!(got, (crate::tty::BUF_SIZE - 1) / 2, "lines kept");
    t.key(Key::Char('m'), false);
    t.key(Key::Enter, false);
    // The byte typed after the buffer filled ('l') is still the start of the line being edited (as n_tty).
    assert_eq!(t.read(&mut big, true, &none, false), Ok(3), "room again after reading");
    assert_eq!(&big[..3], b"lm\n");
}

#[test]
fn tty_injected_mark_is_per_character() {
    let t = Tty::new();
    let line = |t: &Tty| t.read_line(0).unwrap();
    // A stray injected key erased by a typed Backspace leaves a physical line.
    t.key(Key::Char('a'), true);
    t.key(Key::Backspace, false);
    for c in "reboot".chars() {
        t.key(Key::Char(c), false);
    }
    t.key(Key::Enter, false);
    let l = line(&t);
    assert_eq!((l.text.as_str(), l.injected), ("reboot", false), "the erased injected key leaves no mark");
    // The same with Ctrl+U.
    t.key(Key::Char('z'), true);
    t.key(Key::Ctrl(21), false);
    t.key(Key::Char('x'), false);
    t.key(Key::Enter, false);
    assert!(!line(&t).injected, "erased with Ctrl+U: no mark left");
    // An injected Backspace that erased a typed character marks the line ("reboot2" -> "reboot").
    for c in "reboot2".chars() {
        t.key(Key::Char(c), false);
    }
    t.key(Key::Backspace, true);
    t.key(Key::Enter, false);
    let l = line(&t);
    assert_eq!((l.text.as_str(), l.injected), ("reboot", true), "injection changed the line");
    // An injected Enter, an injected character that stays, an injected Ctrl+D hand-over.
    t.key(Key::Char('r'), false);
    t.key(Key::Enter, true);
    assert!(line(&t).injected, "injected Enter");
    t.key(Key::Char('r'), false);
    t.key(Key::Char('s'), true);
    t.key(Key::Enter, false);
    assert!(line(&t).injected, "an injected character in the line");
    t.key(Key::Char('r'), false);
    t.key(Key::Ctrl(4), true);
    assert!(line(&t).injected, "injected Ctrl+D");
    // The next line starts clean.
    t.key(Key::Char('q'), false);
    t.key(Key::Enter, false);
    assert!(!line(&t).injected, "marks reset after each line");
}

#[test]
fn utf8_split_across_writes() {
    use crate::utf8::Carry;
    let mut c = Carry::new();
    let all = "Grüße € 𝄞 ok".as_bytes();
    // Every split point, and byte by byte, gives the same text.
    for cut in 0..=all.len() {
        let mut c = Carry::new();
        let mut s = c.feed(&all[..cut]);
        s.push_str(&c.feed(&all[cut..]));
        assert_eq!(s, "Grüße € 𝄞 ok", "split at {cut}");
        assert!(c.pending().is_empty());
    }
    let mut s = String::new();
    for b in all {
        s.push_str(&c.feed(core::slice::from_ref(b)));
    }
    assert_eq!(s, "Grüße € 𝄞 ok", "byte by byte");
    // Invalid bytes become U+FFFD as from_utf8_lossy does; a broken carried start too.
    assert_eq!(c.feed(b"a\xffb"), "a\u{FFFD}b");
    assert_eq!(c.feed(b"x\xc3"), "x");
    assert_eq!(c.pending(), b"\xc3");
    assert_eq!(c.feed(b"y"), "\u{FFFD}y");
    assert_eq!(c.feed(b"\xe2\x82"), "");
    assert_eq!(c.feed(b"\xac!"), "€!");
    assert_eq!(c.feed(b""), "");
}

#[test]
fn capture_ring_keeps_the_newest_bytes() {
    let mut r = alloc::collections::VecDeque::new();
    crate::capture_into(&mut r, b"abcdef", 8);
    crate::capture_into(&mut r, b"ghij", 8);
    assert_eq!(r.iter().copied().collect::<Vec<u8>>(), b"cdefghij");
    crate::capture_into(&mut r, b"0123456789", 8);
    assert_eq!(r.iter().copied().collect::<Vec<u8>>(), b"23456789", "a write longer than the ring");
    crate::capture_into(&mut r, b"", 8);
    assert_eq!(r.len(), 8);
}

#[test]
fn scrollback_bytes_by_width() {
    let row = 160 * core::mem::size_of::<Cell>() as u64 + core::mem::size_of::<Vec<Cell>>() as u64;
    assert_eq!(crate::vt::scrollback_bytes(4000, 160), 4000 * row);
    assert_eq!(crate::vt::scrollback_bytes(usize::MAX, u32::MAX), u64::MAX, "saturates instead of wrapping");
}

#[test]
fn monitor_splits_and_refuses_injected_physical_commands() {
    assert_eq!(crate::monitor::split(r#"run /bin/x "a b" c"#).unwrap(), ["run", "/bin/x", "a b", "c"]);
    assert_eq!(crate::monitor::split(r#"echo "q\"x""#).unwrap(), ["echo", "q\"x"]);
    assert!(crate::monitor::split("echo \"open").is_err());
    let (ok, out) = crate::monitor::execute("nosuch", false, false);
    assert!(!ok && out.contains("no such command"));
    for cmd in ["reboot", "poweroff"] {
        let (ok, out) = crate::monitor::execute(cmd, true, false);
        assert!(!ok && out.contains("refused"), "{cmd}: {out}");
    }
    let (ok, out) = crate::monitor::execute("echo Grüße  {@}", true, false);
    assert!(ok);
    assert_eq!(out, "Grüße {@}\n");
    let (ok, out) = crate::monitor::execute("help", false, false);
    assert!(ok && out.contains("reboot") && out.contains("log") && out.contains("font"));
    let names: Vec<&str> = crate::monitor::COMMANDS.entries().iter().map(|c| c.name).collect();
    for need in ["help", "log", "mem", "cpus", "net", "reboot", "poweroff", "echo", "clear", "font"] {
        assert!(names.contains(&need), "{need} registered");
    }
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "command names unique");
    let _ = Cell::BLANK;
}

/// The QEMU screendump scenario (`host/tests/in/20-console.sh`): the PPM QEMU wrote equals the model's picture of
/// the expected screen, pixel for pixel. Inputs from the environment: `BITSOS_SCREENDUMP` (P6 PPM),
/// `BITSOS_SCREEN_EXPECT` (one row per line: `attr<TAB>text` segments separated by TABs; first line
/// `face cursor_col cursor_row`). Ignored in normal runs.
#[test]
#[ignore]
fn screendump_matches_model() {
    let ppm = std::fs::read(std::env::var("BITSOS_SCREENDUMP").expect("BITSOS_SCREENDUMP")).expect("read the PPM");
    let spec = std::fs::read_to_string(std::env::var("BITSOS_SCREEN_EXPECT").expect("BITSOS_SCREEN_EXPECT")).expect("read the rows");
    // P6 header: magic, width, height, maxval, one whitespace byte.
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while ppm[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while !ppm[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(String::from_utf8_lossy(&ppm[start..i]).into_owned());
    }
    i += 1;
    assert_eq!(fields[0], "P6");
    let (w, h): (u32, u32) = (fields[1].parse().unwrap(), fields[2].parse().unwrap());
    let rgb = &ppm[i..];
    assert_eq!(rgb.len(), (w * h * 3) as usize, "PPM size");
    let mut lines = spec.lines();
    let head: Vec<&str> = lines.next().expect("header").split_whitespace().collect();
    let face = Face::parse(head[0]).expect("face");
    let cursor = (head[1].parse().unwrap(), head[2].parse().unwrap());
    let grid: Vec<Vec<(char, u8)>> = lines
        .map(|l| {
            let mut row = Vec::new();
            let parts: Vec<&str> = l.split('\t').collect();
            for pair in parts.chunks(2) {
                if let [a, t] = pair {
                    let a: u8 = a.parse().unwrap();
                    row.extend(t.chars().map(|c| (c, a)));
                }
            }
            row
        })
        .collect();
    let want = model_grid(face, w, h, &grid, cursor);
    let mut bad = 0u64;
    let mut first = None;
    for (p, &m) in want.iter().enumerate() {
        let got = (rgb[p * 3] as u32) << 16 | (rgb[p * 3 + 1] as u32) << 8 | rgb[p * 3 + 2] as u32;
        if got != m {
            bad += 1;
            first.get_or_insert((p as u32 % w, p as u32 / w, got, m));
        }
    }
    assert!(bad == 0, "{bad} pixels differ; first at {:?} (got, model)", first.map(|(x, y, g, m)| (x, y, alloc::format!("{g:06x}"), alloc::format!("{m:06x}"))));
    std::println!("screendump {w}x{h} equals the model ({} rows, cursor {:?})", grid.len(), cursor);
}
