//! Reports to input events as Linux hid-core + hid-input hand them to `input_event()`
//! (`hid_input_fetch_field`, `hid_input_var_field`, `hid_input_array_field`, `hidinput_hid_event`).
//! The input core's own filtering (unchanged keys and axes, zero relative moves, fuzz, empty
//! frames) belongs to the evdev layer that receives these events.

use crate::codes::*;
use crate::descriptor::{Field, ReportDescriptor, ReportKind, flags};
use crate::extract::bits_padded;
use crate::map::InputMap;
use crate::usage_map::{Special, Target};
use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// Index into `InputMap::groups`: which input device the event belongs to.
    pub group: usize,
    pub ev: u16,
    pub code: u16,
    pub value: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputError {
    Empty,
    /// No input report with this ID in the descriptor (Linux drops it too).
    UnknownReport(u8),
}

/// hid-input's table from hat direction 0 (centre) and 1-8 (up, clockwise) to X and Y.
const HAT_TO_AXIS: [(i32, i32); 9] = [
    (0, 0),
    (0, -1),
    (1, -1),
    (1, 0),
    (1, 1),
    (0, 1),
    (-1, 1),
    (-1, 0),
    (-1, -1),
];

const ERROR_ROLL_OVER: u32 = 0x0007_0001;

/// Most VOLUMEUP/VOLUMEDOWN presses one report of a relative volume control gives.
pub const VOLUME_STEPS_MAX: u32 = 64;

#[derive(Clone, Debug, Default)]
struct FieldState {
    /// Last values of the slots that can lie inside a report (Linux `field->value`).
    old: Vec<i32>,
    /// Whether slots past `old` exist; they always read 0 (the report never reaches them).
    tail: bool,
    wheel: Vec<i32>,
}

/// Per-device state between reports.
pub struct InputState {
    numbered: bool,
    fields: Vec<Vec<FieldState>>,
    /// Shadow of the input core's key state per group, for MSC_SCAN.
    keys: Vec<BTreeSet<u16>>,
}

impl InputState {
    /// `max_report_bytes`: the longest report the device can deliver (ID byte included), e.g. the
    /// interrupt endpoint's packet size. Values past it can never be non-zero and keep no state.
    /// The state takes at most 8 slots per byte of `max_report_bytes` for each input report, so the
    /// caller passes what the endpoint really delivers, not a huge "unknown" value.
    pub fn new(desc: &ReportDescriptor, map: &InputMap, max_report_bytes: usize) -> InputState {
        let numbered = desc.numbered(ReportKind::Input);
        let data_bytes = max_report_bytes.saturating_sub(numbered as usize) as u64;
        let data_bits = data_bytes.saturating_mul(8);
        let fields = desc
            .reports
            .iter()
            .map(|r| {
                r.fields
                    .iter()
                    .map(|f| {
                        let reach = slots_within(f, data_bits);
                        let kept = if r.kind == ReportKind::Input {
                            reach
                        } else {
                            0
                        };
                        FieldState {
                            old: vec![0; kept],
                            tail: kept < f.count as usize,
                            wheel: vec![0; kept],
                        }
                    })
                    .collect()
            })
            .collect();
        InputState {
            numbered,
            fields,
            keys: vec![BTreeSet::new(); map.groups.len()],
        }
    }

    /// Turns one report (as the device sent it, ID byte first when the descriptor uses IDs) into
    /// events, a SYN_REPORT closing the events of each group it touched.
    pub fn process(
        &mut self,
        desc: &ReportDescriptor,
        map: &InputMap,
        report: &[u8],
        out: &mut Vec<Event>,
    ) -> Result<(), InputError> {
        let (id, data) = match (self.numbered, report.split_first()) {
            (_, None) => return Err(InputError::Empty),
            (true, Some((&id, rest))) => (id, rest),
            (false, Some(_)) => (0, report),
        };
        let r = desc
            .reports
            .iter()
            .position(|rep| rep.kind == ReportKind::Input && rep.id == id)
            .ok_or(InputError::UnknownReport(id))?;
        // A map or descriptor other than the ones this state was made for gets nothing, not a panic.
        let (Some(group), Some(states)) = (map.group_of(r), self.fields.get_mut(r)) else {
            return Ok(());
        };
        let Some(keys) = self.keys.get_mut(group) else {
            return Ok(());
        };
        let start = out.len();
        for (f, field) in desc.reports[r].fields.iter().enumerate() {
            let Some(state) = states.get_mut(f) else {
                break;
            };
            if field.is_padding() {
                continue;
            }
            let mut ctx = Ctx {
                desc,
                map,
                r,
                f,
                field,
                group,
                keys: &mut *keys,
                out,
            };
            if field.is_variable() {
                ctx.variable(state, data);
            } else {
                ctx.array(state, data);
            }
        }
        if out.len() > start {
            out.push(Event {
                group,
                ev: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            });
        }
        Ok(())
    }
}

/// How many of the field's values start inside `data_bits`.
fn slots_within(f: &Field, data_bits: u64) -> usize {
    // Zero-width values never change: like values past the report, they keep no state.
    if f.size == 0 || f.bit_offset >= data_bits {
        return 0;
    }
    let count = f.count as u64;
    let reach = (data_bits - f.bit_offset).div_ceil(f.size as u64);
    // Bounded by the report length the device can send, so the conversion cannot truncate.
    reach.min(count) as usize
}

struct Ctx<'a> {
    desc: &'a ReportDescriptor,
    map: &'a InputMap,
    r: usize,
    f: usize,
    field: &'a Field,
    group: usize,
    keys: &'a mut BTreeSet<u16>,
    out: &'a mut Vec<Event>,
}

impl Ctx<'_> {
    fn fetch(&self, data: &[u8], n: usize) -> i32 {
        self.field.interpret(bits_padded(
            data,
            self.field.value_offset(n as u32),
            self.field.size,
        ))
    }

    fn variable(&mut self, state: &mut FieldState, data: &[u8]) {
        for n in 0..state.old.len() {
            let value = self.fetch(data, n);
            self.event(state, n as u64, value);
            state.old[n] = value;
        }
    }

    fn array(&mut self, state: &mut FieldState, data: &[u8]) {
        let mut new: Vec<i32> = (0..state.old.len()).map(|n| self.fetch(data, n)).collect();
        if state.tail {
            new.push(self.field.interpret(0));
        }
        let mut old = state.old.clone();
        if state.tail {
            old.push(self.field.interpret(0));
        }
        if new
            .iter()
            .any(|&v| self.valid(v) && self.field.usage(self.index(v)) == Some(ERROR_ROLL_OVER))
        {
            return; // the keyboard lost track: keep the old state, as Linux does
        }
        let (mut old_sorted, mut new_sorted) = (old.clone(), new.clone());
        old_sorted.sort_unstable();
        new_sorted.sort_unstable();
        for n in 0..new.len() {
            if self.valid(old[n]) && new_sorted.binary_search(&old[n]).is_err() {
                self.event(state, self.index(old[n]), 0);
            }
            if self.valid(new[n]) && old_sorted.binary_search(&new[n]).is_err() {
                self.event(state, self.index(new[n]), 1);
            }
        }
        new.truncate(state.old.len());
        state.old = new;
    }

    /// Linux `hid_array_value_is_valid`.
    fn valid(&self, v: i32) -> bool {
        let f = self.field;
        v >= f.logical_min
            && v <= f.logical_max
            && ((v as i64 - f.logical_min as i64) as u64) < f.max_usage()
    }

    fn index(&self, v: i32) -> u64 {
        (v as i64 - self.field.logical_min as i64) as u64
    }

    fn emit(&mut self, ev: u16, code: u16, value: i32) {
        self.out.push(Event {
            group: self.group,
            ev,
            code,
            value,
        });
    }

    /// Linux `hidinput_hid_event` for usage index `n`.
    fn event(&mut self, state: &mut FieldState, n: u64, mut value: i32) {
        let Some(Target { ev, code, special }) = self.map.target(self.desc, self.r, self.f, n)
        else {
            return;
        };
        let field = self.field;
        let hat_dir = match special {
            Special::HatDir(d) => Some(d as i64),
            Special::Hat { min, max } if min < max => {
                Some((value as i64 - min as i64) * 8 / (max as i64 - min as i64 + 1) + 1)
            }
            _ => None,
        };
        if let Some(dir) = hat_dir {
            let (x, y) = HAT_TO_AXIS[if (0..=8).contains(&dir) {
                dir as usize
            } else {
                0
            }];
            self.emit(ev, code, x);
            if code < ABS_MAX {
                self.emit(ev, code + 1, y);
            }
            return;
        }
        if field.is_variable() && field.logical_min < field.logical_max {
            let outside = value < field.logical_min || value > field.logical_max;
            if outside && field.flags & flags::NULL_STATE != 0 {
                return;
            }
            value = value.clamp(field.logical_min, field.logical_max);
        }
        if let Special::HiRes { low } = special {
            if value != 0 {
                // One notch = 120, as Windows and Linux REL_WHEEL_HI_RES count.
                let hi = value.saturating_mul(120);
                let mut scratch = 0;
                let acc = state.wheel.get_mut(n as usize).unwrap_or(&mut scratch);
                *acc = acc.saturating_add(hi);
                let notches = *acc / 120;
                *acc -= notches * 120;
                self.emit(EV_REL, low, notches);
                self.emit(EV_REL, code, hi);
            }
            return;
        }
        if ev == EV_ABS && code == ABS_VOLUME && field.is_relative() {
            // A relative volume control: one VOLUMEUP/VOLUMEDOWN press per step, each in its own frame
            // (hid-input.c:1688-1702). Linux takes any count; a hostile 2^31 would flood the input
            // ring, so one report gives at most VOLUME_STEPS_MAX presses (a bound on hostile input).
            let key = if value > 0 { KEY_VOLUMEUP } else { KEY_VOLUMEDOWN };
            for _ in 0..value.unsigned_abs().min(VOLUME_STEPS_MAX) {
                self.emit(EV_KEY, key, 1);
                self.emit(EV_SYN, SYN_REPORT, 0);
                self.emit(EV_KEY, key, 0);
                self.emit(EV_SYN, SYN_REPORT, 0);
            }
            return;
        }
        let pulse = field.is_relative() || special == Special::Pulse;
        let steady = !pulse && field.flags & flags::BUFFERED_BYTE == 0;
        if steady && field.is_variable() && state.old.get(n as usize).copied().unwrap_or(0) == value
        {
            return;
        }
        if ev == EV_KEY {
            let down = self.keys.contains(&code);
            if (!down) as i32 == value {
                self.emit(EV_MSC, MSC_SCAN, field.usage(n).unwrap_or(0) as i32);
            }
            if value != 2 && down != (value != 0) {
                if down {
                    self.keys.remove(&code);
                } else {
                    self.keys.insert(code);
                }
            }
        }
        self.emit(ev, code, value);
        if pulse && ev == EV_KEY && value != 0 {
            // A relative button: press, frame, release (hid-input simulates the release).
            self.emit(EV_SYN, SYN_REPORT, 0);
            self.emit(ev, code, 0);
            self.keys.remove(&code);
        }
    }
}
