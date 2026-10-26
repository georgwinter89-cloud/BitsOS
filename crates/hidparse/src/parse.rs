//! The descriptor parser: Linux `hid_open_report` semantics (drivers/hid/hid-core.c, v6.6) without
//! its fixed caps (R1), plus V1's leniency for long items, unknown global tags and odd delimiters.

use crate::descriptor::{Collection, Field, Report, ReportDescriptor, ReportKind, collection};
use crate::item::{Item, ItemType, items};
use crate::usages::{UsageSpan, Usages};
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// An item runs past the end of the descriptor.
    Truncated,
    /// Report ID 0 or above 255.
    BadReportId(u32),
    /// Pop without Push.
    PopWithoutPush,
    /// End Collection without an open collection.
    EndWithoutCollection,
    /// The descriptor ends inside a collection.
    UnclosedCollection,
    /// A report's bit length does not fit 64 bits.
    ReportTooLong,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// Byte offset of the item at fault (the descriptor length for UnclosedCollection).
    pub offset: usize,
    pub kind: ParseErrorKind,
}

#[derive(Clone, Copy, Default)]
struct Globals {
    usage_page: u32,
    logical_min: i32,
    logical_max: i32,
    physical_min: i32,
    physical_max: i32,
    unit_exponent: i32,
    unit: u32,
    report_size: u32,
    report_id: u32,
    report_count: u32,
}

#[derive(Default)]
struct Locals {
    usages: Usages,
    usage_min: u32,
    delimiter_branch: u32,
}

/// An open collection with the usages of the nearest enclosing application, physical and logical
/// collections (itself included), so a field finds them without walking the stack.
#[derive(Clone, Copy)]
struct Open {
    index: usize,
    application: u32,
    physical: u32,
    logical: u32,
}

struct Parser {
    out: ReportDescriptor,
    globals: Globals,
    stack: Vec<Globals>,
    locals: Locals,
    open: Vec<Open>,
    seq: usize,
}

/// Parses a report descriptor. Never panics; time and memory are linear in the descriptor's length
/// (plus the report lookup per main item, bounded by the 3 * 255 possible reports).
pub fn parse(desc: &[u8]) -> Result<ReportDescriptor, ParseError> {
    let mut p = Parser {
        out: ReportDescriptor::default(),
        globals: Globals::default(),
        stack: Vec::new(),
        locals: Locals::default(),
        open: Vec::new(),
        seq: 0,
    };
    for item in items(desc) {
        let item = item.map_err(|offset| ParseError {
            offset,
            kind: ParseErrorKind::Truncated,
        })?;
        let fail = |kind| ParseError {
            offset: item.offset,
            kind,
        };
        match item.kind {
            ItemType::Main => p.main(&item).map_err(fail)?,
            ItemType::Global => p.global(&item).map_err(fail)?,
            ItemType::Local => p.local(&item),
            ItemType::Reserved | ItemType::Long => {}
        }
    }
    if !p.open.is_empty() {
        return Err(ParseError {
            offset: desc.len(),
            kind: ParseErrorKind::UnclosedCollection,
        });
    }
    Ok(p.out)
}

impl Parser {
    fn global(&mut self, it: &Item) -> Result<(), ParseErrorKind> {
        let g = &mut self.globals;
        match it.tag {
            0 => g.usage_page = it.data,
            1 => g.logical_min = it.sdata(),
            // Linux decides the maximum's sign by the minimum declared so far (15 00 25 FF = 0..255).
            2 => {
                g.logical_max = if g.logical_min < 0 {
                    it.sdata()
                } else {
                    it.data as i32
                }
            }
            3 => g.physical_min = it.sdata(),
            4 => {
                g.physical_max = if g.physical_min < 0 {
                    it.sdata()
                } else {
                    it.data as i32
                }
            }
            5 => {
                // Many devices write the exponent as a 4-bit two's complement nibble (Linux hid-core).
                let raw = it.sdata();
                g.unit_exponent = if raw as u32 & 0xFFFF_FFF0 == 0 {
                    (raw << 28) >> 28
                } else {
                    raw
                };
            }
            6 => g.unit = it.data,
            7 => g.report_size = it.data,
            8 => {
                if it.data == 0 || it.data > 255 {
                    return Err(ParseErrorKind::BadReportId(it.data));
                }
                g.report_id = it.data;
            }
            9 => g.report_count = it.data,
            10 => {
                self.stack.push(self.globals);
                self.out.max_push_depth = self.out.max_push_depth.max(self.stack.len());
            }
            11 => self.globals = self.stack.pop().ok_or(ParseErrorKind::PopWithoutPush)?,
            // Reserved tags: Linux refuses the descriptor, V1 and V2 skip the item.
            _ => {}
        }
        Ok(())
    }

    fn local(&mut self, it: &Item) {
        let l = &mut self.locals;
        if it.tag == 10 {
            // Delimiter: only the usages before the first set and in the first set count (Linux, V1).
            if it.data != 0 {
                l.delimiter_branch += 1;
            }
            return;
        }
        if l.delimiter_branch > 1 {
            return;
        }
        let page = self.globals.usage_page << 16;
        match it.tag {
            0 => {
                let extended = it.size == 4;
                let first = if extended {
                    it.data
                } else {
                    page | (it.data & 0xFFFF)
                };
                l.usages.push(UsageSpan {
                    first,
                    count: 1,
                    extended,
                });
            }
            1 => l.usage_min = it.data,
            2 => {
                // Linux counts from the raw minimum to the raw maximum and pages each usage only
                // when the Maximum item is short.
                if it.data >= l.usage_min {
                    let count = (it.data - l.usage_min) as u64 + 1;
                    let extended = it.size == 4;
                    let first = if extended {
                        l.usage_min
                    } else {
                        page | (l.usage_min & 0xFFFF)
                    };
                    l.usages.push(UsageSpan {
                        first,
                        count,
                        extended,
                    });
                }
            }
            _ => {}
        }
    }

    /// Linux `hid_concatenate_last_usage_page`: a Usage Page after the last short usages applies to them.
    fn concatenate_last_usage_page(&mut self) {
        // Compared with the full page (Linux keeps 32 bits), applied with the low 16 bits.
        let page = self.globals.usage_page;
        for span in self.locals.usages.spans_mut().iter_mut().rev() {
            if span.extended {
                continue;
            }
            if span.first >> 16 == page {
                break;
            }
            span.first = page << 16 | (span.first & 0xFFFF);
        }
    }

    fn main(&mut self, it: &Item) -> Result<(), ParseErrorKind> {
        self.concatenate_last_usage_page();
        let result = match it.tag {
            8 => self.add_field(ReportKind::Input, it.data),
            9 => self.add_field(ReportKind::Output, it.data),
            11 => self.add_field(ReportKind::Feature, it.data),
            10 => {
                let usage = self.locals.usages.get(0).unwrap_or(0);
                let kind = (it.data & 0xFF) as u8;
                let index = self.out.collections.len();
                let mut open = self.open.last().copied().unwrap_or(Open {
                    index,
                    application: 0,
                    physical: 0,
                    logical: 0,
                });
                open.index = index;
                match kind {
                    collection::APPLICATION => open.application = usage,
                    collection::PHYSICAL => open.physical = usage,
                    collection::LOGICAL => open.logical = usage,
                    _ => {}
                }
                self.out.collections.push(Collection {
                    kind,
                    usage,
                    parent: self.open.last().map(|o| o.index),
                    end: index + 1,
                });
                self.open.push(open);
                Ok(())
            }
            12 => match self.open.pop() {
                Some(o) => {
                    let end = self.out.collections.len();
                    self.out.collections[o.index].end = end;
                    Ok(())
                }
                None => Err(ParseErrorKind::EndWithoutCollection),
            },
            // Unknown main tags: Linux warns and goes on.
            _ => Ok(()),
        };
        self.locals = Locals::default();
        result
    }

    fn add_field(&mut self, kind: ReportKind, data: u32) -> Result<(), ParseErrorKind> {
        let g = self.globals;
        // Linux `hid_lookup_collection` for each kind, kept up to date at every Collection item.
        let open = self.open.last().copied();
        let application = open.map_or(0, |o| o.application);
        let field = Field {
            kind,
            report_id: g.report_id as u8,
            seq: self.seq,
            flags: data,
            bit_offset: 0,
            size: g.report_size,
            count: g.report_count,
            logical_min: g.logical_min,
            logical_max: g.logical_max,
            physical_min: g.physical_min,
            physical_max: g.physical_max,
            unit_exponent: g.unit_exponent,
            unit: g.unit,
            usages: core::mem::take(&mut self.locals.usages),
            collection: open.map(|o| o.index),
            innermost: open
                .and_then(|o| self.out.collections.get(o.index))
                .map(|c| (c.kind, c.usage)),
            application,
            physical: open.map_or(0, |o| o.physical),
            logical: open.map_or(0, |o| o.logical),
        };
        self.seq += 1;
        let reports = &mut self.out.reports;
        let at = match reports
            .iter()
            .position(|r| r.kind == kind && r.id == field.report_id)
        {
            Some(at) => at,
            None => {
                reports.push(Report {
                    kind,
                    id: field.report_id,
                    application,
                    bits: 0,
                    fields: Vec::new(),
                });
                reports.len() - 1
            }
        };
        let report = &mut reports[at];
        let bits = g.report_size as u64 * g.report_count as u64;
        let end = report
            .bits
            .checked_add(bits)
            .ok_or(ParseErrorKind::ReportTooLong)?;
        report.fields.push(Field {
            bit_offset: report.bits,
            ..field
        });
        report.bits = end;
        Ok(())
    }
}
