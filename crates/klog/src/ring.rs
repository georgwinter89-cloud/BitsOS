//! The record ring: headers and text live in two circular stores cut from one byte budget, so the ring keeps as
//! many records as their text allows (no fixed record count, R1). The code is generic over the storage: the same
//! ring runs on the static early store (before the heap) and on the heap store sized from memory.
//!
//! Records enter in sequence order (the sequence number is taken under the ring lock), so a reader finds its
//! place by binary search and sees every hole in the numbers as records it missed.

use crate::Level;
use alloc::string::String;

#[derive(Clone, Copy, Debug)]
pub struct Hdr {
    pub seq: u64,
    pub time_ns: u64,
    pub thread: u64,
    /// Absolute (ever-growing) byte position of the text in the text store.
    pub text_pos: u64,
    pub text_len: u32,
    pub cpu: u32,
    pub level: Level,
    /// The emitter wrote the line to serial itself (panic/NMI path): the serial printer leaves it out.
    pub direct: bool,
    pub subsystem: &'static str,
    pub name: &'static str,
}

impl Hdr {
    pub const EMPTY: Hdr =
        Hdr { seq: 0, time_ns: 0, thread: 0, text_pos: 0, text_len: 0, cpu: 0, level: Level::Info, direct: false, subsystem: "", name: "" };
}

/// Average text bytes per record assumed when a byte budget is split between headers and text.
const AVG_TEXT: usize = 96;

/// (header slots, text bytes) for a byte budget.
pub const fn split(bytes: usize) -> (usize, usize) {
    let h = core::mem::size_of::<Hdr>();
    let n = bytes / (h + AVG_TEXT);
    (n, bytes - n * h)
}

/// What the rest of klog needs from a ring, whatever its storage.
pub trait RingOps {
    /// Stores a record whose text is `a` followed by `b` (two parts, so a record copied out of another ring's
    /// wrapped text store needs no buffer).
    fn push_parts(&mut self, h: Hdr, a: &[u8], b: &[u8]);
    fn push(&mut self, h: Hdr, text: &[u8]) {
        self.push_parts(h, text, &[]);
    }
    /// Logical indices of the kept records: `[first, end)`.
    fn span(&self) -> (u64, u64);
    fn hdr(&self, i: u64) -> Hdr;
    fn text(&self, h: &Hdr) -> String;
    /// The text bytes of a record in place (second part non-empty when it wraps the store's end).
    fn text_parts(&self, h: &Hdr) -> (&[u8], &[u8]);
    /// First logical index whose record has `seq >= seq` (`end` if none).
    fn find(&self, seq: u64) -> u64;
    fn bytes(&self) -> usize;
    fn clear(&mut self);
}

pub struct RingCore<H, T> {
    hdrs: H,
    text: T,
    head: u64,
    tail: u64,
    text_head: u64,
    text_tail: u64,
}

impl<H, T> RingCore<H, T> {
    pub const fn new(hdrs: H, text: T) -> Self {
        RingCore { hdrs, text, head: 0, tail: 0, text_head: 0, text_tail: 0 }
    }
}

impl<H: AsRef<[Hdr]> + AsMut<[Hdr]>, T: AsRef<[u8]> + AsMut<[u8]>> RingCore<H, T> {
    fn hcap(&self) -> u64 {
        self.hdrs.as_ref().len() as u64
    }
    fn tcap(&self) -> u64 {
        self.text.as_ref().len() as u64
    }
    fn evict_oldest(&mut self) {
        if self.head == self.tail {
            return;
        }
        self.head += 1;
        self.text_head = if self.head < self.tail { self.hdr(self.head).text_pos } else { self.text_tail };
    }
}

impl<H: AsRef<[Hdr]> + AsMut<[Hdr]>, T: AsRef<[u8]> + AsMut<[u8]>> RingOps for RingCore<H, T> {
    fn push_parts(&mut self, mut h: Hdr, a: &[u8], b: &[u8]) {
        let (hcap, tcap) = (self.hcap(), self.tcap());
        if hcap == 0 {
            return;
        }
        // A text larger than the whole store keeps its beginning; callers cut at a char boundary first.
        let len = ((a.len() + b.len()) as u64).min(tcap);
        while self.tail - self.head >= hcap || (self.text_tail - self.text_head) + len > tcap {
            self.evict_oldest();
        }
        let mut at = self.text_tail;
        let mut left = len as usize;
        for part in [a, b] {
            let part = &part[..part.len().min(left)];
            if part.is_empty() {
                continue;
            }
            let start = (at % tcap) as usize;
            let first = part.len().min(tcap as usize - start);
            let store = self.text.as_mut();
            store[start..start + first].copy_from_slice(&part[..first]);
            store[..part.len() - first].copy_from_slice(&part[first..]);
            at += part.len() as u64;
            left -= part.len();
        }
        h.text_pos = self.text_tail;
        h.text_len = len as u32;
        self.text_tail += len;
        let slot = (self.tail % hcap) as usize;
        self.hdrs.as_mut()[slot] = h;
        self.tail += 1;
    }

    fn span(&self) -> (u64, u64) {
        (self.head, self.tail)
    }

    fn hdr(&self, i: u64) -> Hdr {
        self.hdrs.as_ref()[(i % self.hcap()) as usize]
    }

    fn text(&self, h: &Hdr) -> String {
        let (a, b) = self.text_parts(h);
        let mut v = alloc::vec::Vec::with_capacity(a.len() + b.len());
        v.extend_from_slice(a);
        v.extend_from_slice(b);
        // Text is cut at char boundaries when stored; lossy keeps a reader alive even if that ever slips.
        match String::from_utf8(v) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        }
    }

    fn text_parts(&self, h: &Hdr) -> (&[u8], &[u8]) {
        let tcap = self.tcap();
        if h.text_len == 0 || tcap == 0 {
            return (&[], &[]);
        }
        let store = self.text.as_ref();
        let start = (h.text_pos % tcap) as usize;
        let len = (h.text_len as usize).min(tcap as usize);
        let first = len.min(tcap as usize - start);
        (&store[start..start + first], &store[..len - first])
    }

    fn find(&self, seq: u64) -> u64 {
        let (mut lo, mut hi) = (self.head, self.tail);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.hdr(mid).seq < seq {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    fn bytes(&self) -> usize {
        core::mem::size_of_val(self.hdrs.as_ref()) + self.text.as_ref().len()
    }

    fn clear(&mut self) {
        self.head = self.tail;
        self.text_head = self.text_tail;
    }
}

/// Copies the kept records of `from` with `seq >= since` into `to`, oldest first, at most `max` of them, without
/// allocating (`to` evicts as it needs). Returns the sequence number to continue with and whether `from` has
/// nothing more.
pub fn migrate_chunk(from: &dyn RingOps, to: &mut dyn RingOps, since: u64, max: u64) -> (u64, bool) {
    let (start, end) = (from.find(since), from.span().1);
    let stop = start.saturating_add(max).min(end);
    let mut next = since;
    for i in start..stop {
        let h = from.hdr(i);
        let (a, b) = from.text_parts(&h);
        to.push_parts(h, a, b);
        next = h.seq + 1;
    }
    (next, stop >= end)
}

/// Copies every kept record of `from` into `to`, oldest first (`to` evicts as it needs).
pub fn migrate(from: &dyn RingOps, to: &mut dyn RingOps) -> u64 {
    let (a, b) = from.span();
    let _ = migrate_chunk(from, to, 0, b - a);
    b - a
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn rec(seq: u64) -> Hdr {
        Hdr { seq, time_ns: seq * 10, subsystem: "t", name: "n", ..Hdr::EMPTY }
    }

    fn seqs(r: &dyn RingOps) -> Vec<u64> {
        let (a, b) = r.span();
        (a..b).map(|i| r.hdr(i).seq).collect()
    }

    #[test]
    fn wraps_by_header_slots() {
        let mut r = RingCore::new([Hdr::EMPTY; 4], [0u8; 1024]);
        for s in 0..10 {
            r.push(rec(s), b"x");
        }
        assert_eq!(seqs(&r), [6, 7, 8, 9]);
        assert_eq!(r.text(&r.hdr(r.span().0)), "x");
    }

    #[test]
    fn wraps_by_text_bytes_and_keeps_text_intact() {
        let mut r = RingCore::new(vec![Hdr::EMPTY; 100], vec![0u8; 25]);
        for s in 0..20u64 {
            let t = std::format!("rec{s:03}");
            r.push(rec(s), t.as_bytes());
        }
        // 25 bytes hold four 6-byte texts.
        assert_eq!(seqs(&r), [16, 17, 18, 19]);
        let (a, b) = r.span();
        for i in a..b {
            let h = r.hdr(i);
            assert_eq!(r.text(&h), std::format!("rec{:03}", h.seq), "text across the wrap point");
        }
    }

    #[test]
    fn oversized_text_is_cut_to_the_store() {
        let mut r = RingCore::new([Hdr::EMPTY; 4], [0u8; 8]);
        r.push(rec(1), b"0123456789");
        assert_eq!(r.text(&r.hdr(0)), "01234567");
        r.push(rec(2), b"ab");
        assert_eq!(seqs(&r), [2]);
    }

    #[test]
    fn zero_capacity_drops_quietly() {
        let mut r = RingCore::new([Hdr::EMPTY; 0], [0u8; 0]);
        r.push(rec(1), b"x");
        assert_eq!(r.span(), (0, 0));
        let mut r = RingCore::new([Hdr::EMPTY; 2], [0u8; 0]);
        r.push(rec(1), b"x");
        assert_eq!(r.text(&r.hdr(0)), "");
    }

    #[test]
    fn find_by_sequence_with_holes() {
        let mut r = RingCore::new(vec![Hdr::EMPTY; 8], vec![0u8; 64]);
        for s in [3u64, 4, 7, 8, 12] {
            r.push(rec(s), b"");
        }
        let at = |s| r.hdr(r.find(s)).seq;
        assert_eq!(at(0), 3);
        assert_eq!(at(5), 7);
        assert_eq!(at(12), 12);
        assert_eq!(r.find(13), r.span().1);
    }

    #[test]
    fn migrate_keeps_sequence_and_text_and_trims_to_smaller() {
        let mut a = RingCore::new([Hdr::EMPTY; 16], [0u8; 256]);
        for s in 0..10u64 {
            a.push(rec(s), std::format!("t{s}").as_bytes());
        }
        let mut b = RingCore::new(vec![Hdr::EMPTY; 4], vec![0u8; 256]);
        assert_eq!(migrate(&a, &mut b), 10);
        assert_eq!(seqs(&b), [6, 7, 8, 9]);
        assert_eq!(b.text(&b.hdr(b.span().0)), "t6");
    }

    #[test]
    fn migrate_in_chunks_copies_wrapped_text_without_a_buffer() {
        let mut a = RingCore::new(vec![Hdr::EMPTY; 64], vec![0u8; 22]);
        for s in 0..9u64 {
            a.push(rec(s), std::format!("text{s}").as_bytes());
        }
        // 22 bytes hold four 5-byte texts; the store's end cuts one of them in two.
        assert_eq!(seqs(&a), [5, 6, 7, 8]);
        let (x, y) = a.span();
        assert!((x..y).any(|i| !a.text_parts(&a.hdr(i)).1.is_empty()), "one text wraps");
        let mut b = RingCore::new(vec![Hdr::EMPTY; 64], vec![0u8; 256]);
        let (next, done) = migrate_chunk(&a, &mut b, 0, 3);
        assert_eq!((next, done), (8, false));
        let (next, done) = migrate_chunk(&a, &mut b, next, 3);
        assert_eq!((next, done), (9, true));
        assert_eq!(seqs(&b), [5, 6, 7, 8]);
        let (x, y) = b.span();
        for i in x..y {
            let h = b.hdr(i);
            assert_eq!(b.text(&h), std::format!("text{}", h.seq));
        }
    }

    #[test]
    fn split_spends_the_whole_budget() {
        let (n, t) = split(4 << 20);
        assert!(n > 10_000, "about 4 MB holds tens of thousands of records");
        assert_eq!(n * core::mem::size_of::<Hdr>() + t, 4 << 20);
    }
}
