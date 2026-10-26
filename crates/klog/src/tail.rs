//! The newest log lines as text in a caller's buffer, for the crash record (RB-2, spec/core.md §10.3 "last log
//! KB"): one try of the ring lock, no allocation, so the panic path can call it with the heap in emergency mode.

use crate::lock::Wait;
use crate::render;
use crate::ring::RingOps;
use core::fmt::{self, Write};

/// A prefix (`[    1.234567] I net link.up`) on the stack; subsystem and event names are short static names.
struct Small {
    b: [u8; 160],
    n: usize,
}

impl Write for Small {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let k = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

fn prefix(h: &crate::ring::Hdr) -> Small {
    let mut p = Small { b: [0; 160], n: 0 };
    render::prefix(&mut p, h.time_ns, h.level, h.subsystem, h.name);
    p
}

/// Bytes of record `h` as one line: prefix, a space and the text when there is text, newline.
fn line_len(r: &dyn RingOps, h: &crate::ring::Hdr) -> usize {
    let (a, b) = r.text_parts(h);
    let text = a.len() + b.len();
    prefix(h).n + if text > 0 { 1 + text } else { 0 } + 1
}

/// Writes the newest whole log lines that fit into `out`, oldest first, each ending in `\n`; returns the bytes
/// written. `None` when the ring is busy (held by this or another CPU): the panic path never waits for it.
pub fn try_tail_text(out: &mut [u8]) -> Option<usize> {
    try_tail_text_before(out, u64::MAX)
}

/// As `try_tail_text`, with only the records whose sequence number is below `seq` (the panic path passes
/// `next_seq()` from its start, so the record keeps what happened before the panic, not the report itself).
pub fn try_tail_text_before(out: &mut [u8], seq: u64) -> Option<usize> {
    let g = crate::lock_ring(Wait::Never)?;
    let r = g.active_ref();
    let (head, end) = r.span();
    let end = if seq == u64::MAX { end } else { r.find(seq) };
    let mut first = end;
    let mut total = 0usize;
    while first > head {
        let len = line_len(r, &r.hdr(first - 1));
        if total + len > out.len() {
            break;
        }
        total += len;
        first -= 1;
    }
    let mut at = 0;
    for i in first..end {
        let h = r.hdr(i);
        let p = prefix(&h);
        let (a, b) = r.text_parts(&h);
        let mut put = |s: &[u8]| {
            out[at..at + s.len()].copy_from_slice(s);
            at += s.len();
        };
        put(&p.b[..p.n]);
        if !a.is_empty() || !b.is_empty() {
            put(b" ");
            put(a);
            put(b);
        }
        put(b"\n");
    }
    Some(at)
}

