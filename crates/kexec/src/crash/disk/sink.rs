//! The panic path's disk record (CRASHDISK): lines streamed into the writer's buffer behind the slot header,
//! then the header with checksum and sequence number, then the polled write of exactly the slot's blocks. No
//! lock, no heap; every wait bounded (`BOUND_NS` on the panic clock).

use super::device::Run;
use super::format::{self, Head};
use super::{armed, Armed};
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

/// Bytes of the slot kept for the newest log lines.
const LOG_RESERVE: usize = 96 * 1024;
/// The longest the disk write may take (reset, queues, the writes): the panic's reset delay is 30 s by default
/// and the watchdog counts from that plus 30 s.
pub const BOUND_NS: u64 = 8_000_000_000;
/// Writes one slot may need (the extents' pieces).
const MAX_PIECES: usize = 2 * super::extents::MAX_RUNS;

/// The disk record has been committed once (a panic inside the write does not try again).
static DONE: AtomicBool = AtomicBool::new(false);

pub struct Sink {
    a: Option<&'static Armed>,
    off: usize,
    limit: usize,
    end: usize,
    pub cut: bool,
    /// Why there is no disk record (for the panic's own lines).
    pub off_why: &'static str,
}

impl Sink {
    pub fn open() -> Sink {
        let mut s = Sink { a: None, off: format::HEADER, limit: 0, end: 0, cut: false, off_why: "" };
        let Some(a) = armed() else {
            s.off_why = "not armed at this boot (see crash_info)";
            return s;
        };
        if let Err(why) = a.dev.ready() {
            s.off_why = why;
            return s;
        }
        s.end = a.dev.capacity().min(format::SLOT_BYTES);
        s.limit = s.end.saturating_sub(LOG_RESERVE);
        s.a = Some(a);
        s
    }

    fn put(&mut self, b: &[u8], to: usize) -> bool {
        let Some(a) = self.a else { return false };
        if self.off + b.len() > to {
            self.cut = true;
            return false;
        }
        if !a.dev.put(self.off, b) {
            self.cut = true;
            return false;
        }
        self.off += b.len();
        true
    }

    /// One line (before the log part: the log keeps its reserve).
    pub fn line(&mut self, s: &str) {
        let to = self.limit;
        if self.off + s.len() + 1 <= to {
            self.put(s.as_bytes(), to);
            self.put(b"\n", to);
        } else {
            self.cut = true;
        }
    }

    /// The newest log lines from before the panic (`before_seq`), as many as the rest holds.
    pub fn log_tail(&mut self, before_seq: u64) {
        if self.a.is_none() {
            return;
        }
        self.limit = self.end;
        self.line("log:");
        let room = self.end - self.off;
        let end = self.end;
        let got = klog::try_tail_visit_before(room, before_seq, &mut |b| {
            self.put(b, end);
        });
        if got.is_none() {
            self.line(super::super::LOG_BUSY);
        }
    }

    /// Header, checksum, the write. Ok: (slot, sequence, body bytes, ms the write took).
    pub fn commit(&mut self, h: Head, clock: &dyn Fn() -> u64) -> Result<(usize, u64, u32, u64), (&'static str, u32)> {
        let Some(a) = self.a else { return Err((self.off_why, 0)) };
        if DONE.swap(true, Ordering::AcqRel) {
            return Err(("the disk record was written already", 0));
        }
        let body = (self.off - format::HEADER) as u32;
        let h = Head { kind: format::KIND_RECORD, slot: a.slot as u16, seq: a.seq, body_len: body, tag: a.tag, ..h };
        let h = if self.cut { Head { flags: h.flags | format::flags::CUT, ..h } } else { h };
        let mut f = h.sum_start();
        let mut at = format::HEADER;
        let mut chunk = [0u8; 512];
        while at < self.off {
            let k = (self.off - at).min(chunk.len());
            if !a.dev.get(at, &mut chunk[..k]) {
                return Err(("the buffer could not be read back", 0));
            }
            f.update(&chunk[..k]);
            at += k;
        }
        if !a.dev.put(0, &h.sealed(f.finish())) {
            return Err(("the header could not be placed", 0));
        }
        let bs = a.dev.block_size().max(1);
        let len = self.off.next_multiple_of(bs) as u64;
        let mut runs = [Run::default(); MAX_PIECES];
        let n = a.ext.map((a.slot * format::SLOT_BYTES) as u64, len, 0, &mut runs).ok_or(("the slot does not map onto the area", 0))?;
        // Every block once more against the area and the device's one range, right before the write.
        let allowed = a.dev.allowed();
        for r in &runs[..n] {
            let blocks = r.blocks as u64;
            let in_dev = r.lba >= allowed.0 && r.lba.checked_add(blocks).is_some_and(|e| e <= allowed.0.saturating_add(allowed.1));
            if !a.ext.holds(r.lba, blocks) || !in_dev {
                return Err(("a block outside the area", 0));
            }
        }
        let t0 = clock();
        a.dev.write(&runs[..n], clock, BOUND_NS).map_err(|e| (e.stage, e.status))?;
        Ok((a.slot, a.seq, body, (clock().saturating_sub(t0)) / 1_000_000))
    }
}

/// The wall clock (unix s) at uptime `up_ns`, from what arming saw; 0 without one.
pub fn wall_at(up_ns: u64) -> i64 {
    match armed() {
        Some(a) if a.wall_s > 0 => a.wall_s + (up_ns.saturating_sub(a.up_ns) / 1_000_000_000) as i64,
        _ => 0,
    }
}

/// The memory line of the record (as last sampled; the panic path takes no zone lock) and the heap counters.
/// `counters`: also every non-zero `mm.heap.*` (the disk record; the RAM page keeps its room for the log).
pub fn memory_line(w: &mut dyn Write, now_ns: u64, counters: bool) {
    let (total, free, at) = (
        super::MEM_TOTAL.load(Ordering::Relaxed),
        super::MEM_FREE.load(Ordering::Relaxed),
        super::MEM_AT_NS.load(Ordering::Relaxed),
    );
    if at == 0 {
        let _ = write!(w, "memory: not sampled yet");
    } else {
        let _ = write!(w, "memory: mem.free {} MiB of {} MiB ({} s before)", free >> 20, total >> 20, now_ns.saturating_sub(at) / 1_000_000_000);
    }
    for (name, v) in klog::metrics::snapshot().filter(|_| counters) {
        if name.starts_with("mm.heap.") && v != 0 {
            let _ = write!(w, " {name}={v}");
        }
    }
}
