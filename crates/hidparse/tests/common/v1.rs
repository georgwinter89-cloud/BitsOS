//! V1's pad rules (usb/hidparse.asm, hidparse-model.mjs, gamepad-model.mjs) re-expressed over V2's
//! parsed layout, so V2's parser and extraction can be compared with V1's answers field by field.

use super::json::Json;
use hidparse::descriptor::flags;
use hidparse::extract::{bits, sign_extend};
use hidparse::item::{ItemType, items};
use hidparse::{ReportDescriptor, ReportKind};

/// V1's PAD_LAYOUT, by the names of hidparse-model.mjs `layoutFields`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pad {
    pub id: u32,
    pub bytes: u32,
    pub x_at: u32,
    pub x_bits: u32,
    pub x_min: i32,
    pub x_max: i32,
    pub y_at: u32,
    pub y_bits: u32,
    pub y_min: i32,
    pub y_max: i32,
    pub hat_at: u32,
    pub hat_bits: u32,
    pub hat_min: i32,
    pub hat_max: i32,
    pub buttons: u32,
    pub btn_at: u32,
    pub btn_first: u32,
}

impl Pad {
    pub fn from_json(j: &Json) -> Option<Pad> {
        if j.is_null() {
            return None;
        }
        let n = |k: &str| j.get(k).num() as i64;
        Some(Pad {
            id: n("id") as u32,
            bytes: n("bytes") as u32,
            x_at: n("xAt") as u32,
            x_bits: n("xBits") as u32,
            x_min: n("xMin") as i32,
            x_max: n("xMax") as i32,
            y_at: n("yAt") as u32,
            y_bits: n("yBits") as u32,
            y_min: n("yMin") as i32,
            y_max: n("yMax") as i32,
            hat_at: n("hatAt") as u32,
            hat_bits: n("hatBits") as u32,
            hat_min: n("hatMin") as i32,
            hat_max: n("hatMax") as i32,
            buttons: n("buttons") as u32,
            btn_at: n("btnAt") as u32,
            btn_first: n("btnFirst") as u32,
        })
    }
}

const BUTTON_RUN_MAX: u32 = 16; // V1's PAD_LAYOUT keeps at most 16 buttons
const V1_PUSH_MAX: usize = 4;

/// What V1's hid_parse would answer, computed from V2's layout. `report_bytes`: V1's XR_REPORT.
pub fn v1_view(desc: &ReportDescriptor, report_bytes: u64) -> Option<Pad> {
    if desc.max_push_depth > V1_PUSH_MAX {
        return None;
    }
    let pad = desc.first_gamepad()?;
    let cols = &desc.collections;
    let inputs: Vec<_> = desc
        .fields_in_order()
        .into_iter()
        .filter(|f| f.kind == ReportKind::Input)
        .collect();
    let in_ids = inputs.iter().any(|f| f.report_id != 0);
    let data_var = |f: &hidparse::Field| f.flags & 3 == flags::VARIABLE;
    let usage = |f: &hidparse::Field, i: u64| f.usage(i).unwrap_or(0);
    let btn_id = inputs.iter().find_map(|f| {
        let ok = f.is_inside(cols, pad) && f.size == 1 && data_var(f);
        let n = (f.count as u64).min(report_bytes * 8);
        (ok && (0..n).any(|i| (0x90000..=0x900FF).contains(&usage(f, i)))).then_some(f.report_id)
    })?;
    if btn_id == 0 && in_ids {
        return None;
    }
    let id_bits: u64 = if btn_id != 0 { 8 } else { 0 };
    let mut l = Pad {
        id: btn_id as u32,
        ..Pad::default()
    };
    let (mut run, mut run_next) = (0, 0u32);
    let mut end_all = id_bits;
    for f in inputs.iter().filter(|f| f.report_id == btn_id) {
        if f.count == 0 || f.size == 0 {
            continue;
        }
        if f.size > 32 {
            return None;
        }
        let mut pos = id_bits + f.bit_offset;
        let end = pos + f.count as u64 * f.size as u64;
        if end > report_bytes * 8 {
            return None;
        }
        end_all = end;
        if !f.is_inside(cols, pad) || !data_var(f) {
            if run == 1 {
                run = 2;
            }
            continue;
        }
        let (min, max) = (f.logical_min, f.logical_max);
        for i in 0..f.count as u64 {
            let u = usage(f, i);
            if f.flags & flags::RELATIVE == 0 {
                if u == 0x10030 && max > min {
                    (l.x_at, l.x_bits, l.x_min, l.x_max) = (pos as u32, f.size, min, max);
                }
                if u == 0x10031 && max > min {
                    (l.y_at, l.y_bits, l.y_min, l.y_max) = (pos as u32, f.size, min, max);
                }
                let range = max as i64 - min as i64;
                if u == 0x10039 && l.hat_bits == 0 && (range == 7 || range == 3) {
                    (l.hat_at, l.hat_bits, l.hat_min, l.hat_max) = (pos as u32, f.size, min, max);
                }
            }
            let button = f.size == 1 && u >> 16 == 9;
            if run == 1 {
                if button && u == run_next && l.buttons < BUTTON_RUN_MAX {
                    l.buttons += 1;
                    run_next = run_next.wrapping_add(1);
                } else {
                    run = 2;
                }
            } else if run == 0 && button && u <= 0x900FF {
                run = 1;
                (l.btn_at, l.btn_first, l.buttons) = (pos as u32, u & 0xFF, 1);
                run_next = u.wrapping_add(1);
            }
            pos += f.size as u64;
        }
    }
    if l.buttons == 0 {
        return None;
    }
    l.bytes = end_all.div_ceil(8) as u32;
    Some(l)
}

/// Where V1 (hidparse-model) and V2 (Linux hid-core) read a descriptor differently on purpose:
/// such descriptors are not compared (the reasons are counted and listed in the notes).
pub fn differences(desc: &[u8]) -> Vec<&'static str> {
    #[derive(Clone, Copy, Default)]
    struct G {
        page: u32,
        lmin: i32,
        lmax_u: u32,
        lmax_s: i32,
        lmax_linux: i32,
    }
    let mut out = Vec::new();
    let (mut g, mut stack) = (G::default(), Vec::new());
    let (mut pages_used, mut branch, mut min_size, mut mixed): (Vec<u32>, u32, Option<u8>, bool) =
        (Vec::new(), 0, None, false);
    for it in items(desc) {
        let Ok(it) = it else { break };
        match it.kind {
            ItemType::Global => match it.tag {
                0 => g.page = it.data,
                1 => g.lmin = it.sdata(),
                2 => {
                    (g.lmax_u, g.lmax_s) = (it.data, it.sdata());
                    g.lmax_linux = if g.lmin < 0 {
                        it.sdata()
                    } else {
                        it.data as i32
                    };
                }
                10 => {
                    stack.push(g);
                    if stack.len() > V1_PUSH_MAX {
                        out.push("Push deeper than 4 (V1 refuses, V2 has no limit)");
                    }
                }
                11 => g = stack.pop().unwrap_or(g),
                _ => {}
            },
            ItemType::Local => {
                if it.tag == 10 {
                    branch += (it.data != 0) as u32;
                    continue;
                }
                if branch > 1 {
                    continue;
                }
                if matches!(it.tag, 0 | 2) && it.size != 4 {
                    pages_used.push(g.page & 0xFFFF);
                }
                if it.tag == 1 {
                    min_size = Some(it.size);
                }
                if it.tag == 2 && (it.size == 4) != (min_size == Some(4)) {
                    mixed = true;
                }
            }
            // Only Input and Collection items feed V1's answer.
            ItemType::Main if matches!(it.tag, 8 | 10) => {
                // Linux re-pages the usages after the last one already in the main item's page;
                // V1 pages them all. They differ only if an earlier one is in another page.
                let page = g.page & 0xFFFF;
                let last_in_page = pages_used.iter().rposition(|&p| p == page);
                if last_in_page.is_some_and(|k| pages_used[..k].iter().any(|&p| p != page)) {
                    out.push("Usage Page changes between usages of one main item");
                }
                if mixed {
                    out.push("Usage Minimum and Maximum of different widths");
                }
                let v1_max = if g.lmin < 0 {
                    g.lmax_s
                } else {
                    g.lmax_u as i32
                };
                if it.tag == 8 && v1_max != g.lmax_linux {
                    out.push(
                        "Logical Maximum's sign read at the main item (V1) vs at the item (Linux)",
                    );
                }
                (pages_used, branch, min_size, mixed) = (Vec::new(), 0, None, false);
            }
            ItemType::Main => (pages_used, branch, min_size, mixed) = (Vec::new(), 0, None, false),
            _ => {}
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// gamepad-model.mjs `model()`: the pad_state bits V1 leaves for a report, but every field read
/// with V2's `extract::bits` and its sign rule. None = the report changes nothing.
pub fn pad_bits(l: &Pad, r: &[u8]) -> Option<u32> {
    const B: u32 = 0x8000;
    const Y: u32 = 0x4000;
    const SELECT: u32 = 0x2000;
    const START: u32 = 0x1000;
    const UP: u32 = 0x800;
    const DOWN: u32 = 0x400;
    const LEFT: u32 = 0x200;
    const RIGHT: u32 = 0x100;
    const A: u32 = 0x80;
    const X: u32 = 0x40;
    const L: u32 = 0x20;
    const R: u32 = 0x10;
    const DEFAULT: [u32; 16] = [X, A, B, Y, L, R, L, R, SELECT, START, 0, 0, 0, 0, 0, 0];
    const HAT8: [u32; 8] = [
        UP,
        UP | RIGHT,
        RIGHT,
        DOWN | RIGHT,
        DOWN,
        DOWN | LEFT,
        LEFT,
        UP | LEFT,
    ];
    const HAT4: [u32; 4] = [UP, RIGHT, DOWN, LEFT];
    if r.is_empty() || (r.len() as u32) < l.bytes || (l.id != 0 && r[0] as u32 != l.id) {
        return None;
    }
    let read = |at: u32, n: u32, min: i32| -> Option<i64> {
        if n == 0 {
            return None;
        }
        let raw = bits(r, at as u64, n).ok()?;
        Some(if min < 0 {
            sign_extend(raw, n) as i64
        } else {
            raw as i64
        })
    };
    let mut out = 0;
    if l.buttons > 0 {
        let count = l.buttons.min(16);
        if let Some(v) = read(l.btn_at, count, 0) {
            for i in 0..count {
                if v >> i & 1 != 0 {
                    out |= (l.btn_first + i)
                        .checked_sub(1)
                        .and_then(|k| DEFAULT.get(k as usize))
                        .copied()
                        .unwrap_or(0);
                }
            }
        }
    }
    let axis = |at, n, min: i32, max: i32, low, high| match read(at, n, min) {
        Some(v) if v >= min as i64 && v <= max as i64 && max > min => {
            let q = (max as i64 - min as i64) >> 2;
            if v - min as i64 <= q {
                low
            } else if max as i64 - v <= q {
                high
            } else {
                0
            }
        }
        _ => 0,
    };
    out |= axis(l.x_at, l.x_bits, l.x_min, l.x_max, LEFT, RIGHT);
    out |= axis(l.y_at, l.y_bits, l.y_min, l.y_max, UP, DOWN);
    if let Some(v) = read(l.hat_at, l.hat_bits, l.hat_min) {
        let way = v - l.hat_min as i64;
        let range = l.hat_max as i64 - l.hat_min as i64;
        out |= match range {
            7 if (0..=7).contains(&way) => HAT8[way as usize],
            3 if (0..=3).contains(&way) => HAT4[way as usize],
            _ => 0,
        };
    }
    Some(out)
}
