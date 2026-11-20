//! GM-6p, L7-HW: the batteries of a HID device as Linux hid-input reads them
//! (linux-7.3.1/drivers/hid/hid-input.c:542 `hidinput_setup_battery`, :665 `hidinput_update_battery`, :651
//! `hidinput_update_battery_charge_status`).
//!
//! Linux keeps one battery per report ID: the first battery strength usage of a report creates it and sets its
//! range, later ones of the same report only update it. The strength usages are Generic Device Controls Battery
//! Strength (0x06/0x20) and Battery System Absolute State Of Charge (0x85/0x65, a percentage: its range counts
//! as 0..100 whatever the descriptor says); Battery System Charging (0x85/0x44) is the charge status of the
//! battery of its own report and is dropped in a report without one. A value of 0 is a real 0 % (Linux drops it
//! only on the Digitizer page, which V2 does not map). Like Linux, only Input reports count, constant fields are
//! skipped, and a device without any input application has no battery (`hidinput_connect` gives up before it
//! looks at usages). Not done (V2 has no use for them yet): the digitizer's battery (page 0x0D is not mapped),
//! the battery in a Feature report read with GET_REPORT, Linux's quirk table.

use crate::descriptor::{Field, ReportDescriptor, ReportKind, collection};
use crate::extract;
use crate::map::is_input_application;
use alloc::vec::Vec;

const BATTERY_STRENGTH: u32 = 0x0006_0020;
const STATE_OF_CHARGE: u32 = 0x0085_0065;
const CHARGING: u32 = 0x0085_0044;

/// The first value of `f` whose usage is `u`, found from the usage runs, so a field of any width costs as much
/// as its runs (R1). Past the declared usages the last one repeats (Linux `hid_add_field`). Only the first value
/// per usage and field counts: two values of one field with the same battery usage would say the same.
fn first_value(f: &Field, u: u32) -> Option<u32> {
    let usages = &f.usages;
    let mut best: Option<u64> = None;
    for (i, span) in usages.spans().iter().enumerate() {
        if u >= span.first && ((u - span.first) as u64) < span.count {
            let n = usages.span_start(i) + (u - span.first) as u64;
            best = Some(best.map_or(n, |b| b.min(n)));
        }
    }
    if best.is_none() && usages.len() < f.count as u64 && usages.get(usages.len().saturating_sub(1)) == Some(u) {
        best = Some(usages.len());
    }
    best.filter(|&n| n < f.count as u64).map(|n| n as u32)
}

/// One value of a report that carries a battery usage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spot {
    pub report_id: u8,
    /// Bits after the ID byte.
    pub bit_offset: u64,
    pub size: u32,
    /// Sign-extended (the field's logical minimum is below 0).
    pub signed: bool,
}

impl Spot {
    fn of(f: &Field, n: u32) -> Spot {
        Spot { report_id: f.report_id, bit_offset: f.value_offset(n), size: f.size, signed: f.logical_min < 0 }
    }

    /// The value in `data` (the report after its ID byte); None when its bits lie past the end.
    fn read(&self, data: &[u8]) -> Option<i32> {
        let raw = extract::bits(data, self.bit_offset, self.size).ok()?;
        Some(if self.signed { extract::sign_extend(raw, self.size) } else { raw as i32 })
    }
}

/// What a report said about the battery.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reading {
    /// The capacity in percent, scaled from the range; None when the report has none or Linux drops it
    /// (outside the range).
    pub capacity: Option<i32>,
    pub charging: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatteryMap {
    /// Linux `bat->report_id`: the report this battery lives in (0 when the device numbers no reports).
    pub report_id: u8,
    /// Linux `bat->min`, `bat->max`, from the report's first strength field.
    pub min: i32,
    pub max: i32,
    pub strength: Vec<Spot>,
    pub charging: Vec<Spot>,
    numbered: bool,
}

impl BatteryMap {
    /// The batteries of `desc` in Linux's order (reports in the order they first appear, `list_add_tail`); empty
    /// when no input report has a strength usage (a charging usage alone gives none either).
    pub fn find_all(desc: &ReportDescriptor) -> Vec<BatteryMap> {
        let inputs = desc
            .collections
            .iter()
            .any(|c| matches!(c.kind, collection::APPLICATION | collection::PHYSICAL) && is_input_application(c.usage));
        if !inputs {
            return Vec::new();
        }
        let numbered = desc.numbered(ReportKind::Input);
        let mut out = Vec::new();
        for report in desc.reports_of(ReportKind::Input) {
            let mut range = None;
            let (mut strength, mut charging) = (Vec::new(), Vec::new());
            for f in &report.fields {
                if f.is_constant() || f.is_padding() || !f.is_variable() {
                    continue;
                }
                let mut found: Vec<(u32, u32)> = [BATTERY_STRENGTH, STATE_OF_CHARGE, CHARGING]
                    .into_iter()
                    .filter_map(|u| first_value(f, u).map(|n| (n, u)))
                    .collect();
                found.sort_unstable();
                for (n, u) in found {
                    if u == CHARGING {
                        charging.push(Spot::of(f, n));
                    } else {
                        range.get_or_insert(if u == STATE_OF_CHARGE { (0, 100) } else { (f.logical_min, f.logical_max) });
                        strength.push(Spot::of(f, n));
                    }
                }
            }
            if let Some((min, max)) = range {
                out.push(BatteryMap { report_id: report.id, min, max, strength, charging, numbered });
            }
        }
        out
    }

    /// Linux `hidinput_update_battery`'s range check, then `hidinput_scale_battery_capacity`.
    pub fn capacity(&self, value: i32) -> Option<i32> {
        if value < self.min || value > self.max {
            return None;
        }
        if self.min >= self.max {
            return Some(value);
        }
        let pct = (value as i64 - self.min as i64) * 100 / (self.max as i64 - self.min as i64);
        Some(pct as i32)
    }

    /// What one input report as the device sent it (ID byte first when numbered) says about this battery;
    /// nothing for another report ID.
    pub fn read(&self, report: &[u8]) -> Reading {
        let (id, data) = match (self.numbered, report.split_first()) {
            (_, None) => return Reading::default(),
            (true, Some((&id, rest))) => (id, rest),
            (false, Some(_)) => (0, report),
        };
        let mut r = Reading::default();
        if id != self.report_id {
            return r;
        }
        for s in &self.strength {
            if let Some(c) = s.read(data).and_then(|v| self.capacity(v)) {
                r.capacity = Some(c);
            }
        }
        for s in &self.charging {
            if let Some(v) = s.read(data) {
                r.charging = Some(v != 0);
            }
        }
        r
    }
}
