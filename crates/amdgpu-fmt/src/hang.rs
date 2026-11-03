//! Hang-dump decoder: ring or IB contents as readable packets, for the hang report ([`crate::watch`]) and the
//! desk's MCP dumps.
//!
//! Decoding never trusts the dwords: a header whose length is unknown (a garbage dword, an SDMA op this crate
//! does not know) becomes one raw line and decoding goes on with the next dword (on SDMA, zero dwords are NOPs,
//! so it finds its step again quickly); a packet running past the end is shown as truncated. Lines are text
//! made with `core::fmt` (no_std).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::sim::Isa;
use crate::{pm4, sdma};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Packet,
    /// A dword that does not start a packet this crate can measure.
    Raw,
    /// A packet longer than what follows it.
    Truncated,
}

/// One decoded packet (or raw dword).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// Position: ring pointer value (dword counter) or IB dword offset.
    pub pos: u64,
    pub len: u64,
    pub header: u32,
    pub kind: LineKind,
    pub text: String,
    /// The engine's read pointer lies inside this packet.
    pub at_rptr: bool,
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mark = if self.at_rptr { "=>" } else { "  " };
        write!(f, "{mark} {:#08x} [{:>4}] {:08x}  {}", self.pos, self.len, self.header, self.text)
    }
}

/// Decodes `words` as a packet stream whose first dword sits at position `base`.
pub fn decode_stream(isa: Isa, words: &[u32], base: u64) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut i = 0usize;
    while i < words.len() {
        let rest = &words[i..];
        let header = rest[0];
        let pos = base.wrapping_add(i as u64);
        let (kind, len, text) = match isa {
            Isa::Pm4 => match pm4::decode(rest) {
                Ok((p, n)) => (LineKind::Packet, n, format!("{p}")),
                Err(pm4::DecodeError::Truncated { need, have }) => {
                    let name = pm4::op_name(pm4::hdr_opcode(header)).unwrap_or("?");
                    (LineKind::Truncated, have, format!("{name} TRUNCATED need={need} have={have}"))
                }
                Err(pm4::DecodeError::BadCount { .. }) => {
                    let n = pm4::packet_len(rest).unwrap_or(1).min(rest.len());
                    let name = pm4::op_name(pm4::hdr_opcode(header)).unwrap_or("?");
                    (LineKind::Packet, n, format!("{name} BAD COUNT body={}", n - 1))
                }
                Err(pm4::DecodeError::BadType { .. }) => (LineKind::Raw, 1, String::from("?? type-1 header")),
            },
            Isa::Sdma => match sdma::decode(rest) {
                Ok((p, n)) => (LineKind::Packet, n, format!("{p}")),
                Err(sdma::DecodeError::Truncated { need, have }) => {
                    let name = sdma::op_name(sdma::hdr_op(header)).unwrap_or("?");
                    (LineKind::Truncated, have, format!("{name} TRUNCATED need={need} have={have}"))
                }
                Err(sdma::DecodeError::Unknown { .. }) => {
                    let text = match sdma::op_name(sdma::hdr_op(header)) {
                        Some(n) => format!("?? {n} sub={} (length unknown)", sdma::hdr_sub(header)),
                        None => format!("?? op={} sub={}", sdma::hdr_op(header), sdma::hdr_sub(header)),
                    };
                    (LineKind::Raw, 1, text)
                }
            },
        };
        let len = len.max(1);
        lines.push(Line { pos, len: len as u64, header, kind, text, at_rptr: false });
        i += len;
    }
    lines
}

/// A ring dump.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dump {
    pub lines: Vec<Line>,
    /// The window decoded: positions `from..wptr`.
    pub from: u64,
    pub wptr: u64,
    pub rptr: u64,
    /// The read pointer lies outside the window (or at its end: everything was consumed).
    pub rptr_outside: bool,
}

impl fmt::Display for Dump {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "ring from={:#x} rptr={:#x} wptr={:#x}", self.from, self.rptr, self.wptr)?;
        for l in &self.lines {
            writeln!(f, "{l}")?;
        }
        if self.rptr_outside {
            writeln!(f, "rptr is not inside a decoded packet")?;
        }
        Ok(())
    }
}

/// Decodes the packets of `ring` (a snapshot of the whole ring, a power-of-two number of dwords) from
/// position `from` up to `wptr` (dword counters; `from` should be a packet start the driver knows, e.g. the
/// last signalled submission's start). A window longer than the ring is cut to the last ring's worth.
pub fn decode_ring(isa: Isa, ring: &[u32], from: u64, rptr: u64, wptr: u64) -> Dump {
    let size = ring.len() as u64;
    let mut from = from;
    if !size.is_power_of_two() {
        return Dump { lines: Vec::new(), from, wptr, rptr, rptr_outside: true };
    }
    if wptr.wrapping_sub(from) > size {
        from = wptr.wrapping_sub(size);
    }
    let n = wptr.wrapping_sub(from);
    let words: Vec<u32> = (0..n).map(|i| ring[(from.wrapping_add(i) & (size - 1)) as usize]).collect();
    let mut lines = decode_stream(isa, &words, from);
    let mut found = false;
    for l in &mut lines {
        let off = rptr.wrapping_sub(l.pos);
        if off < l.len {
            l.at_rptr = true;
            found = true;
        }
    }
    Dump { lines, from, wptr, rptr, rptr_outside: !found }
}
