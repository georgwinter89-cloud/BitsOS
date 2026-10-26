//! The codec dump: a plain-text record of read-only verbs and their answers.
//!
//! ```text
//! # format: hdacodec-dump 1
//! # synthetic: no
//! # pci: C7:00.6 1022:15E3
//! 0 00 F0000 10EC0269
//! 0 01 F0005 00000101
//! 0 14 F0900 -
//! ```
//!
//! One read per line: codec address (hex, 0-F), node (hex, 00-7F), verb (hex, up to 5 digits:
//! verb ID and payload), answer (hex, up to 8 digits, or `-` for no answer). Fields are separated
//! by spaces or tabs; an optional `0x` prefix is accepted. Lines starting with `#` are comments;
//! a comment of the form `# key: value` is kept as a header field. Only read-only verbs are
//! allowed. The same read twice is fine if the answers agree. See `DUMP-FORMAT.md` for the list
//! of reads a complete dump holds.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;

use crate::verb::is_read_only;
use crate::{CAD_LAST, NID_LAST, Nid, NoResponse, VerbSource};

/// One read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Line {
    pub cad: u8,
    pub nid: Nid,
    pub verb: u32,
    pub answer: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseErrorKind {
    FieldCount,
    BadNumber,
    CadOutOfRange,
    NidOutOfRange,
    VerbOutOfRange,
    NotReadOnly,
    /// The same read appears with a different answer.
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based.
    pub line: usize,
    pub kind: ParseErrorKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dump {
    pub header: Vec<(String, String)>,
    lines: Vec<Line>,
    index: BTreeMap<(u8, Nid, u32), usize>,
}

fn hex(field: &str, max_digits: usize) -> Option<u32> {
    let digits = field
        .strip_prefix("0x")
        .or_else(|| field.strip_prefix("0X"))
        .unwrap_or(field);
    // from_str_radix alone would also take a leading '+'.
    if digits.is_empty()
        || digits.len() > max_digits
        || !digits.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

impl Dump {
    pub fn new() -> Self {
        Dump::default()
    }

    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    /// Header value for `key`, if present.
    pub fn header_value(&self, key: &str) -> Option<&str> {
        self.header
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The dump says it was made up (`# synthetic: yes`), not read from hardware.
    pub fn is_synthetic(&self) -> bool {
        self.header_value("synthetic")
            .is_some_and(|v| v.starts_with("yes"))
    }

    /// Adds a read; `Err(Conflict)` if the same read is already there with another answer.
    pub fn push(&mut self, line: Line) -> Result<(), ParseErrorKind> {
        if line.cad > CAD_LAST {
            return Err(ParseErrorKind::CadOutOfRange);
        }
        if line.nid > NID_LAST {
            return Err(ParseErrorKind::NidOutOfRange);
        }
        if !is_read_only(line.verb) {
            return Err(ParseErrorKind::NotReadOnly);
        }
        match self.index.get(&(line.cad, line.nid, line.verb)) {
            Some(&i) if self.lines[i].answer == line.answer => Ok(()),
            Some(_) => Err(ParseErrorKind::Conflict),
            None => {
                self.index
                    .insert((line.cad, line.nid, line.verb), self.lines.len());
                self.lines.push(line);
                Ok(())
            }
        }
    }

    /// The recorded answer: `None` if the read is not in the dump, `Some(None)` if it is recorded
    /// as unanswered.
    pub fn get(&self, cad: u8, nid: Nid, verb: u32) -> Option<Option<u32>> {
        self.index
            .get(&(cad, nid, verb))
            .map(|&i| self.lines[i].answer)
    }

    /// Codec addresses that appear in the dump, in order.
    pub fn codecs(&self) -> Vec<u8> {
        let mut cads: Vec<u8> = self.lines.iter().map(|l| l.cad).collect();
        cads.sort_unstable();
        cads.dedup();
        cads
    }

    /// A [`VerbSource`] answering from this dump for codec `cad`.
    pub fn source(&self, cad: u8) -> DumpSource<'_> {
        DumpSource { dump: self, cad }
    }

    pub fn parse(text: &str) -> Result<Dump, ParseError> {
        let mut dump = Dump::new();
        for (n, raw) in text.lines().enumerate() {
            let err = |kind| ParseError { line: n + 1, kind };
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(comment) = line.strip_prefix('#') {
                if let Some((k, v)) = comment.split_once(':') {
                    let k = k.trim();
                    if !k.is_empty() && !k.contains(char::is_whitespace) {
                        dump.header.push((k.to_string(), v.trim().to_string()));
                    }
                }
                continue;
            }
            let mut f = line.split_whitespace();
            let (Some(cad), Some(nid), Some(verb), Some(answer), None) =
                (f.next(), f.next(), f.next(), f.next(), f.next())
            else {
                return Err(err(ParseErrorKind::FieldCount));
            };
            let cad = hex(cad, 1).ok_or(err(ParseErrorKind::BadNumber))?;
            let nid = hex(nid, 2).ok_or(err(ParseErrorKind::BadNumber))?;
            let verb = hex(verb, 5).ok_or(err(ParseErrorKind::BadNumber))?;
            let answer = match answer {
                "-" => None,
                a => Some(hex(a, 8).ok_or(err(ParseErrorKind::BadNumber))?),
            };
            dump.push(Line {
                cad: cad as u8,
                nid: nid as u8,
                verb,
                answer,
            })
            .map_err(err)?;
        }
        Ok(dump)
    }

    /// The dump as text, header first, reads in the order they were added.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        for (k, v) in &self.header {
            let _ = writeln!(s, "# {k}: {v}");
        }
        for l in &self.lines {
            let _ = match l.answer {
                Some(a) => writeln!(s, "{:X} {:02X} {:05X} {:08X}", l.cad, l.nid, l.verb, a),
                None => writeln!(s, "{:X} {:02X} {:05X} -", l.cad, l.nid, l.verb),
            };
        }
        s
    }
}

/// Answers verbs from a [`Dump`]; reads not in the dump get no answer.
pub struct DumpSource<'a> {
    dump: &'a Dump,
    cad: u8,
}

impl VerbSource for DumpSource<'_> {
    fn get(&mut self, nid: Nid, verb: u32) -> Result<u32, NoResponse> {
        self.dump
            .get(self.cad, nid, verb)
            .flatten()
            .ok_or(NoResponse)
    }
}
