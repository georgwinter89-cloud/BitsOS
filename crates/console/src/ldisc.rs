//! The N_TTY line discipline as data (IN-4): what Linux 6.6's drivers/tty/n_tty.c does with the bytes the
//! keyboard sends and the bytes programs write, driven by `termios`. No locks and no waiting here (`tty.rs` adds
//! them), so every rule is host-tested.
//!
//! Followed exactly (n_tty.c line numbers of linux-6.6):
//! - input: ISTRIP, IUCLC (with IEXTEN), IGNCR/ICRNL/INLCR, IXON/IXANY flow control (`n_tty_receive_char_flow_ctrl`
//!   :1243), ISIG with VINTR/VQUIT/VSUSP and the flush unless NOFLSH (`isig` :1095, `n_tty_receive_signal_char`
//!   :1211), canonical editing with VERASE/VKILL/VWERASE (`eraser` :953, UTF-8 aware with IUTF8), VLNEXT,
//!   VREPRINT, VEOF/VEOL/VEOL2 (`n_tty_receive_char_canon` :1274), the special-character map
//!   (`n_tty_set_termios` :1795), the 4096-byte read buffer and its canonical overflow rule
//!   (`n_tty_receive_buf_common` :1689);
//! - echo: ECHO, ECHOE, ECHOK, ECHOKE, ECHONL, ECHOCTL (`^X`), ECHOPRT, the tab erase from the canon column
//!   (`n_tty_process_echo_ops` :585), held back while output is stopped;
//! - reads: one line per canonical read without the EOF character, the EOF skip after a full buffer
//!   (`canon_copy_from_read_buf` :2012, `canon_skip_eof` :2080), switching ICANON with data pending pushes it as a
//!   line (:1799-1813), `input_available_p` (:1929) with VMIN for poll, TIOCINQ counting (`inq_canon` :2479);
//! - output: OPOST with ONLCR, OCRNL, ONOCR, ONLRET, OLCUC, XTABS and the column (`do_output_char` :417).
//!
//! BitsOS addition: each byte remembers whether its key was injected by MCP (`send_keys`), so the monitor can
//! refuse physical-key-only commands typed by injection (R8c). An injected erase that removes a typed byte marks
//! the line too (injection changed what Georg typed).

use crate::termios::{Termios, cc::*, flag::*};
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

/// `N_TTY_BUF_SIZE`: the read buffer of one terminal (a bound against a reader that stopped, as Linux's).
pub const BUF_SIZE: usize = 4096;

/// Echo kept while output is stopped: `ECHO_DISCARD_WATERMARK` of n_tty.c (N_TTY_BUF_SIZE - (256 + 32)); older
/// echo is dropped beyond it, as Linux discards the echo buffer's tail (`__process_echoes`).
pub const HELD_ECHO_MAX: usize = BUF_SIZE - (256 + 32);

pub const SIGINT: u32 = 2;
pub const SIGQUIT: u32 = 3;
pub const SIGTSTP: u32 = 20;

/// The EOF character's place in the buffer (`__DISABLED_CHAR`): a line end that is never read.
const DISABLED: u8 = 0;

#[derive(Clone, Copy, Debug)]
struct Cell {
    c: u8,
    /// A line ends here (Linux `read_flags`).
    eol: bool,
    injected: bool,
}

/// A finished input line for the kernel's own reader (the monitor).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    /// Some key of the line (or its end) came from MCP injection, or injection erased a typed character.
    pub injected: bool,
    /// The line was only the EOF character (Ctrl+D on an empty line).
    pub eof: bool,
    /// Ended by a newline (false for an EOF hand-over or an EOL character).
    pub newline: bool,
}

/// What a batch of input caused, for the caller to act on after it released the line discipline.
#[derive(Default, Debug)]
pub struct Effects {
    /// Bytes to put on the screen (after output processing).
    pub echo: Vec<u8>,
    /// Signals for the foreground process group (ISIG).
    pub signals: Vec<u32>,
    /// Readers may have something now.
    pub readable: bool,
    /// Output was started again (writers waiting for ^Q go on).
    pub started: bool,
    /// Input was dropped because the buffer was full.
    pub dropped: bool,
    /// Held echo was dropped while output was stopped (once per stop).
    pub echo_lost: bool,
}

pub struct Ldisc {
    pub t: Termios,
    buf: VecDeque<Cell>,
    /// Cells at the front that belong to finished lines (`canon_head - read_tail`).
    canon: usize,
    /// Cells at the front readers may take (`publish`, after the echo was drawn; Linux's `commit_head` and the
    /// published `canon_head`).
    published: usize,
    icanon: bool,
    lnext: bool,
    erasing: bool,
    /// The pending data became a line when ICANON was switched on (`push`).
    push: bool,
    pub column: u32,
    canon_column: u32,
    /// Output stopped by the STOP character (`flow.stopped`) or TCXONC TCOOFF (`tco_stopped`).
    stopped: bool,
    tco_stopped: bool,
    /// Echo waiting for output to start again.
    held_echo: Vec<u8>,
    /// Held echo was dropped during this stop (logged once).
    echo_lost: bool,
    /// Injection erased a typed byte of the line being edited.
    tampered: bool,
    /// Dropped-input runs (one per run, so a held key counts once).
    pub dropped_runs: u64,
    dropping: bool,
    /// VINTR characters seen with ISIG (the monitor redraws its prompt on a change).
    pub interrupts: u64,
}

fn iscntrl(c: u8) -> bool {
    c < 0x20 || c == 0x7F
}
/// lib/ctype.c: ASCII and Latin-1 letters and digits.
fn isalnum(c: u8) -> bool {
    c.is_ascii_alphanumeric() || (0xC0..=0xFF).contains(&c) && c != 0xD7 && c != 0xF7
}
fn islower(c: u8) -> bool {
    c.is_ascii_lowercase() || (0xDF..=0xFF).contains(&c) && c != 0xF7
}
fn isupper(c: u8) -> bool {
    c.is_ascii_uppercase() || (0xC0..=0xDE).contains(&c) && c != 0xD7
}
fn tolower(c: u8) -> u8 {
    if isupper(c) { c + 32 } else { c }
}
fn toupper(c: u8) -> u8 {
    if islower(c) { c - 32 } else { c }
}

impl Ldisc {
    pub const fn new(t: Termios) -> Ldisc {
        Ldisc {
            icanon: t.lflag & ICANON != 0,
            t,
            buf: VecDeque::new(),
            canon: 0,
            published: 0,
            lnext: false,
            erasing: false,
            push: false,
            column: 0,
            canon_column: 0,
            stopped: false,
            tco_stopped: false,
            held_echo: Vec::new(),
            echo_lost: false,
            tampered: false,
            dropped_runs: 0,
            dropping: false,
            interrupts: 0,
        }
    }

    fn continuation(&self, c: u8) -> bool {
        self.t.i(IUTF8) && c & 0xC0 == 0x80
    }

    /// Whether `c` is in Linux's `char_map` (and so goes through `receive_special`).
    fn special(&self, c: u8) -> bool {
        let t = &self.t;
        if c == DISABLED {
            return false;
        }
        let canon = t.l(ICANON);
        let ext = canon && t.l(IEXTEN);
        ((t.i(IGNCR) || t.i(ICRNL)) && c == b'\r')
            || (t.i(INLCR) && c == b'\n')
            || (canon && (c == t.c(VERASE) || c == t.c(VKILL) || c == t.c(VEOF) || c == b'\n' || c == t.c(VEOL)))
            || (ext && (c == t.c(VWERASE) || c == t.c(VLNEXT) || c == t.c(VEOL2)))
            || (ext && t.l(ECHO) && c == t.c(VREPRINT))
            || (t.i(IXON) && (c == t.c(VSTART) || c == t.c(VSTOP)))
            || (t.l(ISIG) && (c == t.c(VINTR) || c == t.c(VQUIT) || c == t.c(VSUSP)))
    }

    // ---- input

    /// Bytes from the keyboard (one key's worth, usually). `injected`: they came from MCP injection.
    pub fn receive(&mut self, bytes: &[u8], injected: bool) -> Effects {
        self.receive_inner(bytes, injected, false).0
    }

    /// `receive` with flow control (IN-7, a pty's input: Linux's flip buffer keeps what the line discipline has
    /// no room for, `n_tty_receive_buf_common` stops at `receive_room`): stops at the first byte without room
    /// instead of dropping it. Returns the effects and how many bytes were taken.
    pub fn receive_flow(&mut self, bytes: &[u8], injected: bool) -> (Effects, usize) {
        self.receive_inner(bytes, injected, true)
    }

    fn receive_inner(&mut self, bytes: &[u8], injected: bool, flow: bool) -> (Effects, usize) {
        let mut fx = Effects::default();
        let mut taken = bytes.len();
        for (i, &b) in bytes.iter().enumerate() {
            if !self.room() {
                if flow {
                    taken = i;
                    break;
                }
                if !self.dropping {
                    self.dropping = true;
                    self.dropped_runs += 1;
                }
                fx.dropped = true;
                continue;
            }
            self.dropping = false;
            self.one(b, injected, &mut fx);
        }
        if !fx.echo.is_empty() && self.stopped {
            self.held_echo.append(&mut fx.echo);
            let over = self.held_echo.len().saturating_sub(HELD_ECHO_MAX);
            if over > 0 {
                self.held_echo.drain(..over);
                fx.echo_lost = !core::mem::replace(&mut self.echo_lost, true);
            }
        }
        (fx, taken)
    }

    /// What was received becomes visible to readers (the caller drew the echo first).
    pub fn publish(&mut self) {
        self.published = if self.icanon && !self.t.l(EXTPROC) { self.canon } else { self.buf.len() };
    }

    /// Linux's room check: the buffer keeps one byte free; in canonical mode without a finished line the newest byte
    /// is overwritten instead, so erase characters and the line end still work.
    fn room(&mut self) -> bool {
        let used = self.buf.len();
        if used < BUF_SIZE - 1 {
            return true;
        }
        if self.icanon && self.canon == 0 {
            if used >= BUF_SIZE {
                self.buf.pop_back();
            }
            return true;
        }
        false
    }

    fn one(&mut self, mut c: u8, injected: bool, fx: &mut Effects) {
        if self.lnext {
            self.lnext = false;
            if self.t.i(ISTRIP) {
                c &= 0x7F;
            }
            if self.t.i(IUCLC) && self.t.l(IEXTEN) {
                c = tolower(c);
            }
            self.plain(c, injected, fx);
            return;
        }
        if self.t.i(ISTRIP) {
            c &= 0x7F;
        }
        if self.t.i(IUCLC) && self.t.l(IEXTEN) {
            c = tolower(c);
        }
        if self.t.l(EXTPROC) {
            self.put(c, injected, false);
            fx.readable |= !self.icanon;
            return;
        }
        if self.special(c) {
            self.receive_special(c, injected, fx);
        } else {
            self.plain(c, injected, fx);
        }
    }

    /// `n_tty_receive_char`.
    fn plain(&mut self, c: u8, injected: bool, fx: &mut Effects) {
        if self.stopped && !self.tco_stopped && self.t.i(IXON) && self.t.i(IXANY) {
            self.start(fx);
        }
        if self.t.l(ECHO) {
            self.finish_erasing(fx);
            if self.at_line_start() {
                self.canon_column = self.column;
            }
            self.echo_char(c, fx);
        }
        if c == 0xFF && self.t.i(PARMRK) {
            self.put(c, injected, false);
        }
        self.put(c, injected, false);
        fx.readable |= !self.icanon;
    }

    /// `n_tty_receive_char_special`.
    fn receive_special(&mut self, mut c: u8, injected: bool, fx: &mut Effects) {
        let t = self.t;
        if t.i(IXON) && (c == t.c(VSTART) || c == t.c(VSTOP)) {
            if c == t.c(VSTART) {
                self.start(fx);
            } else {
                self.stopped = true;
            }
            return;
        }
        if t.l(ISIG) {
            let sig = if c == t.c(VINTR) {
                Some(SIGINT)
            } else if c == t.c(VQUIT) {
                Some(SIGQUIT)
            } else if c == t.c(VSUSP) {
                Some(SIGTSTP)
            } else {
                None
            };
            if let Some(sig) = sig {
                self.signal_char(sig, c, fx);
                return;
            }
        }
        if self.stopped && !self.tco_stopped && t.i(IXON) && t.i(IXANY) {
            self.start(fx);
        }
        if c == b'\r' {
            if t.i(IGNCR) {
                return;
            }
            if t.i(ICRNL) {
                c = b'\n';
            }
        } else if c == b'\n' && t.i(INLCR) {
            c = b'\r';
        }
        if self.icanon && self.receive_canon(c, injected, fx) {
            return;
        }
        if t.l(ECHO) {
            self.finish_erasing(fx);
            if c == b'\n' {
                self.echo_raw(b'\n', fx);
            } else {
                if self.at_line_start() {
                    self.canon_column = self.column;
                }
                self.echo_char(c, fx);
            }
        }
        if c == 0xFF && t.i(PARMRK) {
            self.put(c, injected, false);
        }
        self.put(c, injected, false);
        fx.readable |= !self.icanon;
    }

    /// `isig` + `n_tty_receive_signal_char`.
    fn signal_char(&mut self, sig: u32, c: u8, fx: &mut Effects) {
        if sig == SIGINT {
            self.interrupts += 1;
        }
        fx.signals.push(sig);
        if !self.t.l(NOFLSH) {
            // Signal and flush: the pending echo, the output and the input go.
            fx.echo.clear();
            self.held_echo.clear();
            self.reset_buffer();
            fx.readable = true;
        }
        if self.t.i(IXON) {
            self.start(fx);
        }
        if self.t.l(ECHO) {
            self.echo_char(c, fx);
        }
    }

    /// `n_tty_receive_char_canon`; true when `c` was consumed here.
    fn receive_canon(&mut self, c: u8, injected: bool, fx: &mut Effects) -> bool {
        let t = self.t;
        if c == t.c(VERASE) || c == t.c(VKILL) || (c == t.c(VWERASE) && t.l(IEXTEN)) {
            self.eraser(c, injected, fx);
            return true;
        }
        if c == t.c(VLNEXT) && t.l(IEXTEN) {
            self.lnext = true;
            if t.l(ECHO) {
                self.finish_erasing(fx);
                if t.l(ECHOCTL) {
                    self.echo_raw(b'^', fx);
                    self.echo_raw(b'\x08', fx);
                }
            }
            return true;
        }
        if c == t.c(VREPRINT) && t.l(ECHO) && t.l(IEXTEN) {
            self.finish_erasing(fx);
            self.echo_char(c, fx);
            self.echo_raw(b'\n', fx);
            let line: Vec<u8> = self.buf.iter().skip(self.canon).map(|x| x.c).collect();
            for b in line {
                self.echo_char(b, fx);
            }
            return true;
        }
        if c == b'\n' {
            if t.l(ECHO) || t.l(ECHONL) {
                self.echo_raw(b'\n', fx);
            }
            self.newline(c, injected, fx);
            return true;
        }
        if c == t.c(VEOF) {
            self.newline(DISABLED, injected, fx);
            return true;
        }
        if c == t.c(VEOL) || (c == t.c(VEOL2) && t.l(IEXTEN)) {
            if t.l(ECHO) {
                if self.at_line_start() {
                    self.canon_column = self.column;
                }
                self.echo_char(c, fx);
            }
            if c == 0xFF && t.i(PARMRK) {
                self.put(c, injected, false);
            }
            self.newline(c, injected, fx);
            return true;
        }
        false
    }

    /// `n_tty_receive_handle_newline`: the line is finished and readable.
    fn newline(&mut self, c: u8, injected: bool, fx: &mut Effects) {
        let mark = injected || core::mem::take(&mut self.tampered);
        self.put(c, mark, true);
        self.canon = self.buf.len();
        fx.readable = true;
    }

    fn put(&mut self, c: u8, injected: bool, eol: bool) {
        self.buf.push_back(Cell { c, eol, injected });
    }

    fn at_line_start(&self) -> bool {
        self.buf.len() == self.canon
    }

    /// `eraser`: VERASE, VWERASE and VKILL with their echo.
    fn eraser(&mut self, c: u8, injected: bool, fx: &mut Effects) {
        #[derive(PartialEq)]
        enum Kind {
            Erase,
            Werase,
            Kill,
        }
        if self.at_line_start() {
            return;
        }
        let t = self.t;
        let kind = if c == t.c(VERASE) {
            Kind::Erase
        } else if c == t.c(VWERASE) {
            Kind::Werase
        } else {
            if !t.l(ECHO) {
                self.truncate(self.canon, injected);
                return;
            }
            if !t.l(ECHOK) || !t.l(ECHOKE) || !t.l(ECHOE) {
                self.truncate(self.canon, injected);
                self.finish_erasing(fx);
                self.echo_char(t.c(VKILL), fx);
                if t.l(ECHOK) {
                    self.echo_raw(b'\n', fx);
                }
                return;
            }
            Kind::Kill
        };
        let mut seen_alnums = 0;
        while self.buf.len() > self.canon {
            let end = self.buf.len();
            let mut head = end;
            let mut ch;
            // One possibly multi-byte character.
            loop {
                head -= 1;
                ch = self.buf[head].c;
                if !(self.continuation(ch) && head != self.canon) {
                    break;
                }
            }
            if self.continuation(ch) {
                break; // never erase half a character
            }
            if kind == Kind::Werase {
                if isalnum(ch) || ch == b'_' {
                    seen_alnums += 1;
                } else if seen_alnums > 0 {
                    break;
                }
            }
            let cnt = end - head;
            let bytes: Vec<u8> = self.buf.iter().skip(head).map(|x| x.c).collect();
            self.truncate(head, injected);
            if t.l(ECHO) {
                if t.l(ECHOPRT) {
                    if !self.erasing {
                        self.echo_raw(b'\\', fx);
                        self.erasing = true;
                    }
                    self.echo_char(ch, fx);
                    for &b in bytes.iter().skip(1).take(cnt - 1) {
                        self.echo_raw(b, fx);
                        self.column = self.column.saturating_sub(1);
                    }
                } else if kind == Kind::Erase && !t.l(ECHOE) {
                    self.echo_char(t.c(VERASE), fx);
                } else if ch == b'\t' {
                    self.erase_tab(fx);
                } else {
                    if iscntrl(ch) && t.l(ECHOCTL) {
                        self.echo_str(b"\x08 \x08", fx);
                    }
                    if !iscntrl(ch) || t.l(ECHOCTL) {
                        self.echo_str(b"\x08 \x08", fx);
                    }
                }
            }
            if kind == Kind::Erase {
                break;
            }
        }
        if self.at_line_start() && t.l(ECHO) {
            self.finish_erasing(fx);
        }
    }

    /// Drops the cells from `at` on; an injected key erasing typed bytes marks the line.
    fn truncate(&mut self, at: usize, injected: bool) {
        if injected && self.buf.iter().skip(at).any(|x| !x.injected) {
            self.tampered = true;
        }
        self.buf.truncate(at);
    }

    /// `echo_erase_tab` + its echo op: go back to where the tab started.
    fn erase_tab(&mut self, fx: &mut Effects) {
        let mut num_chars: u32 = 0;
        let mut after_tab = false;
        for x in self.buf.iter().skip(self.canon).rev() {
            let ch = x.c;
            if ch == b'\t' {
                after_tab = true;
                break;
            } else if iscntrl(ch) {
                if self.t.l(ECHOCTL) {
                    num_chars += 2;
                }
            } else if !self.continuation(ch) {
                num_chars += 1;
            }
        }
        num_chars &= 7;
        if !after_tab {
            num_chars += self.canon_column;
        }
        let num_bs = 8 - (num_chars & 7);
        for _ in 0..num_bs {
            fx.echo.push(b'\x08');
            self.column = self.column.saturating_sub(1);
        }
    }

    fn finish_erasing(&mut self, fx: &mut Effects) {
        if self.erasing {
            self.echo_raw(b'/', fx);
            self.erasing = false;
        }
    }

    /// `echo_char`: control characters as `^X` with ECHOCTL (not TAB).
    fn echo_char(&mut self, c: u8, fx: &mut Effects) {
        if self.t.l(ECHOCTL) && iscntrl(c) && c != b'\t' {
            fx.echo.push(b'^');
            fx.echo.push(c ^ 0o100);
            self.column += 2;
        } else {
            self.echo_raw(c, fx);
        }
    }

    fn echo_str(&mut self, s: &[u8], fx: &mut Effects) {
        for &b in s {
            self.echo_raw(b, fx);
        }
    }

    /// An echoed byte goes out like program output (OPOST applies).
    fn echo_raw(&mut self, c: u8, fx: &mut Effects) {
        if self.t.o(OPOST) {
            self.output_char(c, &mut fx.echo);
        } else {
            fx.echo.push(c);
        }
    }

    fn start(&mut self, fx: &mut Effects) {
        if self.stopped {
            self.stopped = false;
            fx.started = true;
            fx.echo.append(&mut self.held_echo);
            self.echo_lost = false;
        }
    }

    fn reset_buffer(&mut self) {
        self.buf.clear();
        self.canon = 0;
        self.published = 0;
        self.erasing = false;
        self.push = false;
        self.tampered = false;
    }

    // ---- termios and flow control

    /// `n_tty_set_termios` after a TCSETS (the caller applied `copy_hw`). Returns whether output started again.
    pub fn set_termios(&mut self, new: Termios) -> bool {
        let old = self.t;
        self.t = new;
        if (old.lflag ^ new.lflag) & (ICANON | EXTPROC) != 0 {
            for x in self.buf.iter_mut() {
                x.eol = false;
            }
            if !new.l(ICANON) || self.buf.is_empty() {
                self.canon = 0;
                self.push = false;
            } else {
                if let Some(last) = self.buf.back_mut() {
                    last.eol = true;
                }
                self.canon = self.buf.len();
                self.push = true;
            }
            self.erasing = false;
            self.lnext = false;
        }
        self.icanon = new.l(ICANON);
        self.publish();
        if !new.i(IXON) && old.i(IXON) && !self.tco_stopped && self.stopped {
            self.stopped = false;
            return true;
        }
        false
    }

    /// Echo held while output was stopped, now that it runs again.
    pub fn take_held_echo(&mut self) -> Vec<u8> {
        if self.stopped {
            return Vec::new();
        }
        self.echo_lost = false;
        core::mem::take(&mut self.held_echo)
    }

    pub fn stopped(&self) -> bool {
        self.stopped
    }

    /// TCXONC TCOOFF/TCOON.
    pub fn tco(&mut self, stop: bool) -> bool {
        if stop {
            if !self.tco_stopped {
                self.tco_stopped = true;
                self.stopped = true;
            }
            false
        } else if self.tco_stopped {
            self.tco_stopped = false;
            self.stopped = false;
            true
        } else {
            false
        }
    }

    /// TCFLSH TCIFLUSH (and isig's flush).
    pub fn flush_input(&mut self) {
        self.reset_buffer();
    }

    // ---- reading

    /// `input_available_p`: a read would get something. `poll`: poll's rule (VMIN bytes when VTIME is 0).
    pub fn available(&self, poll: bool) -> bool {
        if self.icanon && !self.t.l(EXTPROC) {
            return self.published.min(self.canon) > 0;
        }
        let amt = if poll && self.t.c(VTIME) == 0 && self.t.c(VMIN) != 0 { self.t.c(VMIN) as usize } else { 1 };
        self.published.min(self.buf.len()) >= amt
    }

    pub fn canonical(&self) -> bool {
        self.icanon && !self.t.l(EXTPROC)
    }

    /// One canonical read step into `out`: the line up to and including its end (the EOF character is not
    /// copied), or as much as fits. Returns the bytes copied.
    pub fn read_canon(&mut self, out: &mut [u8]) -> usize {
        let mut n = 0;
        let mut consumed = 0;
        let mut found = false;
        let limit = self.canon.min(self.published);
        while n < out.len() && consumed < limit {
            let x = self.buf[consumed];
            consumed += 1;
            if x.eol {
                found = true;
                if x.c != DISABLED {
                    out[n] = x.c;
                    n += 1;
                }
                break;
            }
            out[n] = x.c;
            n += 1;
        }
        self.consume(consumed);
        if found {
            self.push = false;
        } else if n == out.len() {
            // The buffer filled exactly in front of an EOF character: eat it (`canon_skip_eof`).
            if let Some(x) = self.buf.front()
                && self.canon.min(self.published) > 0
                && x.eol
                && x.c == DISABLED
            {
                self.consume(1);
            }
        }
        n
    }

    /// One non-canonical read step: everything there, up to `out.len()`. Returns the bytes copied.
    pub fn read_raw(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.buf.len()).min(self.published);
        for (i, o) in out.iter_mut().take(n).enumerate() {
            *o = self.buf[i].c;
        }
        self.consume(n);
        n
    }

    fn consume(&mut self, n: usize) {
        self.buf.drain(..n);
        self.canon = self.canon.saturating_sub(n);
        self.published = self.published.saturating_sub(n);
    }

    /// The next finished line for the kernel's own reader (canonical mode only).
    pub fn take_line(&mut self) -> Option<Line> {
        let limit = self.canon.min(self.published);
        if !self.canonical() || limit == 0 {
            return None;
        }
        let end = self.buf.iter().take(limit).position(|x| x.eol)?;
        let cells: Vec<Cell> = self.buf.drain(..=end).collect();
        self.canon -= end + 1;
        self.published -= end + 1;
        let last = cells[end];
        let injected = cells.iter().any(|x| x.injected);
        let body: Vec<u8> = cells[..end].iter().map(|x| x.c).collect();
        let text = String::from_utf8_lossy(&body).into_owned();
        let eof = last.c == DISABLED && body.is_empty();
        Some(Line { text, injected, eof, newline: last.c == b'\n' })
    }

    /// TIOCINQ: bytes a read could get (canonical: finished lines without their EOF characters).
    pub fn inq(&self) -> usize {
        if self.canonical() {
            self.buf.iter().take(self.canon.min(self.published)).filter(|x| !(x.eol && x.c == DISABLED)).count()
        } else {
            self.buf.len().min(self.published)
        }
    }

    /// The line being typed (not finished), as text.
    pub fn editing(&self) -> String {
        let b: Vec<u8> = self.buf.iter().skip(self.canon).map(|x| x.c).collect();
        String::from_utf8_lossy(&b).into_owned()
    }

    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    // ---- output

    /// Program output through OPOST (`n_tty_write`): the bytes for the screen. Without OPOST unchanged.
    pub fn output(&mut self, data: &[u8]) -> Vec<u8> {
        if !self.t.o(OPOST) {
            return data.to_vec();
        }
        let mut out = Vec::with_capacity(data.len() + data.len() / 8);
        for &c in data {
            self.output_char(c, &mut out);
        }
        out
    }

    /// `do_output_char`.
    fn output_char(&mut self, mut c: u8, out: &mut Vec<u8>) {
        let t = self.t;
        match c {
            b'\n' => {
                if t.o(ONLRET) {
                    self.column = 0;
                }
                if t.o(ONLCR) {
                    self.canon_column = 0;
                    self.column = 0;
                    out.extend_from_slice(b"\r\n");
                    return;
                }
                self.canon_column = self.column;
            }
            b'\r' => {
                if t.o(ONOCR) && self.column == 0 {
                    return;
                }
                if t.o(OCRNL) {
                    c = b'\n';
                    if t.o(ONLRET) {
                        self.canon_column = 0;
                        self.column = 0;
                    }
                } else {
                    self.canon_column = 0;
                    self.column = 0;
                }
            }
            b'\t' => {
                let spaces = 8 - (self.column & 7);
                self.column += spaces;
                if t.oflag & TABDLY == XTABS {
                    out.extend(core::iter::repeat_n(b' ', spaces as usize));
                    return;
                }
            }
            b'\x08' => self.column = self.column.saturating_sub(1),
            _ => {
                if !iscntrl(c) {
                    if t.o(OLCUC) {
                        c = toupper(c);
                    }
                    if !self.continuation(c) {
                        self.column += 1;
                    }
                }
            }
        }
        out.push(c);
    }
}
