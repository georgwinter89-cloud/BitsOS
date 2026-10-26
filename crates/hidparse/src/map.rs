//! A whole device to Linux input devices: `hidinput_connect` with `HID_QUIRK_INPUT_PER_APP` (set by
//! hid-generic), one group (= one evdev node) per application collection usage. Duplicates follow
//! Linux: a "clear" code is taken by every usage, any other code only by the first usage that
//! asks for it in configuration order (input reports, then output reports, fields and usages in
//! order). Only the first-come claims are stored, so the work and memory stay bounded by the code
//! space and the descriptor's length, however many usages a range declares (R1).

use crate::codes::*;
use crate::covered::Covered;
use crate::descriptor::{Field, ReportDescriptor, ReportKind, collection};
use crate::usage_map::{
    GD_GAMEPAD, GD_JOYSTICK, KNOWN_PAGES, REPEAT_PAGES, Raw, Special, Target, map_usage,
};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbsInfo {
    pub min: i32,
    pub max: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

/// The capabilities of one Linux input device (what EVIOCGBIT and EVIOCGABS answer).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Group {
    /// Usage of the application collection (Linux `hidinput->application`).
    pub application: u32,
    pub keys: BTreeSet<u16>,
    pub rel: BTreeSet<u16>,
    pub abs: BTreeMap<u16, AbsInfo>,
    pub leds: BTreeSet<u16>,
    /// EV_REP: the kernel repeats held keys.
    pub repeat: bool,
}

impl Group {
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.rel.is_empty() && self.abs.is_empty() && self.leds.is_empty()
    }

    /// Takes `code` for `ev`; false if a non-clear claim finds it taken.
    fn claim(&mut self, ev: u16, code: u16, clear: bool) -> bool {
        let fresh = match ev {
            EV_KEY => self.keys.insert(code),
            EV_REL => self.rel.insert(code),
            EV_LED => self.leds.insert(code),
            EV_ABS => !self.abs.contains_key(&code),
            _ => false,
        };
        clear || fresh
    }
}

const GD_X: u32 = 0x0001_0030;
const GD_KEYBOARD: u32 = 0x0001_0006;
const GD_SYSTEM_CONTROL: u32 = 0x0001_0080;
const CP_CONSUMER_CONTROL: u32 = 0x000C_0001;

/// Linux `IS_INPUT_APPLICATION` (include/linux/hid.h:900-904).
pub fn is_input_application(a: u32) -> bool {
    (0x0001_0000..=0x0001_0008).contains(&a)
        || (0x000D_0001..=0x000D_0006).contains(&a)
        || matches!(a, GD_SYSTEM_CONTROL | CP_CONSUMER_CONTROL | 0x0001_000C)
}
const GD_RZ: u32 = 0x0001_0035;

/// Everything of a field that `map_usage` and the axis parameters look at: two fields with the
/// same key map every usage the same way.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct MapKey {
    kind: ReportKind,
    flags: u32,
    size: u32,
    counted: bool,
    logical: (i32, i32),
    physical: (i32, i32),
    unit: (u32, i32),
    application: u32,
    physical_usage: u32,
    innermost: Option<(u8, u32)>,
}

impl MapKey {
    fn of(f: &Field) -> MapKey {
        MapKey {
            kind: f.kind,
            flags: f.flags,
            size: f.size,
            counted: f.count > 0,
            logical: (f.logical_min, f.logical_max),
            physical: (f.physical_min, f.physical_max),
            unit: (f.unit, f.unit_exponent),
            application: f.application,
            physical_usage: f.physical,
            innermost: f.innermost,
        }
    }
}

/// Usages already configured, per group and field key.
type Seen = BTreeMap<(usize, MapKey), Covered>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputMap {
    pub groups: Vec<Group>,
    /// Group of each report of the descriptor (by index into `reports`), None for unmapped ones.
    report_group: Vec<Option<usize>>,
    /// Usages that won a non-clear code: (report, field, usage index).
    won: BTreeSet<(usize, usize, u64)>,
}

impl InputMap {
    pub fn new(desc: &ReportDescriptor) -> InputMap {
        let mut map = InputMap {
            report_group: alloc::vec![None; desc.reports.len()],
            ..InputMap::default()
        };
        let mut seen = Seen::new();
        // hidinput_connect (hid-input.c:2183-2193): no input device at all unless an application or
        // physical collection is one of the input applications.
        if !desc.collections.iter().any(|c| {
            matches!(c.kind, collection::APPLICATION | collection::PHYSICAL) && is_input_application(c.usage)
        }) {
            return map;
        }
        let apps = desc
            .collections
            .iter()
            .filter(|c| c.kind == collection::APPLICATION)
            .count();
        for kind in [ReportKind::Input, ReportKind::Output] {
            for (r, report) in desc.reports.iter().enumerate() {
                if report.kind != kind || report.fields.iter().all(Field::is_padding) {
                    continue;
                }
                // One input device unless there are several applications; System Control and
                // Consumer Control join the keyboard (hidinput_match_application, hid-input.c:2187-2206).
                let joins_keyboard = matches!(report.application, GD_SYSTEM_CONTROL | CP_CONSUMER_CONTROL);
                let found = map.groups.iter().position(|g| {
                    apps <= 1
                        || g.application == report.application
                        || (joins_keyboard && g.application == GD_KEYBOARD)
                });
                let g = found.unwrap_or_else(|| {
                    map.groups.push(Group {
                        application: report.application,
                        ..Group::default()
                    });
                    map.groups.len() - 1
                });
                map.report_group[r] = Some(g);
                for (f, field) in report.fields.iter().enumerate() {
                    map.configure(&mut seen, g, r, f, field);
                }
            }
        }
        map
    }

    pub fn group_of(&self, report: usize) -> Option<usize> {
        self.report_group.get(report).copied().flatten()
    }

    /// Where usage index `n` of field `f` in report `r` goes, None if it is ignored.
    pub fn target(&self, desc: &ReportDescriptor, r: usize, f: usize, n: u64) -> Option<Target> {
        self.group_of(r)?;
        let field = desc.reports.get(r)?.fields.get(f)?;
        match map_usage(field, field.usage(n)?) {
            Raw::Map {
                target,
                clear: true,
            } => Some(target),
            Raw::Map {
                target,
                clear: false,
            } if self.won.contains(&(r, f, n)) => Some(target),
            _ => None,
        }
    }

    fn configure(&mut self, seen: &mut Seen, g: usize, r: usize, f: usize, field: &Field) {
        // A usage declared again in this field, or by an earlier field that maps the same way in
        // the same group, cannot win a code it lost or take one it holds: only the first counts.
        // Clear absolute X..Rz are the exception, the last field sets their range (as Linux).
        let mut done = Covered::default();
        let same = seen.entry((g, MapKey::of(field))).or_default();
        let mut todo: Vec<(u64, u32)> = Vec::new();
        for (s, span) in field.usages.spans().iter().enumerate() {
            let start = field.usages.span_start(s);
            // `Usages::push` keeps a run below 2^32, so `last` is a valid usage.
            let last = (span.first as u64 + span.count - 1) as u32;
            let (lo_page, hi_page) = (span.first >> 16, last >> 16);
            let configured =
                field.kind == ReportKind::Input && !field.is_constant() && field.count > 0;
            if configured
                && REPEAT_PAGES
                    .iter()
                    .any(|&p| (lo_page..=hi_page).contains(&p))
            {
                self.groups[g].repeat = true;
            }
            let index = |usage: u32| (start + (usage - span.first) as u64, usage);
            for (a, b) in done.add(span.first, last) {
                for (c, d) in same.add(a, b) {
                    todo.extend(candidates(c, d as u64).into_iter().map(index));
                }
                if !field.is_relative() {
                    todo.extend((a.max(GD_X)..=b.min(GD_RZ)).map(index));
                }
            }
        }
        todo.sort_unstable();
        todo.dedup();
        for (n, usage) in todo {
            self.claim(g, r, f, field, n, usage);
        }
    }

    fn claim(&mut self, g: usize, r: usize, f: usize, field: &Field, n: u64, usage: u32) {
        let Raw::Map { target, clear } = map_usage(field, usage) else {
            return;
        };
        let group = &mut self.groups[g];
        if let Special::HiRes { low } = target.special {
            group.rel.insert(low);
        }
        if !group.claim(target.ev, target.code, clear) {
            return;
        }
        if !clear {
            self.won.insert((r, f, n));
        }
        if target.ev != EV_ABS {
            return;
        }
        if target.code == ABS_VOLUME && field.is_relative() {
            // Turned into key presses by `InputState` (hid-input.c:1442-1446).
            group.keys.insert(KEY_VOLUMEUP);
            group.keys.insert(KEY_VOLUMEDOWN);
        }
        if matches!(target.special, Special::Hat { .. } | Special::HatDir(_)) {
            let hat = AbsInfo {
                min: -1,
                max: 1,
                fuzz: 0,
                flat: 0,
                resolution: 0,
            };
            group.abs.insert(target.code, hat);
            if target.code < ABS_MAX {
                group.abs.insert(target.code + 1, hat);
            }
            return;
        }
        let (a, b) = (field.logical_min, field.logical_max);
        let (fuzz, flat) = if matches!(field.application, GD_GAMEPAD | GD_JOYSTICK) {
            let span = b.wrapping_sub(a);
            (span >> 8, span >> 4)
        } else {
            (0, 0)
        };
        let resolution = abs_resolution(field, target.code);
        group.abs.insert(
            target.code,
            AbsInfo {
                min: a,
                max: b,
                fuzz,
                flat,
                resolution,
            },
        );
    }
}

/// The usages of `first..=last` worth configuring one by one, in order: every id up to the
/// one-by-one limit of a known page, the first id above it, and the first usage of an unknown page
/// (all later ones of the same default would be duplicates).
fn candidates(first: u32, last: u64) -> Vec<u32> {
    let mut out = Vec::new();
    let (lo_page, hi_page) = (first >> 16, (last >> 16) as u32);
    for &(page, limit) in KNOWN_PAGES {
        if page < lo_page || page > hi_page {
            continue;
        }
        let lo = if page == lo_page { first & 0xFFFF } else { 0 };
        let hi = if page == hi_page {
            (last & 0xFFFF) as u32
        } else {
            0xFFFF
        };
        for id in lo..=hi.min(limit) {
            out.push(page << 16 | id);
        }
        if hi > limit {
            out.push(page << 16 | lo.max(limit + 1));
        }
    }
    let mut page = lo_page;
    while page <= hi_page {
        if KNOWN_PAGES.iter().all(|&(p, _)| p != page) {
            out.push(if page == lo_page { first } else { page << 16 });
            break;
        }
        if page == u32::MAX >> 16 {
            break;
        }
        page += 1;
    }
    out.sort_unstable();
    out
}

/// Linux `hidinput_calc_abs_res`: units per millimetre, or per radian for rotations; 0 = unknown.
fn abs_resolution(field: &Field, code: u16) -> i32 {
    let mut exponent = field.unit_exponent;
    let mut logical = match field.logical_max.checked_sub(field.logical_min) {
        Some(v) if v > 0 => v,
        _ => return 0,
    };
    let mut physical = match field.physical_max.checked_sub(field.physical_min) {
        Some(v) if v > 0 => v,
        _ => return 0,
    };
    match code {
        ABS_X | ABS_Y | ABS_Z => match field.unit {
            0x11 => exponent = exponent.saturating_add(1), // centimetres
            0x13 => {
                // inches
                let Some(p) = physical.checked_mul(254) else {
                    return 0;
                };
                physical = p;
                exponent = exponent.saturating_sub(1);
            }
            _ => return 0,
        },
        ABS_RX | ABS_RY | ABS_RZ | ABS_WHEEL => match field.unit {
            0x14 => {
                // degrees
                let Some(l) = logical.checked_mul(573) else {
                    return 0;
                };
                logical = l;
                exponent = exponent.saturating_add(1);
            }
            0x12 => {}
            _ => return 0,
        },
        _ => return 0,
    }
    while exponent < 0 {
        let Some(l) = logical.checked_mul(10) else {
            return 0;
        };
        logical = l;
        exponent += 1;
    }
    while exponent > 0 {
        let Some(p) = physical.checked_mul(10) else {
            return 0;
        };
        physical = p;
        exponent -= 1;
    }
    ((logical as i64 + physical as i64 / 2) / physical as i64) as i32
}
