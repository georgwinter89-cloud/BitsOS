//! Rendering an event into one text line on the stack, so emitting never allocates (the panic and NMI paths
//! may run while the heap lock is held). Format, unchanged from G0 and V1's mirror:
//! `[    1.234567] I net link.up ifname=igc0 speed_mbps=1000 | message`.

use crate::{Event, Level};
use core::fmt::{self, Write};

/// Bytes of a rendered line including the time stamp and names; longer text is cut and marked (a bound on a
/// single record, not on the number of records).
pub const LINE_BYTES: usize = 1024;
const CUT_MARK: &str = " [cut]";

pub struct LineBuf {
    buf: [u8; LINE_BYTES],
    len: usize,
    cut: bool,
}

impl LineBuf {
    pub const fn new() -> Self {
        LineBuf { buf: [0; LINE_BYTES], len: 0, cut: false }
    }
    pub fn as_str(&self) -> &str {
        // Only whole chars are ever copied in (see `write_str`); the fallback never triggers.
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn was_cut(&self) -> bool {
        self.cut
    }
    fn finish(&mut self) {
        if self.cut {
            let m = CUT_MARK.as_bytes();
            self.buf[self.len..self.len + m.len()].copy_from_slice(m);
            self.len += m.len();
        }
    }
}

impl Write for LineBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.cut {
            return Ok(());
        }
        let room = LINE_BYTES - CUT_MARK.len() - self.len;
        let mut n = s.len().min(room);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        if n < s.len() {
            self.cut = true;
        }
        Ok(())
    }
}

/// Writes through, with control characters (newline, carriage return, ESC, C1, ...) escaped as `\n`, `\r`,
/// `\u{1b}`, so text from outside can never break a line or reach a terminal as a control sequence.
pub struct Escaped<'a, W: Write>(pub &'a mut W);

impl<W: Write> Write for Escaped<'_, W> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut rest = s;
        while let Some(i) = rest.find(char::is_control) {
            self.0.write_str(&rest[..i])?;
            let c = rest[i..].chars().next().unwrap_or(' ');
            match c {
                '\n' => self.0.write_str("\\n")?,
                '\r' => self.0.write_str("\\r")?,
                '\t' => self.0.write_str("\\t")?,
                c => write!(self.0, "\\u{{{:x}}}", c as u32)?,
            }
            rest = &rest[i + c.len_utf8()..];
        }
        self.0.write_str(rest)
    }
}

/// The part before the text: `[    1.234567] I net link.up`.
pub fn prefix(w: &mut impl Write, time_ns: u64, level: Level, subsystem: &str, name: &str) {
    let _ = write!(w, "[{:5}.{:06}] {} {} {}", time_ns / 1_000_000_000, (time_ns / 1000) % 1_000_000, level.letter(), subsystem, name);
}

/// Renders the whole line; returns the byte offset where the record text (fields and message) starts.
pub fn line(out: &mut LineBuf, time_ns: u64, ev: &Event<'_>) -> usize {
    prefix(out, time_ns, ev.level, ev.subsystem, ev.name);
    let has_text = !ev.fields.is_empty() || ev.message.is_some();
    if has_text {
        let _ = out.write_str(" ");
    }
    let text_start = out.len();
    for (i, f) in ev.fields.iter().enumerate() {
        let _ = write!(out, "{}{}={}", if i > 0 { " " } else { "" }, f.key, f.value);
    }
    if let Some(m) = ev.message {
        let _ = out.write_str(if ev.fields.is_empty() { "| " } else { " | " });
        // Messages may carry text from outside (names, paths, SSIDs): one record is always one line.
        let _ = write!(Escaped(out), "{m}");
    }
    out.finish();
    text_start.min(out.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_are_escaped() {
        let mut b = LineBuf::new();
        let _ = write!(Escaped(&mut b), "a\nb\r\u{1b}[2J\u{9b}x\tä");
        assert_eq!(b.as_str(), "a\\nb\\r\\u{1b}[2J\\u{9b}x\\tä");
    }

    #[test]
    fn cut_at_the_limit_keeps_whole_chars() {
        let room = LINE_BYTES - CUT_MARK.len();
        for len in [room - 1, room, room + 1] {
            let mut b = LineBuf::new();
            let _ = b.write_str(&"x".repeat(len));
            b.finish();
            assert_eq!(b.was_cut(), len > room, "{len}");
            assert!(b.len() <= LINE_BYTES);
        }
        // A 3-byte char across the limit is left out whole.
        let mut b = LineBuf::new();
        let _ = b.write_str(&("x".repeat(room - 1) + "\u{20ac}"));
        b.finish();
        assert!(b.was_cut() && b.as_str().starts_with(&"x".repeat(room - 1)) && b.as_str().ends_with(CUT_MARK));
        assert_eq!(b.len(), room - 1 + CUT_MARK.len());
    }

    #[test]
    fn cuts_long_lines_at_a_char_boundary_and_marks_them() {
        let mut b = LineBuf::new();
        let long: alloc::string::String = "ä".repeat(LINE_BYTES);
        let _ = b.write_str(&long);
        b.finish();
        assert!(b.was_cut());
        assert!(b.as_str().ends_with(CUT_MARK));
        assert!(b.len() <= LINE_BYTES);
    }
}
