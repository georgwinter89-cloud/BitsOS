//! Connection lists: which F02 reads a list needs, and decoding their answers (short form: four
//! 8-bit entries per answer; long form: two 16-bit entries; the top bit of an entry marks a range
//! from the previous entry up to this one). V1 `widget.asm` `conns_read`, Linux
//! `hda_codec.c` `snd_hda_get_raw_connections`.

use alloc::vec::Vec;

use crate::{NID_LAST, Nid};

/// Entries a list can usefully hold: verb 701 selects an input with an 8-bit index, so an entry
/// at index 256 or later can never be chosen. A protocol limit, not a chosen one; it only bites
/// when ranges expand a hostile list (127 raw entries could otherwise name ~16k inputs).
pub const MAX_ENTRIES: usize = 256;

/// Why a connection list was thrown away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnError {
    /// An entry names a node above [`NID_LAST`]; no index after it could be trusted.
    NodeOutOfRange(u16),
}

/// A decoded connection list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnList {
    /// Inputs in hardware order; the index here is what verb 701 selects.
    pub nodes: Vec<Nid>,
    /// Range entries that went backwards or had nothing before them; skipped as Linux does.
    pub skipped_ranges: u32,
    /// Set when the whole list was unusable (then `nodes` is empty).
    pub error: Option<ConnError>,
    /// Entries past [`MAX_ENTRIES`] were dropped.
    pub truncated: bool,
}

/// Entries in the list (parameter 0x0E bits 6:0).
pub fn len(param: u32) -> u8 {
    (param & 0x7F) as u8
}

/// Long form (parameter 0x0E bit 7).
pub fn long_form(param: u32) -> bool {
    param & 0x80 != 0
}

fn per_answer(param: u32) -> u8 {
    if long_form(param) { 2 } else { 4 }
}

/// The start indices to read with F02 for a list whose parameter 0x0E is `param`.
pub fn reads(param: u32) -> impl Iterator<Item = u8> {
    (0..len(param)).step_by(per_answer(param) as usize)
}

/// Decodes the answers to [`reads`], in that order. Missing answers (fewer than [`reads`] gives)
/// end the list where they start.
pub fn decode(param: u32, answers: &[u32]) -> ConnList {
    let mut out = ConnList::default();
    let per = per_answer(param) as usize;
    let (bits, range_bit) = if long_form(param) {
        (16, 0x8000u32)
    } else {
        (8, 0x80u32)
    };
    let mask = (1u32 << bits) - 1;
    let mut prev: Option<u16> = None;
    for i in 0..len(param) as usize {
        let Some(&answer) = answers.get(i / per) else {
            break;
        };
        let raw = (answer >> ((i % per) * bits)) & mask;
        let node = (raw & !range_bit) as u16;
        if node > NID_LAST as u16 {
            return ConnList {
                nodes: Vec::new(),
                skipped_ranges: out.skipped_ranges,
                error: Some(ConnError::NodeOutOfRange(node)),
                truncated: false,
            };
        }
        if raw & range_bit != 0 {
            match prev {
                Some(p) if p < node => out.nodes.extend((p + 1..=node).map(|n| n as Nid)),
                _ => {
                    out.skipped_ranges += 1;
                    continue;
                }
            }
        } else {
            out.nodes.push(node as Nid);
        }
        prev = Some(node);
        if out.nodes.len() > MAX_ENTRIES {
            out.nodes.truncate(MAX_ENTRIES);
            out.truncated = true;
            break;
        }
    }
    out
}
