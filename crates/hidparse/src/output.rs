//! Output reports: LED states into the bytes of the output reports that carry them (Linux
//! `hidinput_input_event` -> `hid_set_field` -> `hid_output_report`, linux-6.6/drivers/hid/hid-core.c:1810-1858).

use crate::codes::EV_LED;
use crate::descriptor::{ReportDescriptor, ReportKind};
use crate::map::InputMap;
use alloc::vec;
use alloc::vec::Vec;

/// An output report ready to send: `id` 0 = unnumbered; `bytes` starts with the ID byte when numbered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputReport {
    pub id: u8,
    pub bytes: Vec<u8>,
}

/// Writes the low `bits` (at most 32) bits of `value` at bit `offset`, bit 0 of a byte first (Linux
/// `implement`). Bits past the end of `buf` are dropped.
pub fn put_bits(buf: &mut [u8], offset: u64, bits: u32, value: u32) {
    let bits = bits.min(32);
    for i in 0..bits as u64 {
        let Some(at) = offset.checked_add(i) else { return };
        let Some(byte) = usize::try_from(at / 8).ok().and_then(|b| buf.get_mut(b)) else { return };
        let mask = 1u8 << (at % 8);
        if value >> i & 1 != 0 {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
    }
}

/// Every output report of `group` with an LED field, each LED value from `on(LED code)`, all other values 0.
/// A report longer than a control transfer can carry (64 KiB) is left out: no device can take it.
pub fn led_reports(desc: &ReportDescriptor, map: &InputMap, group: usize, on: impl Fn(u16) -> bool) -> Vec<OutputReport> {
    let numbered = desc.numbered(ReportKind::Output);
    let mut out = Vec::new();
    for (r, report) in desc.reports.iter().enumerate() {
        if report.kind != ReportKind::Output || map.group_of(r) != Some(group) {
            continue;
        }
        let len = report.len_bytes(numbered);
        if len == 0 || len > u16::MAX as u64 {
            continue;
        }
        let head = numbered as usize;
        let mut bytes = vec![0u8; len as usize];
        if numbered {
            bytes[0] = report.id;
        }
        let mut leds = false;
        for (f, field) in report.fields.iter().enumerate() {
            // Zero-width values carry nothing (and could be counted in billions).
            if !field.is_variable() || field.size == 0 {
                continue;
            }
            // `len` bounds count * size, so this loop is at most 2^19 values long.
            for n in 0..field.count as u64 {
                let Some(t) = map.target(desc, r, f, n) else { continue };
                if t.ev != EV_LED {
                    continue;
                }
                leds = true;
                put_bits(&mut bytes[head..], field.value_offset(n as u32), field.size, on(t.code) as u32);
            }
        }
        if leds {
            out.push(OutputReport { id: report.id, bytes });
        }
    }
    out
}
