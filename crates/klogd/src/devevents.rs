//! RB-8 (B6): `/dev/bitsos/events`, the record reader of the journal service `bitsjournal` (root only, 0600:
//! events may carry peer addresses). Linux's model is `/dev/kmsg` (linux-6.6/kernel/printk/printk.c
//! devkmsg_open/devkmsg_read): every open file has its own position, a read hands out whole records only
//! (EINVAL when the first one does not fit, as devkmsg_read), records overwritten before they were read are
//! reported, a blocking read waits for new ones. Unlike Linux, the gap is a line of its own instead of EPIPE (the
//! journal wants to write down what was lost and go on), and three sources arrive in one stream: the log ring,
//! the per-CPU trace rings (RB-7a, without context switches: those stay in the ring) and the metric rounds
//! (RB-7b). One line per item, fields separated by one space, the text last (`\` `\n` `\r` escaped):
//!
//! - `B <boot> <reload> <next_seq> <now_ns> <wall_ms|-> <utc_offset_min>` first, once per open: `boot` is a
//!   random 64-bit id of this boot in hex (sequence numbers and times start at 0 again after every boot and
//!   Reload), `reload` kexec's counter.
//! - `E <seq> <time_ns> <wall_ms|-> <L> <cpu> <thread> <subsystem> <name> <text>` a log record (`text` = its
//!   rendered `key=value ... | message`).
//! - `T <cpu> <pos> <time_ns> <wall_ms|-> <L> <subsystem> <name> <text>` a trace entry.
//! - `M <round> <time_ns> <wall_ms|-> <series>=<value><unit> ...` a metric round (long rounds in several lines
//!   with the same head; units as MCP `metrics` labels them: % B B/s /s C W n).
//! - `G <before> <count>` log records lost to this reader; `g <cpu> <count>` trace entries lost.
//! - `F <target>` a Reload or reboot waits: make every record below `target` durable and ack it.
//! - `I <next_seq> <now_ns> <wall_ms|->` nothing new for a second (a blocking read's heartbeat). `next_seq` is
//!   this reader's own position: every record below it was handed out (never klog's newest number, which may
//!   already be ahead of what this reader saw).
//!
//! Writes: `A <next>` = every record with `seq < next` of this boot is durable (`klog::journal::note_ack`; the
//! first one attaches the reader as the journal a Reload waits for). A write is checked whole before any of its
//! lines is applied (EINVAL: none is); an ack above klog's next number is clamped to it. Wall times are the wall
//! clock's offset at read time added to the boot time (`-` without a wall clock).
//!
//! A read hands out at most `READ_MAX` bytes; buffers of 64 KiB or more read every line klog makes in practice
//! (a line longer than the buffer stays EINVAL, as devkmsg_read). poll/select/epoll: readable when log records,
//! a metric round or a flush request wait (Linux devkmsg_poll: POLLIN|POLLRDNORM only, no POLLOUT); trace entries
//! alone do not make it readable (context switches would wake it all the time). The node names no PollQueue:
//! the VFS looks again every 10 ms (`vfs poll.legacy`), so the klog append path stays as it is. lseek as
//! devkmsg_llseek: offset 0 only (ESPIPE otherwise), SEEK_SET/SEEK_DATA = from the oldest again (a new `B`
//! line), SEEK_END = only what comes from now on.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use klog::trace::{Entry, Kind};
use klog::{Filter, Level, Record};
use ksync::SpinLock;
use uapi::errno::*;
use vfs::{Cred, Errno, File};

/// Log records copied per ring read (klog copies at most 32 per lock hold anyway).
const PAGE: usize = 64;
/// A blocking read looks for new items this often.
const POLL_NS: u64 = 20_000_000;
/// A blocking read with nothing new answers the heartbeat after this long.
const IDLE_NS: u64 = 1_000_000_000;
/// Longest line of a metric round (a round with more series continues in another line).
const METRIC_LINE: usize = 3000;
/// The metric store is skipped for this read when busy longer.
const STORE_WAIT_NS: u64 = 2_000_000;
/// Most bytes one read makes and hands out (the buffer size is the caller's: a huge one must not copy the
/// whole ring at once).
pub const READ_MAX: usize = 256 << 10;

static READS: AtomicU64 = AtomicU64::new(0);
static LINES: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static BOOT: ksync::Once<u64> = ksync::Once::new();

/// This boot's random id (the journal's key besides the sequence number).
pub fn boot_id() -> u64 {
    *BOOT.get_or_init(|| {
        let mut b = [0u8; 8];
        vfs::random::fill(&mut b);
        // Never 0: the journal uses 0 for "unknown".
        u64::from_le_bytes(b).max(1)
    })
}

/// (reads, lines, bytes) handed out since boot (the cost numbers of the notes, B6).
pub fn stats() -> (u64, u64, u64) {
    (READS.load(Ordering::Relaxed), LINES.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed))
}

/// `\`, newline and carriage return escaped, so a record is one line.
pub fn escape_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
}

/// The wall clock's offset to boot time now, in ns (None: no wall time).
fn wall_offset() -> Option<i128> {
    time::wall::realtime_ns().map(|w| w as i128 - time::now_ns() as i128)
}

fn wall(out: &mut String, off: Option<i128>, time_ns: u64) {
    match off.map(|o| o + time_ns as i128).filter(|&w| w >= 0) {
        Some(w) => {
            let _ = write!(out, "{}", w / 1_000_000);
        }
        None => out.push('-'),
    }
}

pub fn record_line(out: &mut String, r: &Record, off: Option<i128>) {
    let _ = write!(out, "E {} {} ", r.seq, r.time_ns);
    wall(out, off, r.time_ns);
    let _ = write!(out, " {} {} {} {} {}", r.level.letter(), r.cpu, r.thread, r.subsystem, r.name);
    if !r.text.is_empty() {
        out.push(' ');
        escape_into(&r.text, out);
    }
    out.push('\n');
}

pub fn entry_line(out: &mut String, e: &Entry, off: Option<i128>) {
    let _ = write!(out, "T {} {} {} ", e.cpu, e.pos, e.time_ns);
    wall(out, off, e.time_ns);
    let _ = write!(out, " {} {} {} ", e.level().letter(), e.subsystem(), e.name());
    let mut t = String::new();
    let _ = e.write_text(&mut t);
    escape_into(&t, out);
    out.push('\n');
}

/// One reader's position in the three sources.
#[derive(Default)]
struct Cursor {
    started: bool,
    seq: u64,
    round: u64,
    /// Next position per trace ring (index = ring order of `klog::trace::rings`), grown when rings appear.
    trace: Vec<u64>,
    /// A ring whose entry at this position could not be read last time (being written): skipped if again.
    stalled: Vec<Option<u64>>,
    /// The flush request already shown.
    flush_shown: Option<u64>,
    /// Lines made but not handed out yet (whole lines).
    pending: String,
}

impl Cursor {
    fn head(&mut self, off: Option<i128>) {
        let now = time::now_ns();
        let reload = kexec::boot::facts().map_or(0, |f| f.counter);
        let _ = write!(self.pending, "B {:016x} {} {} {} ", boot_id(), reload, klog::next_seq(), now);
        wall(&mut self.pending, off, now);
        let _ = writeln!(self.pending, " {}", time::wall::utc_offset_minutes());
        self.started = true;
        for r in klog::trace::rings() {
            self.trace.push(r.span().0);
            self.stalled.push(None);
        }
    }

    fn log(&mut self, budget: usize, off: Option<i128>) {
        while self.pending.len() < budget {
            let page = klog::read(self.seq, PAGE, &Filter::ALL);
            for g in &page.gaps {
                let _ = writeln!(self.pending, "G {} {}", g.before, g.count);
            }
            for r in &page.records {
                record_line(&mut self.pending, r, off);
            }
            let moved = page.next != self.seq;
            self.seq = page.next;
            if page.records.len() < PAGE || page.busy || !moved {
                break;
            }
        }
    }

    fn trace(&mut self, budget: usize, off: Option<i128>) {
        for (i, r) in klog::trace::rings().enumerate() {
            if i >= self.trace.len() {
                self.trace.push(r.span().0);
                self.stalled.push(None);
            }
            let (old, next) = r.span();
            if self.trace[i] < old {
                let _ = writeln!(self.pending, "g {} {}", r.cpu(), old - self.trace[i]);
                self.trace[i] = old;
            }
            while self.trace[i] < next && self.pending.len() < budget {
                let p = self.trace[i];
                match r.get(p) {
                    Some(e) => {
                        if e.kind != Kind::Switch {
                            entry_line(&mut self.pending, &e, off);
                        }
                    }
                    None if self.stalled[i] != Some(p) => {
                        self.stalled[i] = Some(p);
                        break;
                    }
                    None => {
                        let _ = writeln!(self.pending, "g {} 1", r.cpu());
                    }
                }
                self.trace[i] = p + 1;
            }
        }
    }

    fn metrics(&mut self, budget: usize, off: Option<i128>) {
        let pending = &mut self.pending;
        let cur = &mut self.round;
        crate::metrics::with_store(STORE_WAIT_NS, |s| {
            let rounds = s.rounds();
            let mut round = (*cur).max(rounds.start);
            while round < rounds.end && pending.len() < budget {
                let Some(t) = s.time(round) else { break };
                let mut head = alloc::format!("M {round} {t} ");
                wall(&mut head, off, t);
                let mut line = head.clone();
                for (name, series) in s.series() {
                    let Some(v) = series.at(round) else { continue };
                    if line.len() > METRIC_LINE {
                        line.push('\n');
                        pending.push_str(&line);
                        line.clone_from(&head);
                    }
                    let _ = write!(line, " {name}=");
                    series.unit.write(v, &mut line);
                    line.push_str(series.unit.label());
                }
                line.push('\n');
                pending.push_str(&line);
                round += 1;
            }
            *cur = round;
        });
    }

    /// Skips everything there is now (SEEK_END): only the head line (if not shown yet) and what comes later.
    fn to_end(&mut self) {
        if self.started {
            self.pending.clear();
        } else {
            self.head(wall_offset());
        }
        self.seq = klog::next_seq();
        for (i, r) in klog::trace::rings().enumerate() {
            if i >= self.trace.len() {
                self.trace.push(0);
                self.stalled.push(None);
            }
            self.trace[i] = r.span().1;
        }
        if let Some(end) = crate::metrics::with_store(STORE_WAIT_NS, |s| s.rounds().end) {
            self.round = end;
        }
    }

    /// Fills `pending` with up to about `budget` bytes of new lines.
    fn produce(&mut self, budget: usize) {
        let off = wall_offset();
        if !self.started {
            self.head(off);
        }
        if let Some(t) = klog::journal::request()
            && self.flush_shown != Some(t)
        {
            let _ = writeln!(self.pending, "F {t}");
            self.flush_shown = Some(t);
        }
        self.log(budget, off);
        self.trace(budget, off);
        self.metrics(budget, off);
    }

    /// Moves whole lines that fit into `buf`.
    fn hand_out(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        let mut n = 0;
        for line in self.pending.split_inclusive('\n') {
            if n + line.len() > buf.len() {
                break;
            }
            n += line.len();
        }
        if n == 0 && !self.pending.is_empty() {
            return Err(EINVAL);
        }
        buf[..n].copy_from_slice(&self.pending.as_bytes()[..n]);
        LINES.fetch_add(self.pending[..n].matches('\n').count() as u64, Ordering::Relaxed);
        BYTES.fetch_add(n as u64, Ordering::Relaxed);
        self.pending.drain(..n);
        Ok(n)
    }
}

/// One open file of the node.
struct Reader {
    /// None while a read works on it (a second read of the same file meanwhile gets EAGAIN).
    cur: SpinLock<Option<Cursor>>,
    attached: AtomicBool,
}

impl Reader {
    /// Runs `f` on the cursor. Another read of the same open file holds it meanwhile: O_NONBLOCK gets EAGAIN,
    /// a blocking call waits for it (bounded: that read answers within about `IDLE_NS`; EINTR on a signal), as
    /// Linux serializes on the reader's mutex.
    fn with_cursor<R>(&self, nonblock: bool, f: impl FnOnce(&mut Cursor) -> R) -> Result<R, Errno> {
        let mut c = loop {
            if let Some(c) = self.cur.lock().take() {
                break c;
            }
            if nonblock {
                return Err(EAGAIN);
            }
            if vfs::dentry::interrupted() {
                return Err(EINTR);
            }
            sched::sleep_ns(POLL_NS);
        };
        let r = f(&mut c);
        *self.cur.lock() = Some(c);
        Ok(r)
    }

    fn read(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        self.with_cursor(nonblock, |c| Self::fill(c, buf, nonblock))?
    }

    /// Whether a read would find something now (poll; trace entries alone do not count, see the module doc).
    fn readable(&self) -> bool {
        let g = self.cur.lock();
        let Some(c) = g.as_ref() else { return true };
        if !c.started || !c.pending.is_empty() || klog::next_seq() > c.seq {
            return true;
        }
        if klog::journal::request().is_some_and(|t| c.flush_shown != Some(t)) {
            return true;
        }
        let round = c.round;
        drop(g);
        crate::metrics::with_store(0, |s| s.rounds().end > round.max(s.rounds().start)) == Some(true)
    }

    /// devkmsg_llseek: SEEK_SET/SEEK_DATA start from the oldest again, SEEK_END skips what is there now.
    fn seek(&self, off: i64, whence: u32) -> Result<u64, Errno> {
        if off != 0 {
            return Err(ESPIPE);
        }
        match whence {
            vfs::abi::SEEK_SET | vfs::abi::SEEK_DATA => self.with_cursor(false, |c| *c = Cursor::default())?,
            vfs::abi::SEEK_END => self.with_cursor(false, |c| c.to_end())?,
            _ => return Err(EINVAL),
        }
        Ok(0)
    }

    fn fill(c: &mut Cursor, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        READS.fetch_add(1, Ordering::Relaxed);
        let t0 = time::now_ns();
        loop {
            if c.pending.is_empty() {
                c.produce(buf.len().min(READ_MAX));
            }
            if !c.pending.is_empty() {
                return c.hand_out(buf);
            }
            if nonblock {
                return Err(EAGAIN);
            }
            if time::now_ns().saturating_sub(t0) >= IDLE_NS {
                let now = time::now_ns();
                // The cursor's own position, not klog::next_seq(): records logged since the last produce() are
                // above it and not handed out yet, so the journal must not ack them.
                let _ = write!(c.pending, "I {} {} ", c.seq, now);
                wall(&mut c.pending, wall_offset(), now);
                c.pending.push('\n');
                return c.hand_out(buf);
            }
            if vfs::dentry::interrupted() {
                return Err(EINTR);
            }
            sched::sleep_ns(POLL_NS);
        }
    }

    /// One command line: the ack it carries (EINVAL: not a command).
    fn parse_command(line: &str) -> Result<u64, Errno> {
        let mut w = line.split(' ');
        match (w.next(), w.next().and_then(|n| n.parse::<u64>().ok()), w.next()) {
            (Some("A"), Some(next), None) => Ok(next),
            _ => Err(EINVAL),
        }
    }

    fn ack(&self, next: u64) {
        let top = klog::next_seq();
        if next > top {
            klog::event!(Warn, "klog", "journal.ack_clamped", acked = next, next_seq = top; "an ack beyond the newest record: clamped");
        }
        if !self.attached.swap(true, Ordering::AcqRel) {
            klog::journal::attach();
            klog::event!(Info, "klog", "journal.attached", acked = next.min(top));
        }
        klog::journal::note_ack(next);
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        if self.attached.load(Ordering::Acquire) {
            klog::journal::detach();
            klog::event!(Info, "klog", "journal.detached", acked = klog::journal::acked());
        }
    }
}

impl File for Reader {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read(buf, true)
    }
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        self.read(buf, nonblock)
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        let text = core::str::from_utf8(buf).map_err(|_| EINVAL)?;
        // All lines checked before the first is applied: a bad line leaves the journal state as it was.
        let mut acks = Vec::new();
        for line in text.split('\n').map(|l| l.trim_end_matches('\r')).filter(|l| !l.is_empty()) {
            acks.push(Self::parse_command(line)?);
        }
        if let Some(&next) = acks.iter().max() {
            self.ack(next);
        }
        Ok(buf.len())
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o600
    }
    fn rdev(&self) -> u64 {
        rdev_number()
    }
    fn ioctl(&self, _cmd: u64, _arg: u64) -> Result<Vec<u8>, Errno> {
        Err(ENOTTY)
    }
    fn lseek(&self, off: i64, whence: u32) -> Option<Result<u64, Errno>> {
        Some(self.seek(off, whence))
    }
    fn events(&self) -> vfs::Events {
        vfs::Events(if self.readable() { vfs::Events::IN } else { 0 })
    }
}

/// A misc device number (major 10) from Linux's dynamic range (MISC_DYNAMIC_MINOR hands those out from 255
/// down); BitsOS has no other misc node there.
fn rdev_number() -> u64 {
    vfs::abi::encode_dev(10, 240)
}

/// The node itself: every open gets its own `Reader`.
struct Node;

fn new_reader() -> Arc<dyn File> {
    Arc::new(Reader { cur: SpinLock::named("rb.devevents", Some(Cursor::default())), attached: AtomicBool::new(false) })
}

impl File for Node {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o600
    }
    fn rdev(&self) -> u64 {
        rdev_number()
    }
    fn open_node(&self, _flags: u64, who: Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        // The node is 0600 root:root; refused here too, whatever its mode was changed to.
        if who.uid != 0 {
            return Some(Err(EACCES));
        }
        Some(Ok(new_reader()))
    }
}

fn make() -> Arc<dyn File> {
    Arc::new(Node)
}
vfs::devnode!("/dev/bitsos/events", make);

/// A reader for the ktests (as an open of the node gives one).
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn open_for_test() -> Arc<dyn File> {
    new_reader()
}

/// Level letters as the lines carry them (the journal parses them back).
pub fn level_of(letter: &str) -> Option<Level> {
    [Level::Error, Level::Warn, Level::Info, Level::Debug, Level::Trace].into_iter().find(|l| letter.starts_with(l.letter()) && letter.len() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_is_one_line_with_wall_time_and_escaped_text() {
        let r = Record { seq: 7, time_ns: 2_500_000_000, cpu: 1, thread: 42, level: Level::Warn, subsystem: "pm", name: "conn.closed", text: String::from("peer=1.2.3.4:443 | two\nlines \\ ok") };
        let mut s = String::new();
        record_line(&mut s, &r, Some(1_800_000_000_000_000_000));
        assert_eq!(s, "E 7 2500000000 1800000002500 W 1 42 pm conn.closed peer=1.2.3.4:443 | two\\nlines \\\\ ok\n");
        let mut s = String::new();
        record_line(&mut s, &Record { text: String::new(), ..r }, None);
        assert_eq!(s, "E 7 2500000000 - W 1 42 pm conn.closed\n");
    }

    #[test]
    fn whole_lines_only() {
        let mut c = Cursor { pending: String::from("E 1 a\nE 2 bb\n"), ..Cursor::default() };
        let mut b = [0u8; 8];
        assert_eq!(c.hand_out(&mut b), Ok(6));
        assert_eq!(&b[..6], b"E 1 a\n");
        let mut small = [0u8; 4];
        assert_eq!(c.hand_out(&mut small), Err(EINVAL), "a line that does not fit is EINVAL, as devkmsg_read");
        assert_eq!(c.pending, "E 2 bb\n", "and stays for a larger buffer");
        assert_eq!(level_of("W"), Some(Level::Warn));
        assert_eq!(level_of("WW"), None);
    }
}
