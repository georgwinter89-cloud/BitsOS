//! Readers with sequence numbers (MCP `log`, the deferred sinks, later bitsjournal): `read` from a sequence
//! number forward, `read_tail` for the newest records, both with filters by level, subsystem and name.
//!
//! The ring lock is held for at most a small batch of records at a time (copied into memory reserved before the
//! lock), so a large read never stalls emitters for long, and it is tried for a bounded time per batch: a reader
//! never hangs, it gets `Page::busy` and what it read so far (with `next` right after it), and simply asks again.
//! Holes in the sequence (records overwritten before the reader came, or dropped on the panic/NMI path because
//! the ring was busy) come back as `Gap`s, so a reader always knows what it missed.

use crate::ring::{Hdr, RingOps};
use crate::{Level, Record};
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Default)]
pub struct Filter<'a> {
    /// The most verbose level to include (`Warn` = errors and warnings).
    pub level: Option<Level>,
    /// A subsystem and its dotted sub-subsystems (`net` matches `net.dhcp`).
    pub subsystem: Option<&'a str>,
    /// A prefix of the event name (`link.` matches `link.up`).
    pub name: Option<&'a str>,
}

impl Filter<'_> {
    pub const ALL: Filter<'static> = Filter { level: None, subsystem: None, name: None };

    fn matches(&self, h: &Hdr) -> bool {
        self.accepts(h.level, h.subsystem, h.name)
    }

    /// The same test for anything with a level, subsystem and name (trace entries, RB-7a).
    pub fn accepts(&self, level: Level, subsystem: &str, name: &str) -> bool {
        self.level.is_none_or(|l| level <= l)
            && self.subsystem.is_none_or(|s| crate::levels::covers(s, subsystem))
            && self.name.is_none_or(|n| name.starts_with(n))
    }
}

struct Span<'f, 'a> {
    f: &'f Filter<'a>,
    from_ns: u64,
    to_ns: u64,
}

impl Span<'_, '_> {
    fn matches(&self, h: &Hdr) -> bool {
        (self.from_ns..=self.to_ns).contains(&h.time_ns) && self.f.matches(h)
    }
}

/// `count` sequence numbers right before `before` that the reader will never see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gap {
    pub before: u64,
    pub count: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Page {
    pub records: Vec<Record>,
    pub gaps: Vec<Gap>,
    /// Pass as `since` in the next call.
    pub next: u64,
    /// Sequence number of the oldest record still kept (= `next` when the ring is empty).
    pub oldest: u64,
    /// `since` was beyond the newest sequence number (a reader from before a restart); `next` starts over.
    pub ahead: bool,
    /// The ring lock could not be had in time (held too long, or this CPU is inside klog): the page holds only
    /// what was read before; call again with `next`.
    pub busy: bool,
}

impl Page {
    pub fn missed(&self) -> u64 {
        self.gaps.iter().map(|g| g.count).sum()
    }
}

/// Records copied out per ring-lock hold, the text bytes reserved for them, and the headers looked at per hold
/// (a narrow filter skips many). RB-klog: a hold copies at most this much into memory reserved before the lock
/// (no allocation under it: the old reader built a `String` per record with the ring held, and a busy heap made
/// that hold last milliseconds while every emitter waited).
const BATCH: usize = 32;
const BATCH_TEXT: usize = 8 << 10;
const SCAN: u64 = 512;

/// What one hold copies out: headers (`text_pos` = offset into `text`), their text, the gaps in front of them.
struct Batch {
    hdrs: Vec<Hdr>,
    text: Vec<u8>,
    gaps: Vec<Gap>,
}

impl Batch {
    /// Allocates; call outside the lock.
    fn new() -> Self {
        Batch { hdrs: Vec::with_capacity(BATCH), text: Vec::with_capacity(BATCH_TEXT), gaps: Vec::with_capacity(BATCH) }
    }
    fn clear(&mut self) {
        self.hdrs.clear();
        self.text.clear();
        self.gaps.clear();
    }
    fn len(&self) -> usize {
        self.hdrs.len()
    }
    /// Room for one more record of `len` text bytes without growing (an empty batch takes any record, cut to
    /// the room it has).
    fn fits(&self, len: usize) -> bool {
        self.hdrs.is_empty() || (self.hdrs.len() < self.hdrs.capacity() && self.text.len() + len <= self.text.capacity())
    }
    fn gap_room(&self) -> bool {
        self.gaps.len() < self.gaps.capacity()
    }
    /// Under the lock: copies the record (never grows the buffers).
    fn take(&mut self, r: &dyn RingOps, h: &Hdr) {
        let (a, b) = r.text_parts(h);
        let at = self.text.len();
        for part in [a, b] {
            let room = self.text.capacity() - self.text.len();
            self.text.extend_from_slice(&part[..part.len().min(room)]);
        }
        self.hdrs.push(Hdr { text_pos: at as u64, text_len: (self.text.len() - at) as u32, ..*h });
    }
    /// After the lock: the records (allocates their strings).
    fn records(&self) -> impl Iterator<Item = Record> + '_ {
        self.hdrs.iter().map(|h| {
            let t = &self.text[h.text_pos as usize..h.text_pos as usize + h.text_len as usize];
            Record {
                seq: h.seq,
                time_ns: h.time_ns,
                cpu: h.cpu,
                thread: h.thread,
                level: h.level,
                subsystem: h.subsystem,
                name: h.name,
                // Text is cut at char boundaries when stored; lossy keeps a reader alive even if that ever slips.
                text: String::from_utf8_lossy(t).into_owned(),
            }
        })
    }
}

fn oldest(r: &dyn RingOps, seq_now: u64) -> u64 {
    let (a, b) = r.span();
    if a == b { seq_now } else { r.hdr(a).seq }
}

/// Records with `seq >= since` that match `f`, at most `max`, oldest first.
pub fn read(since: u64, max: usize, f: &Filter<'_>) -> Page {
    read_span(since, max, f, 0, u64::MAX)
}

/// `read` limited to records stamped `from_ns..=to_ns` (MCP `events`; the time filter runs under the ring lock
/// like the others, so records outside the window are never copied).
pub fn read_span(since: u64, max: usize, f: &Filter<'_>, from_ns: u64, to_ns: u64) -> Page {
    let f = &Span { f, from_ns, to_ns };
    let mut page = Page { next: since, ..Page::default() };
    if max == 0 {
        return page;
    }
    let mut expected = since;
    let mut b = Batch::new();
    loop {
        b.clear();
        let (seq_now, at_end) = {
            let Some(g) = crate::lock_ring(crate::Wait::Bounded(crate::READ_WAIT_NS)) else {
                page.busy = true;
                page.next = expected;
                return page;
            };
            let seq_now = crate::next_seq();
            let r = g.active_ref();
            page.oldest = oldest(r, seq_now);
            if since > seq_now {
                page.ahead = true;
                page.next = seq_now;
                return page;
            }
            let end = r.span().1;
            let mut i = r.find(expected);
            let stop = i.saturating_add(SCAN).min(end);
            while i < stop {
                let h = r.hdr(i);
                let m = f.matches(&h);
                // A record is consumed (and the gap in front of it reported) only if it is also taken.
                if m && (page.records.len() + b.len() >= max || !b.fits(h.text_len as usize)) {
                    break;
                }
                if h.seq > expected {
                    if !b.gap_room() {
                        break;
                    }
                    b.gaps.push(Gap { before: h.seq, count: h.seq - expected });
                }
                expected = h.seq + 1;
                i += 1;
                if m {
                    b.take(r, &h);
                }
            }
            (seq_now, i >= end)
        };
        page.gaps.extend_from_slice(&b.gaps);
        page.records.extend(b.records());
        if page.records.len() >= max {
            page.next = expected;
            return page;
        }
        if at_end {
            if seq_now > expected {
                page.gaps.push(Gap { before: seq_now, count: seq_now - expected });
            }
            page.next = seq_now.max(expected);
            return page;
        }
    }
}

/// The newest `max` records that match `f`, oldest first (a snapshot: no gaps reported).
pub fn read_tail(max: usize, f: &Filter<'_>) -> Page {
    read_tail_span(max, f, 0, u64::MAX)
}

/// `read_tail` limited to records stamped `from_ns..=to_ns`.
pub fn read_tail_span(max: usize, f: &Filter<'_>, from_ns: u64, to_ns: u64) -> Page {
    let f = &Span { f, from_ns, to_ns };
    let mut page = Page::default();
    let mut out: Vec<Record> = Vec::new();
    let mut pos: Option<u64> = None;
    let mut b = Batch::new();
    while out.len() < max {
        b.clear();
        let done = {
            let Some(g) = crate::lock_ring(crate::Wait::Bounded(crate::READ_WAIT_NS)) else {
                page.busy = true;
                if pos.is_none() {
                    page.next = crate::next_seq();
                }
                break;
            };
            let seq_now = crate::next_seq();
            let r = g.active_ref();
            let (head, end) = r.span();
            page.oldest = oldest(r, seq_now);
            page.next = seq_now;
            // Records below `head` were overwritten since the last hold; the tail ends there.
            let mut i = match pos {
                Some(p) if p <= head => break,
                Some(p) => p,
                None => end,
            };
            let stop = i.saturating_sub(SCAN).max(head);
            while i > stop && out.len() + b.len() < max {
                let h = r.hdr(i - 1);
                if f.matches(&h) {
                    if !b.fits(h.text_len as usize) {
                        break;
                    }
                    b.take(r, &h);
                }
                i -= 1;
            }
            pos = Some(i);
            i <= head
        };
        // Newest first within the batch, like `out`.
        out.extend(b.records());
        if done {
            break;
        }
    }
    out.reverse();
    page.records = out;
    page
}
