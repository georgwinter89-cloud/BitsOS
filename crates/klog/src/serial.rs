//! Serial output (COM1 in QEMU; on the Mini-PC no port, and before the console the early screen console behind
//! hal's byte hook) as a printer that works from the ring (RB-klog).
//!
//! Before, every emitter took a klog serial lock and wrote its own line with interrupts off: the other CPUs
//! waited for each line in turn (a real 16550 at 115200 baud needs 87 us a byte, the early screen console draws
//! every byte, QEMU exits to the host for every port write), and a caller with interrupts already off stayed
//! deaf for all of it. Now, as Linux's `console_unlock` does (linux-6.6/kernel/printk/printk.c:3012,
//! `console_flush_all` at :2945, one record per `console_emit_next_record` at :2864): the emitter stores its
//! record and then tries the printer flag once. The winner prints every record from `PRINTED` up to the
//! newest, its own line straight from its stack and the
//! others copied out of the ring one at a time (one short ring hold each, the text only; the prefix is
//! rendered after the lock). Whoever finds the flag taken just goes on: the holder looks again after letting
//! go, so no stored record is left behind. The printer keeps interrupts on between lines (hal masks them only
//! for one line) and holds a preempt guard, so it is not switched away while the others count on it.
//!
//! Once klogd runs, one emitter prints at most `HANDOFF_LINES` lines for the others; the rest is klogd's
//! (`flush` from `drain_deferred`, `pending` wakes it), so no thread prints for a whole storm. Records that
//! could not be stored (ring busy) are written by their emitter straight away, and the panic/NMI path writes
//! its own lines directly (and marks them, so the printer does not repeat them).

use crate::lock::{EMIT_WAIT_NS, Wait, cpu_tag};
use crate::ring::Hdr;
use crate::{next_seq, render};
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

static ON: AtomicBool = AtomicBool::new(true);
/// CPU tag of the printer (0 = none).
static PRINTER: AtomicUsize = AtomicUsize::new(0);
/// The next sequence number serial has not had.
static PRINTED: AtomicU64 = AtomicU64::new(0);

/// Lines one emitter prints for the others once klogd runs.
const HANDOFF_LINES: usize = 32;
/// Room for the prefix (`[ 123456.123456] I subsystem name`; both names are short static names).
const PREFIX_MAX: usize = 160;

pub(crate) fn set_on(on: bool) {
    if !on {
        ON.store(false, Ordering::Release);
    } else if !ON.swap(true, Ordering::AcqRel) {
        // What came while serial was off stays in the ring only.
        PRINTED.fetch_max(next_seq(), Ordering::AcqRel);
    }
}

pub(crate) fn on() -> bool {
    ON.load(Ordering::Acquire)
}

/// Serial has stored records it has not printed yet (klogd's wake-up check).
pub fn pending() -> bool {
    on() && PRINTED.load(Ordering::Acquire) < next_seq()
}

/// After `record` stored `line` as `seq`.
pub(crate) fn stored(seq: u64, line: &str) {
    if on() {
        // A caller with interrupts off (an IRQ handler, an `IrqSpinLock` holder) prints one line, as it did
        // before (its own if it is next); the others stay deaf to IPIs for no one else's lines.
        let budget = if !crate::lock::irqs_on() {
            1
        } else if crate::drainer() {
            HANDOFF_LINES
        } else {
            usize::MAX
        };
        flush(Some((seq, line)), budget);
    }
}

/// A record the ring did not take (busy): only its emitter has it.
pub(crate) fn unstored(line: &str) {
    if on() {
        hal::serial::write_line(line.as_bytes());
    }
}

/// Prints stored records up to the newest (at most `budget` lines) if no other CPU prints right now; `own` is
/// the caller's record, printed from its buffer. Returns the lines printed.
pub fn flush(own: Option<(u64, &str)>, budget: usize) -> usize {
    if !on() {
        return 0;
    }
    let me = cpu_tag();
    let mut printed = 0;
    loop {
        // SeqCst against `record` (sequence number taken, then this flag tried): see the module text.
        if PRINTER.compare_exchange(0, me, Ordering::SeqCst, Ordering::Relaxed).is_err() {
            return printed;
        }
        let pg = ksync::PreemptGuard::new();
        let busy = print_some(own, budget, &mut printed);
        PRINTER.swap(0, Ordering::SeqCst);
        drop(pg);
        if busy || printed >= budget || !pending() {
            return printed;
        }
    }
}

/// The printer's loop (caller holds `PRINTER`). True if the ring stayed busy (try again later).
fn print_some(own: Option<(u64, &str)>, budget: usize, printed: &mut usize) -> bool {
    while *printed < budget {
        let next = PRINTED.load(Ordering::Acquire);
        if next >= next_seq() {
            return false;
        }
        if let Some((s, line)) = own
            && s == next
        {
            hal::serial::write_line(line.as_bytes());
            PRINTED.store(s + 1, Ordering::Release);
            *printed += 1;
            continue;
        }
        let mut buf = PrintBuf::new();
        match copy_out(next, &mut buf, Wait::Bounded(EMIT_WAIT_NS)) {
            Copied::Line { seq, direct } => {
                if !direct {
                    hal::serial::write_line(buf.line());
                    *printed += 1;
                }
                PRINTED.store(seq + 1, Ordering::Release);
            }
            // Every number in between belongs to a record the ring never took (its emitter wrote it).
            Copied::None { seq_now } => PRINTED.store(seq_now.max(next), Ordering::Release),
            Copied::Busy => return true,
        }
    }
    false
}

enum Copied {
    Line { seq: u64, direct: bool },
    None { seq_now: u64 },
    Busy,
}

/// One line: the text copied under the ring lock after `PREFIX_MAX` bytes of room, the prefix put in front of it
/// afterwards.
struct PrintBuf {
    b: [u8; PREFIX_MAX + 1 + render::LINE_BYTES],
    start: usize,
    end: usize,
}

impl PrintBuf {
    fn new() -> Self {
        PrintBuf { b: [0; PREFIX_MAX + 1 + render::LINE_BYTES], start: PREFIX_MAX, end: PREFIX_MAX }
    }
    fn line(&self) -> &[u8] {
        &self.b[self.start..self.end]
    }
    /// Under the ring lock: the text only.
    fn put_text(&mut self, a: &[u8], b: &[u8]) {
        if a.is_empty() && b.is_empty() {
            return;
        }
        let mut at = PREFIX_MAX + 1;
        for part in [a, b] {
            let n = part.len().min(self.b.len() - at);
            self.b[at..at + n].copy_from_slice(&part[..n]);
            at += n;
        }
        self.b[PREFIX_MAX] = b' ';
        self.end = at;
    }
    /// After the lock: the prefix right in front of the text.
    fn put_prefix(&mut self, h: &Hdr) {
        let mut p = Prefix { b: [0; PREFIX_MAX], n: 0 };
        render::prefix(&mut p, h.time_ns, h.level, h.subsystem, h.name);
        self.start = PREFIX_MAX - p.n;
        self.b[self.start..PREFIX_MAX].copy_from_slice(&p.b[..p.n]);
    }
}

struct Prefix {
    b: [u8; PREFIX_MAX],
    n: usize,
}

impl Write for Prefix {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let k = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

/// The first kept record with `seq >= next`, copied out in one short ring hold.
#[inline(never)]
fn copy_out(next: u64, buf: &mut PrintBuf, wait: Wait) -> Copied {
    let h = {
        let Some(g) = crate::lock_ring(wait) else { return Copied::Busy };
        let r = g.active_ref();
        let seq_now = next_seq();
        let i = r.find(next);
        if i >= r.span().1 {
            return Copied::None { seq_now };
        }
        let h = r.hdr(i);
        if !h.direct {
            let (a, b) = r.text_parts(&h);
            buf.put_text(a, b);
        }
        h
    };
    if !h.direct {
        buf.put_prefix(&h);
    }
    Copied::Line { seq: h.seq, direct: h.direct }
}

/// Time the panic path spends on lines serial had not printed yet (`flush_emergency`). hal's `write_line` waits
/// up to 4,000,000 tries for its line flag with interrupts off and then writes anyway; a CPU stopped by the panic
/// NMI in the middle of a line keeps that flag, so every line after it pays the whole wait (milliseconds each,
/// more in QEMU). The budget keeps the report and the `panic=reboot` reset from slipping behind 256 of those.
pub const EMERGENCY_BUDGET_NS: u64 = 20_000_000;

/// The panic path: prints what serial has not had yet, at most `max` lines and for at most
/// `EMERGENCY_BUDGET_NS` (the first line always goes; without a clock only `max` counts), without the printer
/// flag (the other CPUs are stopped, one of them may hold it) and with one try of the ring per line (a stopped
/// holder only ends it early). No allocation. Returns the lines printed.
pub fn flush_emergency(max: usize) -> usize {
    flush_emergency_within(max, EMERGENCY_BUDGET_NS, crate::now)
}

/// `flush_emergency` with its budget and clock given (the host test drives a clock of its own).
pub(crate) fn flush_emergency_within(max: usize, budget_ns: u64, clock: fn() -> u64) -> usize {
    if !on() {
        return 0;
    }
    let t0 = clock();
    let mut printed = 0;
    for _ in 0..max {
        let next = PRINTED.load(Ordering::Acquire);
        if next >= next_seq() {
            return printed;
        }
        if printed > 0 && clock().saturating_sub(t0) >= budget_ns {
            // B6 in the lock-free form: the rest stays in the ring (and the crash record); serial goes on with
            // the report. This line goes out directly and is marked, so no printer repeats it.
            let left = next_seq().saturating_sub(next);
            crate::try_event!(Warn, "klog", "serial.emergency_cut", printed = printed, left = left,
                budget_ms = budget_ns / 1_000_000);
            return printed;
        }
        let mut buf = PrintBuf::new();
        match copy_out(next, &mut buf, Wait::Never) {
            Copied::Line { seq, direct } => {
                if !direct {
                    hal::serial::write_line(buf.line());
                    printed += 1;
                }
                PRINTED.store(seq + 1, Ordering::Release);
            }
            Copied::None { seq_now } => {
                PRINTED.store(seq_now.max(next), Ordering::Release);
                return printed;
            }
            Copied::Busy => return printed,
        }
    }
    printed
}

/// Puts the printer back to the newest record and frees its flag (tests that turn serial on and off).
#[cfg(test)]
pub(crate) fn __reset() {
    PRINTER.store(0, Ordering::SeqCst);
    PRINTED.store(next_seq(), Ordering::SeqCst);
}

/// Makes serial owe every record from `seq` on again (the emergency budget test).
#[cfg(test)]
pub(crate) fn __rewind(seq: u64) {
    PRINTED.store(seq, Ordering::SeqCst);
}
