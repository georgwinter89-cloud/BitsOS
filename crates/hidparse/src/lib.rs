//! HID report descriptors for BitsOS V2 (item IN-1, `plan/spec/usb.md` §4).
//!
//! - [`parse`] turns a report descriptor into a [`ReportDescriptor`]: collections and, per report
//!   kind and report ID, the fields with bit offsets, sizes, logical/physical ranges, units and usages.
//! - [`Field::value`] and [`extract`] read values out of a report.
//! - [`InputMap`] maps usages to Linux evdev codes like drivers/hid/hid-input.c with hid-generic
//!   (one group = one input device per application collection), and [`InputState`] turns reports
//!   into the events hid-input would send.
//! - [`boot`] has the boot-protocol fallback layouts; [`output`] builds LED output reports.
//!
//! Hostile descriptors and reports never panic; there is no cap on counts (R1), memory stays
//! proportional to the descriptor. Where V2 deliberately differs from V1 and from Linux:
//! `plan/notes/IN-1.md`.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod boot;
pub mod codes;
mod covered;
pub mod descriptor;
pub mod extract;
pub mod input;
pub mod item;
mod keyboard;
pub mod map;
pub mod output;
pub mod parse;
mod usage_map;
pub mod usages;

pub use descriptor::{Collection, Field, Report, ReportDescriptor, ReportKind};
pub use extract::ExtractError;
pub use input::{Event, InputError, InputState};
pub use map::{AbsInfo, Group, InputMap, is_input_application};
pub use output::{OutputReport, led_reports, put_bits};
pub use parse::{ParseError, ParseErrorKind, parse};
pub use usage_map::{Special, Target};
