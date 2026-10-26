//! `send_keys` and `send_mouse` (MC-3, V2-PLAN §6, R8c): input injected into `console::evdev`'s EvdevSink as
//! Linux `input_event`s. Every event this file builds carries `injected = true`; no argument can change that,
//! so an injected key can never trigger a physical-key-only action (R8c/R8f: the console's `physical_only`
//! commands refuse lines typed with the flag, contract ktest `contract.evdev_injected`).
//!
//! Keys are physical key codes (Linux `KEY_*`); text is turned into them with a keyboard layout: `de` (Georg's
//! QWERTZ keyboard, as V1's keymap, AltGr for `{ [ ] } \ ~ @ | €`, no dead keys) or `us`. All of a call's input
//! is checked before the first event goes out (all or nothing, like V1).

use super::{bad, number, opt_str};
use crate::{Json, ToolContext, ToolError, ToolOutput, ToolResult, mcp_tool};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use console::evdev::{EvdevSink, InputEvent};

pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const EV_REL: u16 = 2;
pub const EV_ABS: u16 = 3;
pub const SYN_REPORT: u16 = 0;
pub const REL_X: u16 = 0;
pub const REL_Y: u16 = 1;
pub const REL_HWHEEL: u16 = 6;
pub const REL_WHEEL: u16 = 8;
pub const ABS_X: u16 = 0;
pub const ABS_Y: u16 = 1;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SIDE: u16 = 0x113;
pub const BTN_EXTRA: u16 = 0x114;

pub const KEY_ESC: u16 = 1;
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_TAB: u16 = 15;
pub const KEY_ENTER: u16 = 28;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_SPACE: u16 = 57;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_LEFTMETA: u16 = 125;

/// The evdev device numbers MCP input appears under (one keyboard, one pointer), far from the numbers IN gives
/// real devices (counted from 0). IN-3 may register them as virtual devices (change request in the notes).
pub const INJECT_KEYBOARD: u32 = 0xFFFF_FF00;
pub const INJECT_POINTER: u32 = 0xFFFF_FF01;

/// Bounds on one request (hostile input), not counts of things. `EVENTS_MAX` is what one request may push: three
/// quarters of the input ring (`console::evdev`, 4096 pending events, drop-oldest), so one call can never push
/// its own first events out, and a quarter stays for real input waiting to be read. A full `text` of shifted
/// characters (6 events each, 3000) fits; text plus many modifier-heavy `keys` can exceed it and is refused whole.
pub const TEXT_MAX: usize = 500;
pub const KEYS_MAX: usize = 100;
pub const EVENTS_MAX: usize = 3072;
const NAME_MAX: usize = 40;
pub const MOVE_MAX: i64 = 65_535;
pub const WHEEL_MAX: i64 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    De,
    Us,
}

const SHIFT: u8 = 1;
const ALTGR: u8 = 2;

/// Key code, then the characters it types: (us plain, us shift, de plain, de shift, de AltGr); '\0' = none.
const KEYS: &[(u16, [char; 5])] = &[
    (2, ['1', '!', '1', '!', '\0']),
    (3, ['2', '@', '2', '"', '²']),
    (4, ['3', '#', '3', '§', '³']),
    (5, ['4', '$', '4', '$', '\0']),
    (6, ['5', '%', '5', '%', '\0']),
    (7, ['6', '^', '6', '&', '\0']),
    (8, ['7', '&', '7', '/', '{']),
    (9, ['8', '*', '8', '(', '[']),
    (10, ['9', '(', '9', ')', ']']),
    (11, ['0', ')', '0', '=', '}']),
    (12, ['-', '_', 'ß', '?', '\\']),
    (13, ['=', '+', '´', '`', '\0']),
    (16, ['q', 'Q', 'q', 'Q', '@']),
    (17, ['w', 'W', 'w', 'W', '\0']),
    (18, ['e', 'E', 'e', 'E', '€']),
    (19, ['r', 'R', 'r', 'R', '\0']),
    (20, ['t', 'T', 't', 'T', '\0']),
    (21, ['y', 'Y', 'z', 'Z', '\0']),
    (22, ['u', 'U', 'u', 'U', '\0']),
    (23, ['i', 'I', 'i', 'I', '\0']),
    (24, ['o', 'O', 'o', 'O', '\0']),
    (25, ['p', 'P', 'p', 'P', '\0']),
    (26, ['[', '{', 'ü', 'Ü', '\0']),
    (27, [']', '}', '+', '*', '~']),
    (30, ['a', 'A', 'a', 'A', '\0']),
    (31, ['s', 'S', 's', 'S', '\0']),
    (32, ['d', 'D', 'd', 'D', '\0']),
    (33, ['f', 'F', 'f', 'F', '\0']),
    (34, ['g', 'G', 'g', 'G', '\0']),
    (35, ['h', 'H', 'h', 'H', '\0']),
    (36, ['j', 'J', 'j', 'J', '\0']),
    (37, ['k', 'K', 'k', 'K', '\0']),
    (38, ['l', 'L', 'l', 'L', '\0']),
    (39, [';', ':', 'ö', 'Ö', '\0']),
    (40, ['\'', '"', 'ä', 'Ä', '\0']),
    (41, ['`', '~', '^', '°', '\0']),
    (43, ['\\', '|', '#', '\'', '\0']),
    (44, ['z', 'Z', 'y', 'Y', '\0']),
    (45, ['x', 'X', 'x', 'X', '\0']),
    (46, ['c', 'C', 'c', 'C', '\0']),
    (47, ['v', 'V', 'v', 'V', '\0']),
    (48, ['b', 'B', 'b', 'B', '\0']),
    (49, ['n', 'N', 'n', 'N', '\0']),
    (50, ['m', 'M', 'm', 'M', 'µ']),
    (51, [',', '<', ',', ';', '\0']),
    (52, ['.', '>', '.', ':', '\0']),
    (53, ['/', '?', '-', '_', '\0']),
    (57, [' ', '\0', ' ', '\0', '\0']),
    // KEY_102ND: the extra key left of Y on an ISO (German) keyboard.
    (86, ['\0', '\0', '<', '>', '|']),
];

/// Names for `keys` (lower case). Modifiers can be pressed alone too.
const NAMES: &[(&str, u16)] = &[
    ("esc", 1), ("escape", 1), ("backspace", 14), ("tab", 15), ("enter", 28), ("return", 28), ("space", 57),
    ("ctrl", 29), ("control", 29), ("shift", 42), ("alt", 56), ("altgr", 100), ("meta", 125), ("super", 125), ("win", 125),
    ("rctrl", 97), ("rshift", 54), ("capslock", 58), ("numlock", 69), ("scrolllock", 70),
    ("f1", 59), ("f2", 60), ("f3", 61), ("f4", 62), ("f5", 63), ("f6", 64), ("f7", 65), ("f8", 66), ("f9", 67), ("f10", 68),
    ("f11", 87), ("f12", 88), ("sysrq", 99), ("print", 99), ("printscreen", 99), ("home", 102), ("up", 103),
    ("pageup", 104), ("pgup", 104), ("left", 105), ("right", 106), ("end", 107), ("down", 108), ("pagedown", 109),
    ("pgdn", 109), ("insert", 110), ("delete", 111), ("del", 111), ("pause", 119), ("menu", 127), ("compose", 127),
    ("plus", 0xFFFF), ("minus", 0xFFFE),
];
const MODIFIERS: &[u16] = &[29, 42, 56, 100, 125, 97, 54];

/// One key press with the modifiers held around it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stroke {
    pub mods: Vec<u16>,
    pub key: u16,
}

/// The key and modifiers that type `c` on `layout`; None when the layout has no key for it.
pub fn char_key(layout: Layout, c: char) -> Option<(u16, u8)> {
    match c {
        '\n' | '\r' => return Some((KEY_ENTER, 0)),
        '\t' => return Some((KEY_TAB, 0)),
        '\u{8}' => return Some((KEY_BACKSPACE, 0)),
        '\u{1b}' => return Some((KEY_ESC, 0)),
        // '\0' marks "no character" in the table below.
        '\0' => return None,
        _ => {}
    }
    let cols: &[(usize, u8)] = match layout {
        Layout::Us => &[(0, 0), (1, SHIFT)],
        Layout::De => &[(2, 0), (3, SHIFT), (4, ALTGR)],
    };
    for &(code, chars) in KEYS {
        for &(i, m) in cols {
            if chars[i] == c {
                return Some((code, m));
            }
        }
    }
    None
}

fn stroke(key: u16, m: u8, mut mods: Vec<u16>) -> Stroke {
    if m & SHIFT != 0 && !mods.contains(&KEY_LEFTSHIFT) {
        mods.push(KEY_LEFTSHIFT);
    }
    if m & ALTGR != 0 && !mods.contains(&KEY_RIGHTALT) {
        mods.push(KEY_RIGHTALT);
    }
    Stroke { mods, key }
}

/// `text` as key strokes: every character must exist on the layout. "\r\n" is one Enter.
pub fn text_strokes(layout: Layout, text: &str) -> Result<Vec<Stroke>, ToolError> {
    let n = text.chars().count();
    if n > TEXT_MAX {
        return Err(ToolError::BadArgument(format!("text: {n} characters, at most {TEXT_MAX} per call")));
    }
    let mut out = Vec::with_capacity(n);
    let mut prev_cr = false;
    for c in text.chars() {
        if c == '\n' && prev_cr {
            prev_cr = false;
            continue;
        }
        prev_cr = c == '\r';
        let (key, m) = char_key(layout, c).ok_or_else(|| {
            ToolError::BadArgument(format!("text: {:?} (U+{:04X}) has no key on the {} layout; nothing was sent", c, c as u32, layout_name(layout)))
        })?;
        out.push(stroke(key, m, Vec::new()));
    }
    Ok(out)
}

/// One `keys` entry: "enter", "ctrl+c", "ctrl+alt+f2", "shift+tab", "altgr+q", "ctrl++" (the key '+').
pub fn combo_stroke(layout: Layout, combo: &str) -> Result<Stroke, ToolError> {
    let bad_key = |why: &str| ToolError::BadArgument(format!("keys: {:?} {why}; nothing was sent", combo.chars().take(NAME_MAX).collect::<String>()));
    if combo.is_empty() || combo.len() > NAME_MAX {
        return Err(bad_key("must be 1 to 40 bytes"));
    }
    let (mods_part, key_part) = match combo.strip_suffix("++") {
        Some(m) => (m, "+"),
        None if combo == "+" => ("", "+"),
        None => match combo.rsplit_once('+') {
            Some((m, k)) => (m, k),
            None => ("", combo),
        },
    };
    let mut mods = Vec::new();
    for m in mods_part.split('+').filter(|m| !m.is_empty() || !mods_part.is_empty()) {
        let code = NAMES.iter().find(|(n, _)| n.eq_ignore_ascii_case(m)).map(|&(_, c)| c).filter(|c| MODIFIERS.contains(c));
        let code = code.ok_or_else(|| bad_key("has a part that is no modifier (ctrl, shift, alt, altgr, meta, rctrl, rshift)"))?;
        if !mods.contains(&code) {
            mods.push(code);
        }
    }
    if key_part.is_empty() {
        return Err(bad_key("names no key"));
    }
    let named = NAMES.iter().find(|(n, _)| n.eq_ignore_ascii_case(key_part)).map(|&(_, c)| c);
    let (key, m) = match named {
        Some(0xFFFF) => char_key(layout, '+').ok_or_else(|| bad_key("cannot be typed"))?,
        Some(0xFFFE) => char_key(layout, '-').ok_or_else(|| bad_key("cannot be typed"))?,
        Some(c) => (c, 0),
        None => {
            let mut cs = key_part.chars();
            match (cs.next(), cs.next()) {
                (Some(c), None) => char_key(layout, c).ok_or_else(|| bad_key("is no key on this layout"))?,
                _ => return Err(bad_key("is no key name (enter, esc, tab, f1-f12, up, home, delete, ...) and no single character")),
            }
        }
    };
    Ok(stroke(key, m, mods))
}

fn layout_name(l: Layout) -> &'static str {
    match l {
        Layout::De => "de",
        Layout::Us => "us",
    }
}

fn ev(time_ns: u64, device: u32, kind: u16, code: u16, value: i32) -> InputEvent {
    // R8c: the one place MCP input events are made; the flag is not an argument.
    InputEvent { time_ns, kind, code, value, device, injected: true }
}

/// Events for key strokes: modifiers and key down, report, key and modifiers up, report.
pub fn key_events(strokes: &[Stroke], now: u64) -> Vec<InputEvent> {
    let mut out = Vec::with_capacity(strokes.len() * 6);
    let d = INJECT_KEYBOARD;
    for s in strokes {
        for &m in &s.mods {
            out.push(ev(now, d, EV_KEY, m, 1));
        }
        out.push(ev(now, d, EV_KEY, s.key, 1));
        out.push(ev(now, d, EV_SYN, SYN_REPORT, 0));
        out.push(ev(now, d, EV_KEY, s.key, 0));
        for &m in s.mods.iter().rev() {
            out.push(ev(now, d, EV_KEY, m, 0));
        }
        out.push(ev(now, d, EV_SYN, SYN_REPORT, 0));
    }
    out
}

/// Puts `events` into `sink`; returns how many events the sink dropped while this call pushed (its ring was
/// full). The sink drops the oldest pending events first, so these are events already waiting (from an earlier call
/// or a real device); this call's own go only if real input floods the ring at the same time (`EVENTS_MAX` leaves
/// a quarter of it). The sink keeps one counter, so drops of real input pushed meanwhile count too.
/// Pushes a call's events at a keyboard's pace (`EvdevSink::push_paced`); returns the events the device's
/// readers lost meanwhile (their queues overflowed). The sink's own ring is not counted: nothing reads it in the
/// running kernel, so it is always full (it made every call report all its events as dropped).
pub fn inject(sink: &EvdevSink, events: &[InputEvent]) -> u64 {
    sink.push_paced(events)
}

fn reject_injected_flag(args: &Json) -> Result<(), ToolError> {
    if args.get("injected").is_some() || args.get("device").is_some() {
        return Err(ToolError::BadArgument(String::from("injected: MCP input is always marked injected and comes from MCP's own devices (R8c)")));
    }
    Ok(())
}

/// The events `send_keys` sends for `args` (pure, host-tested).
pub fn send_keys_events(args: &Json, now: u64) -> Result<(Vec<InputEvent>, usize), ToolError> {
    reject_injected_flag(args)?;
    let layout = match opt_str(args, "layout")?.unwrap_or("de") {
        "de" => Layout::De,
        "us" => Layout::Us,
        _ => return Err(bad("layout")),
    };
    let mut strokes = match opt_str(args, "text")? {
        Some(t) => text_strokes(layout, t)?,
        None => Vec::new(),
    };
    match args.get("keys") {
        None | Some(Json::Null) => {}
        Some(Json::Arr(a)) => {
            if a.len() > KEYS_MAX {
                return Err(ToolError::BadArgument(format!("keys: {} entries, at most {KEYS_MAX} per call", a.len())));
            }
            for k in a {
                let k = k.as_str().ok_or_else(|| bad("keys"))?;
                strokes.push(combo_stroke(layout, k.trim())?);
            }
        }
        Some(_) => return Err(bad("keys")),
    }
    if strokes.is_empty() {
        return Err(ToolError::BadArgument(String::from("text or keys: nothing to send")));
    }
    let n = strokes.len();
    // Counted before building: modifiers and key down and up, two reports per stroke.
    let events: usize = strokes.iter().map(|s| 2 * s.mods.len() + 4).sum();
    if events > EVENTS_MAX {
        return Err(ToolError::BadArgument(format!(
            "text or keys: {n} strokes would be {events} input events, at most {EVENTS_MAX} per call (the input ring holds 4096); send it in several calls; nothing was sent"
        )));
    }
    Ok((key_events(&strokes, now), n))
}

/// The events `send_mouse` sends for `args`; `frame` bounds absolute positions (pure, host-tested).
pub fn send_mouse_events(args: &Json, now: u64, frame: Option<(u32, u32)>) -> Result<Vec<InputEvent>, ToolError> {
    reject_injected_flag(args)?;
    let int = |k: &str, lim: i64| -> Result<Option<i64>, ToolError> {
        match args.get(k) {
            None | Some(Json::Null) => Ok(None),
            Some(v) => v.as_i64().filter(|n| n.abs() <= lim).map(Some).ok_or_else(|| bad(k)),
        }
    };
    let (dx, dy) = (int("dx", MOVE_MAX)?, int("dy", MOVE_MAX)?);
    let (x, y) = (number(args, "x")?, number(args, "y")?);
    let (wheel, hwheel) = (int("wheel", WHEEL_MAX)?, int("hwheel", WHEEL_MAX)?);
    let button = match opt_str(args, "button")? {
        None => None,
        Some("left") => Some(BTN_LEFT),
        Some("right") => Some(BTN_RIGHT),
        Some("middle") => Some(BTN_MIDDLE),
        Some("back") => Some(BTN_SIDE),
        Some("forward") => Some(BTN_EXTRA),
        Some(_) => return Err(bad("button")),
    };
    let action = opt_str(args, "action")?;
    if action.is_some() && button.is_none() {
        return Err(ToolError::BadArgument(String::from("action: needs a button")));
    }
    let presses: &[i32] = match action.unwrap_or("click") {
        "click" => &[1, 0],
        "double" => &[1, 0, 1, 0],
        "down" => &[1],
        "up" => &[0],
        _ => return Err(bad("action")),
    };
    if (x.is_some() || y.is_some()) && (dx.is_some() || dy.is_some()) {
        return Err(ToolError::BadArgument(String::from("x: give a position (x, y) or a movement (dx, dy), not both")));
    }
    if x.is_some() != y.is_some() {
        return Err(ToolError::BadArgument(String::from(if x.is_some() { "y: x needs y" } else { "x: y needs x" })));
    }
    let d = INJECT_POINTER;
    let mut out = Vec::new();
    if let (Some(x), Some(y)) = (x, y) {
        let (fw, fh) = frame.map(|(w, h)| (w as u64, h as u64)).unwrap_or((MOVE_MAX as u64 + 1, MOVE_MAX as u64 + 1));
        if x >= fw {
            return Err(ToolError::BadArgument(format!("x: {x} is outside the screen (width {fw})")));
        }
        if y >= fh {
            return Err(ToolError::BadArgument(format!("y: {y} is outside the screen (height {fh})")));
        }
        out.push(ev(now, d, EV_ABS, ABS_X, x as i32));
        out.push(ev(now, d, EV_ABS, ABS_Y, y as i32));
        out.push(ev(now, d, EV_SYN, SYN_REPORT, 0));
    }
    if dx.is_some() || dy.is_some() {
        if let Some(v) = dx.filter(|v| *v != 0) {
            out.push(ev(now, d, EV_REL, REL_X, v as i32));
        }
        if let Some(v) = dy.filter(|v| *v != 0) {
            out.push(ev(now, d, EV_REL, REL_Y, v as i32));
        }
        out.push(ev(now, d, EV_SYN, SYN_REPORT, 0));
    }
    for (code, v) in [(REL_WHEEL, wheel), (REL_HWHEEL, hwheel)] {
        if let Some(v) = v.filter(|v| *v != 0) {
            out.push(ev(now, d, EV_REL, code, v as i32));
            out.push(ev(now, d, EV_SYN, SYN_REPORT, 0));
        }
    }
    if let Some(b) = button {
        for &p in presses {
            out.push(ev(now, d, EV_KEY, b, p));
            out.push(ev(now, d, EV_SYN, SYN_REPORT, 0));
        }
    }
    if out.is_empty() {
        return Err(ToolError::BadArgument(String::from("x, dx, wheel or button: nothing to send")));
    }
    Ok(out)
}

fn report(what: &str, n: usize, events: &[InputEvent], device: u32, dropped: u64) -> String {
    let mut s = format!("sent {what}: {n}, {} input events, all marked injected (R8c), device {device:#x}", events.len());
    if dropped > 0 {
        klog::event!(Warn, "mcp", "input.dropped", events = dropped, device = device; "a reader's input ring overflowed: it did not read in time");
        s.push_str(&format!(
            "; meanwhile readers of the device lost {dropped} events (their rings overflowed: a reader did not read in time)"
        ));
    }
    s
}

fn send_keys(ctx: &ToolContext, args: &Json) -> ToolResult {
    let (events, n) = send_keys_events(args, ctx.now_ns())?;
    let dropped = inject(console::evdev::sink(), &events);
    // Counts only: the text may be something Georg would not want in a log (R8i).
    klog::event!(Debug, "mcp", "input.keys", strokes = n, events = events.len(), dropped = dropped);
    Ok(alloc::vec![ToolOutput::Text(report("key strokes", n, &events, INJECT_KEYBOARD, dropped))])
}

fn send_mouse(ctx: &ToolContext, args: &Json) -> ToolResult {
    let frame = display::with_frame(|f| (f.width, f.height));
    let events = send_mouse_events(args, ctx.now_ns(), frame)?;
    let dropped = inject(console::evdev::sink(), &events);
    klog::event!(Debug, "mcp", "input.mouse", events = events.len(), dropped = dropped);
    Ok(alloc::vec![ToolOutput::Text(report("pointer reports", events.iter().filter(|e| e.kind == EV_SYN).count(), &events, INJECT_POINTER, dropped))])
}

mcp_tool!(
    name: "send_keys",
    owner: "MC",
    safety: Change,
    description: "Types on the machine as a keyboard would: text (characters, a newline is Enter; at most 500) and/or keys (key names and combinations after the text: enter, esc, tab, backspace, f1-f12, up, down, left, right, home, end, pageup, pagedown, insert, delete, ctrl+c, ctrl+alt+f2, shift+tab, altgr+q, ctrl++; at most 100). layout de (default, Georg's keyboard) or us. Every event is marked injected, so it can never trigger a physical-key-only action (R8c). Nothing is sent unless all of it can be typed. Keys go out no faster than the programs reading the keyboard take them (a reader that takes nothing for 250 ms no longer holds the call back); the answer names events a reader lost.",
    schema: r#"{"type":"object","properties":{"text":{"type":"string","description":"characters to type"},"keys":{"type":"array","items":{"type":"string"},"description":"key names or combinations, after the text"},"layout":{"type":"string","enum":["de","us"],"description":"keyboard layout for characters (default de)"}}}"#,
    handler: send_keys,
);

mcp_tool!(
    name: "send_mouse",
    owner: "MC",
    safety: Change,
    description: "Moves and clicks the pointer: a position x, y (screen pixels, as in screen's PNG at scale 1) or a movement dx, dy; wheel / hwheel (notches); button left, right, middle, back or forward with action click (default), double, down or up. Order: move, wheel, button. Every event is marked injected (R8c).",
    schema: r#"{"type":"object","properties":{"x":{"type":"integer","minimum":0},"y":{"type":"integer","minimum":0},"dx":{"type":"integer"},"dy":{"type":"integer"},"wheel":{"type":"integer"},"hwheel":{"type":"integer"},"button":{"type":"string","enum":["left","right","middle","back","forward"]},"action":{"type":"string","enum":["click","double","down","up"]}}}"#,
    handler: send_mouse,
);

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn args(s: &str) -> Json {
        Json::parse(s).unwrap()
    }
    fn keys_of(ev: &[InputEvent]) -> Vec<(u16, i32)> {
        ev.iter().filter(|e| e.kind == EV_KEY).map(|e| (e.code, e.value)).collect()
    }

    #[test]
    fn every_printable_ascii_character_has_a_key_on_both_layouts() {
        for layout in [Layout::De, Layout::Us] {
            for c in ' '..='~' {
                assert!(char_key(layout, c).is_some(), "{c:?} on {layout:?}");
            }
        }
        for c in "äöüÄÖÜß§°€µ²³<>|".chars() {
            assert!(char_key(Layout::De, c).is_some(), "{c:?} on de");
        }
        assert_eq!(char_key(Layout::De, '\0'), None);
        assert!(matches!(send_keys_events(&args(r#"{"text":"a\u0000"}"#), 0), Err(ToolError::BadArgument(_))));
    }

    #[test]
    fn german_layout_braces_and_swapped_y_z() {
        // V1 could not send '{'; on de it is AltGr+7.
        assert_eq!(char_key(Layout::De, '{'), Some((8, ALTGR)));
        assert_eq!(char_key(Layout::De, 'z'), Some((21, 0)));
        assert_eq!(char_key(Layout::Us, 'z'), Some((44, 0)));
        assert_eq!(char_key(Layout::De, '@'), Some((16, ALTGR)));
        assert_eq!(char_key(Layout::De, '|'), Some((86, ALTGR)));
        assert_eq!(char_key(Layout::Us, '{'), Some((26, SHIFT)));
        let (ev, n) = send_keys_events(&args(r#"{"text":"{A"}"#), 5).unwrap();
        assert_eq!(n, 2);
        assert_eq!(keys_of(&ev), vec![(100, 1), (8, 1), (8, 0), (100, 0), (42, 1), (30, 1), (30, 0), (42, 0)]);
        assert!(ev.iter().all(|e| e.injected && e.device == INJECT_KEYBOARD && e.time_ns == 5));
        assert_eq!(ev.iter().filter(|e| e.kind == EV_SYN).count(), 4);
    }

    #[test]
    fn combinations() {
        let s = |c: &str| combo_stroke(Layout::De, c);
        assert_eq!(s("ctrl+c"), Ok(Stroke { mods: vec![29], key: 46 }));
        assert_eq!(s("CTRL+ALT+F2"), Ok(Stroke { mods: vec![29, 56], key: 60 }));
        assert_eq!(s("shift+tab"), Ok(Stroke { mods: vec![42], key: 15 }));
        assert_eq!(s("enter"), Ok(Stroke { mods: vec![], key: 28 }));
        assert_eq!(s("ctrl++"), Ok(Stroke { mods: vec![29], key: 27 }));
        assert_eq!(s("+"), Ok(Stroke { mods: vec![], key: 27 }));
        assert_eq!(s("ctrl+plus"), Ok(Stroke { mods: vec![29], key: 27 }));
        assert_eq!(s("ctrl+minus"), Ok(Stroke { mods: vec![29], key: 53 }));
        assert_eq!(s("ctrl+shift+Z"), Ok(Stroke { mods: vec![29, 42], key: 21 }));
        assert_eq!(s("altgr"), Ok(Stroke { mods: vec![], key: 100 }));
        for bad_combo in ["", "foo", "ctrl+", "hyper+a", "ctrl+foo", "a+b", "ctrl+€€", &"x".repeat(41)] {
            assert!(matches!(s(bad_combo), Err(ToolError::BadArgument(_))), "{bad_combo:?}");
        }
    }

    #[test]
    fn send_keys_arguments() {
        let (ev, n) = send_keys_events(&args(r#"{"text":"ls\r\n","keys":["ctrl+c"],"layout":"us"}"#), 0).unwrap();
        assert_eq!(n, 4);
        assert_eq!(keys_of(&ev), vec![(38, 1), (38, 0), (31, 1), (31, 0), (28, 1), (28, 0), (29, 1), (46, 1), (46, 0), (29, 0)]);
        for a in [
            "{}", r#"{"text":""}"#, r#"{"text":"日本"}"#, r#"{"layout":"fr","text":"a"}"#, r#"{"keys":"enter"}"#, r#"{"keys":[1]}"#,
            r#"{"keys":["nokey"]}"#, r#"{"text":"a","injected":false}"#, r#"{"text":"a","device":0}"#,
        ] {
            assert!(matches!(send_keys_events(&args(a), 0), Err(ToolError::BadArgument(_))), "{a}");
        }
        let long = format!(r#"{{"text":"{}"}}"#, "a".repeat(TEXT_MAX + 1));
        assert!(matches!(send_keys_events(&args(&long), 0), Err(ToolError::BadArgument(_))));
        let many = format!(r#"{{"keys":[{}]}}"#, vec![r#""a""#; KEYS_MAX + 1].join(","));
        assert!(matches!(send_keys_events(&args(&many), 0), Err(ToolError::BadArgument(_))));
        let full = format!(r#"{{"text":"{}"}}"#, "{".repeat(TEXT_MAX));
        let (ev, _) = send_keys_events(&args(&full), 0).unwrap();
        assert!(ev.len() <= EVENTS_MAX && EVENTS_MAX < 4096, "a full text fits the input ring");
        // Full text plus a full keys list with every modifier: 3000 + 100 * 18 events, refused whole.
        let heavy = vec![r#""ctrl+alt+shift+altgr+meta+rctrl+rshift+a""#; KEYS_MAX].join(",");
        let both = format!(r#"{{"text":"{}","keys":[{heavy}]}}"#, "{".repeat(TEXT_MAX));
        match send_keys_events(&args(&both), 0) {
            Err(ToolError::BadArgument(m)) => assert!(m.contains("4800 input events") && m.contains("nothing was sent"), "{m}"),
            other => panic!("{other:?}"),
        }
        let (ev, _) = send_keys_events(&args(&format!(r#"{{"keys":[{heavy}]}}"#)), 0).unwrap();
        assert_eq!(ev.len(), 1800);
        // The count used for the refusal is exactly what key_events builds.
        let s = combo_stroke(Layout::De, "ctrl+alt+shift+altgr+meta+rctrl+rshift+a").unwrap();
        assert_eq!(key_events(core::slice::from_ref(&s), 0).len(), 2 * s.mods.len() + 4);
    }

    #[test]
    fn mouse_arguments() {
        let ev = send_mouse_events(&args(r#"{"x":100,"y":50,"button":"left"}"#), 1, Some((2560, 1440))).unwrap();
        let v: Vec<(u16, u16, i32)> = ev.iter().map(|e| (e.kind, e.code, e.value)).collect();
        assert_eq!(v, vec![(3, 0, 100), (3, 1, 50), (0, 0, 0), (1, 0x110, 1), (0, 0, 0), (1, 0x110, 0), (0, 0, 0)]);
        assert!(ev.iter().all(|e| e.injected && e.device == INJECT_POINTER));
        let ev = send_mouse_events(&args(r#"{"dx":-5,"dy":0,"wheel":-2,"button":"right","action":"double"}"#), 1, None).unwrap();
        let v: Vec<(u16, u16, i32)> = ev.iter().filter(|e| e.kind != EV_SYN).map(|e| (e.kind, e.code, e.value)).collect();
        assert_eq!(v, vec![(2, 0, -5), (2, 8, -2), (1, 0x111, 1), (1, 0x111, 0), (1, 0x111, 1), (1, 0x111, 0)]);
        for a in [
            "{}", r#"{"x":2560,"y":0}"#, r#"{"x":1,"y":1440}"#, r#"{"x":1}"#, r#"{"x":1,"y":1,"dx":1}"#, r#"{"dx":70000}"#, r#"{"wheel":101}"#,
            r#"{"button":"top"}"#, r#"{"action":"click"}"#, r#"{"button":"left","action":"triple"}"#, r#"{"dx":1.5}"#, r#"{"x":-1,"y":0}"#,
            r#"{"button":"left","injected":false}"#,
        ] {
            assert!(matches!(send_mouse_events(&args(a), 0, Some((2560, 1440))), Err(ToolError::BadArgument(_))), "{a}");
        }
    }

    #[test]
    fn injection_reaches_a_sink_with_the_flag() {
        let sink = EvdevSink::new();
        let (ev, _) = send_keys_events(&args(r#"{"keys":["ctrl+alt+shift+d","altgr+q"]}"#), 9).unwrap();
        assert_eq!(inject(&sink, &ev), 0);
        let mut got = Vec::new();
        while let Some(e) = sink.pop() {
            got.push(e);
        }
        assert_eq!(got, ev);
        assert!(got.iter().all(|e| e.injected));
    }
}
