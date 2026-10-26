//! The parsed layout: collections, and per report kind and report ID the fields with their bit positions.

use crate::usages::Usages;
use alloc::vec::Vec;

/// Main item data bits (HID 1.11 §6.2.2.5), the same values as Linux `HID_MAIN_ITEM_*`.
pub mod flags {
    pub const CONSTANT: u32 = 0x001;
    pub const VARIABLE: u32 = 0x002;
    pub const RELATIVE: u32 = 0x004;
    pub const WRAP: u32 = 0x008;
    pub const NONLINEAR: u32 = 0x010;
    pub const NO_PREFERRED: u32 = 0x020;
    pub const NULL_STATE: u32 = 0x040;
    pub const VOLATILE: u32 = 0x080;
    pub const BUFFERED_BYTE: u32 = 0x100;
}

/// Collection types (HID 1.11 §6.2.2.6).
pub mod collection {
    pub const PHYSICAL: u8 = 0;
    pub const APPLICATION: u8 = 1;
    pub const LOGICAL: u8 = 2;
    pub const NAMED_ARRAY: u8 = 4;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReportKind {
    Input,
    Output,
    Feature,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collection {
    /// Collection type (`collection::*`, or another value the device chose).
    pub kind: u8,
    /// The first usage declared before the Collection item, 0 if none.
    pub usage: u32,
    pub parent: Option<usize>,
    /// One past the index of its last nested collection: collections are stored in order of their
    /// Collection items, so the ones nested in `i` are exactly `i + 1..end`.
    pub end: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub kind: ReportKind,
    pub report_id: u8,
    /// Position of this main item among all Input/Output/Feature items of the descriptor.
    pub seq: usize,
    /// Main item data (`flags::*`).
    pub flags: u32,
    /// Bit offset of the first value, counted after the report ID byte (as Linux `report_offset`).
    pub bit_offset: u64,
    /// Bits per value (Report Size) and number of values (Report Count).
    pub size: u32,
    pub count: u32,
    pub logical_min: i32,
    pub logical_max: i32,
    pub physical_min: i32,
    pub physical_max: i32,
    pub unit_exponent: i32,
    pub unit: u32,
    /// Empty for padding fields (Linux registers no field for them, but they take bits).
    pub usages: Usages,
    /// Innermost enclosing collection.
    pub collection: Option<usize>,
    /// Type and usage of that collection (Linux looks at it through `usage->collection_index`, e.g. for
    /// the programmable buttons of a Named Array).
    pub innermost: Option<(u8, u32)>,
    /// Usages of the innermost enclosing application, physical and logical collections (0 = none),
    /// as Linux `field->application`, `->physical`, `->logical`.
    pub application: u32,
    pub physical: u32,
    pub logical: u32,
}

impl Field {
    pub fn is_variable(&self) -> bool {
        self.flags & flags::VARIABLE != 0
    }

    pub fn is_constant(&self) -> bool {
        self.flags & flags::CONSTANT != 0
    }

    pub fn is_relative(&self) -> bool {
        self.flags & flags::RELATIVE != 0
    }

    pub fn is_padding(&self) -> bool {
        self.usages.is_empty()
    }

    /// Linux `field->maxusage`: declared usages, at least one per value.
    pub fn max_usage(&self) -> u64 {
        self.usages.len().max(self.count as u64)
    }

    /// Usage of value `n` of a variable field, or of array index `n`.
    pub fn usage(&self, n: u64) -> Option<u32> {
        self.usages.get(n)
    }

    /// Bit offset of value `n` after the report ID byte (saturates for an `n` past the field).
    pub fn value_offset(&self, n: u32) -> u64 {
        self.bit_offset
            .saturating_add((n as u64).saturating_mul(self.size as u64))
    }

    /// Whether this field lies inside collection `c` (directly or nested).
    pub fn is_inside(&self, collections: &[Collection], c: usize) -> bool {
        match (self.collection, collections.get(c)) {
            (Some(i), Some(col)) => i >= c && i < col.end,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub kind: ReportKind,
    /// 0 = the descriptor declares no report ID for these fields.
    pub id: u8,
    /// Usage of the application collection of the report's first field (Linux `report->application`).
    pub application: u32,
    /// Length in bits after the ID byte.
    pub bits: u64,
    pub fields: Vec<Field>,
}

impl Report {
    /// Bytes the device sends for this report, the ID byte included when the kind is numbered.
    pub fn len_bytes(&self, numbered: bool) -> u64 {
        self.bits.div_ceil(8) + numbered as u64
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReportDescriptor {
    pub collections: Vec<Collection>,
    /// In order of first appearance.
    pub reports: Vec<Report>,
    /// Deepest Push nesting seen (V1 refused more than 4, V2 has no limit).
    pub max_push_depth: usize,
}

impl ReportDescriptor {
    /// Linux `report_enum->numbered`: reports of this kind start with their ID byte.
    pub fn numbered(&self, kind: ReportKind) -> bool {
        self.reports.iter().any(|r| r.kind == kind && r.id != 0)
    }

    pub fn report(&self, kind: ReportKind, id: u8) -> Option<&Report> {
        self.reports.iter().find(|r| r.kind == kind && r.id == id)
    }

    pub fn reports_of(&self, kind: ReportKind) -> impl Iterator<Item = &Report> {
        self.reports.iter().filter(move |r| r.kind == kind)
    }

    /// Every field of every report, in declaration order.
    pub fn fields_in_order(&self) -> Vec<&Field> {
        let mut all: Vec<&Field> = self.reports.iter().flat_map(|r| r.fields.iter()).collect();
        all.sort_by_key(|f| f.seq);
        all
    }

    /// The first application collection that is a Joystick or Gamepad (Generic Desktop 4 or 5).
    pub fn first_gamepad(&self) -> Option<usize> {
        self.collections.iter().position(|c| {
            c.kind == collection::APPLICATION && matches!(c.usage, 0x0001_0004 | 0x0001_0005)
        })
    }
}
