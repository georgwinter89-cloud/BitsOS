//! The VT's text model (no drawing, no locks: host-tested): rows of cells with a colour attribute, the cursor,
//! scrollback with a view offset (Shift+PageUp/PageDown), and what changed since the last render.
//!
//! Output handling: `\n` starts a new line at column 0 (the kernel console has always done CR+LF on `\n`), `\r`,
//! `\t` (to the next multiple of 8), `\x08` (cursor left), lines wrap at the right edge. Escape sequences follow
//! Linux's VT (drivers/tty/vt/vt.c, TERM=linux): cursor movement, erasing, inserting and deleting characters and
//! lines, the scroll region, saved cursor, modes, the alternate screen, reports (`csi`, `edit`). Anything else is
//! parsed to its end and dropped, so programs never print escape garbage.
//!
//! Scrollback is a window of rows kept for looking back (`limit`), a memory bound like klog's ring, not a count
//! of things (R1): `console_scrollback=` / the monitor's `scrollback` command change it.

mod csi;
mod edit;

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use csi::Csi;

/// 16-colour palette (0x00RRGGBB). 0 is the console background, 7 its text colour.
pub const PALETTE: [u32; 16] = [
    0x0010_1418, 0x00FF_5050, 0x0050_C878, 0x00FF_C040, 0x0050_80FF, 0x00C8_64C8, 0x0050_C8C8, 0x00C8_C8C8,
    0x0068_6C70, 0x00FF_8080, 0x0080_FFA0, 0x00FF_E080, 0x0080_A8FF, 0x00F0_90F0, 0x0080_F0F0, 0x00FF_FFFF,
];
pub const FG_INDEX: u8 = 7;
pub const BG_INDEX: u8 = 0;
/// Attribute byte: low nibble foreground index, high nibble background index.
pub const DEFAULT_ATTR: u8 = FG_INDEX;

pub fn attr(fg: u8, bg: u8) -> u8 {
    (fg & 15) | (bg & 15) << 4
}
pub fn fg_of(a: u8) -> u32 {
    PALETTE[(a & 15) as usize]
}
pub fn bg_of(a: u8) -> u32 {
    PALETTE[(a >> 4) as usize]
}
/// The palette index for a 0x00RRGGBB colour the kernel asks for (exact entries; anything else is the text colour).
pub fn index_of(color: u32) -> u8 {
    PALETTE.iter().position(|&p| p == color).map(|i| i as u8).unwrap_or(FG_INDEX)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub attr: u8,
}

impl Cell {
    pub const BLANK: Cell = Cell { ch: ' ', attr: DEFAULT_ATTR };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Esc {
    None,
    Start,
    Csi(Csi),
    /// `ESC (`, `ESC )`, `ESC #` ...: one more character (a charset name) that is ignored.
    Charset,
    /// `ESC ]`: `P` starts a palette entry, a digit an OSC string, anything else ends it (Linux's ESnonstd).
    OscStart,
    /// An OSC string, up to BEL or ESC.
    Osc,
    /// Hex digits of `ESC ] P` still to swallow.
    Palette(u8),
    /// A sequence too long to keep: swallowed up to its final byte.
    Skip,
}

/// What `ESC 7` / `CSI s` keep (Linux's `save_cur`), with the open line's rows around the cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Saved {
    col: u32,
    row: u32,
    attr: u8,
    reverse: bool,
    line_rows: u32,
    line_below: u32,
}

const SAVED0: Saved = Saved { col: 0, row: 0, attr: DEFAULT_ATTR, reverse: false, line_rows: 0, line_below: 0 };

/// Default scrollback window in rows.
pub const DEFAULT_SCROLLBACK: usize = 4000;

/// Bytes of reports (DSR, DA) kept for the terminal's input until `take_reply`.
const REPLY_MAX: usize = 256;

pub struct Screen {
    pub cols: u32,
    pub rows: u32,
    /// Scrollback rows followed by the `rows` screen rows (always at least `rows` entries).
    hist: VecDeque<Vec<Cell>>,
    /// Column `cols` means the last column was written and the next character wraps (Linux's `need_wrap`).
    pub col: u32,
    pub row: u32,
    pub attr: u8,
    /// Rows looked back (0 = the live screen).
    pub view: u32,
    pub limit: usize,
    esc: Esc,
    /// Screen rows changed since the last render.
    dirty: Vec<bool>,
    /// Whole screen to redraw (view moved, cleared, resized).
    pub full: bool,
    /// Scroll-ups of the live screen since the last render (the renderer moves pixels instead of redrawing).
    pub scrolled: u32,
    /// Rows above the cursor's row that belong to the line being written (it wrapped at the right edge), so
    /// `write_above` can carry the whole open line, e.g. a long half-typed command.
    line_rows: u32,
    /// Rows below the cursor's row that belong to the open line (a line editor moved the cursor up into it).
    line_below: u32,
    /// A `\r` ended a pending wrap: the line feed after it continues the open line (BusyBox's line editing
    /// writes `\r\n` at the right edge instead of relying on the wrap).
    wrap_cr: bool,
    /// Scroll region rows `top..bot` (DECSTBM).
    top: u32,
    bot: u32,
    /// SGR 7: cells get the colours swapped.
    reverse: bool,
    /// DECAWM, IRM, DECOM and DECTCEM.
    autowrap: bool,
    insert: bool,
    origin: bool,
    cursor_on: bool,
    saved: Saved,
    /// The main screen's rows while the alternate screen is shown.
    alt: Option<Vec<Vec<Cell>>>,
    /// Reports for the terminal's input (Linux's `respond_string`).
    reply: Vec<u8>,
}

impl Screen {
    pub fn new(cols: u32, rows: u32) -> Screen {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let mut hist = VecDeque::new();
        for _ in 0..rows {
            hist.push_back(vec![Cell::BLANK; cols as usize]);
        }
        Screen {
            cols,
            rows,
            hist,
            col: 0,
            row: 0,
            attr: DEFAULT_ATTR,
            view: 0,
            limit: DEFAULT_SCROLLBACK,
            esc: Esc::None,
            dirty: vec![true; rows as usize],
            full: true,
            scrolled: 0,
            line_rows: 0,
            line_below: 0,
            wrap_cr: false,
            top: 0,
            bot: rows,
            reverse: false,
            autowrap: true,
            insert: false,
            origin: false,
            cursor_on: true,
            saved: SAVED0,
            alt: None,
            reply: Vec::new(),
        }
    }

    /// Keeps the text when the font changes: rows are cut or padded to the new width, the newest rows stay.
    pub fn resize(&mut self, cols: u32, rows: u32) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        for r in self.hist.iter_mut() {
            r.resize(cols as usize, Cell::BLANK);
        }
        // The rows below the cursor are dropped, so the cursor's row stays the last used one.
        let abs = self.base() + self.row as usize;
        self.hist.truncate(abs + 1);
        while self.hist.len() < rows as usize {
            self.hist.push_back(vec![Cell::BLANK; cols as usize]);
        }
        self.row = (abs - (self.hist.len() - rows as usize)) as u32;
        self.cols = cols;
        self.rows = rows;
        self.col = self.col.min(cols - 1);
        self.view = 0;
        self.dirty = vec![true; rows as usize];
        self.full = true;
        self.scrolled = 0;
        self.line_rows = 0;
        self.line_below = 0;
        self.wrap_cr = false;
        (self.top, self.bot) = (0, rows);
        self.trim();
    }

    fn base(&self) -> usize {
        self.hist.len() - self.rows as usize
    }
    fn cell_mut(&mut self, col: u32, row: u32) -> &mut Cell {
        let b = self.base();
        &mut self.hist[b + row as usize][col as usize]
    }
    fn touch(&mut self, row: u32) {
        if let Some(d) = self.dirty.get_mut(row as usize) {
            *d = true;
        }
    }

    /// Rows kept above the screen.
    pub fn scrollback_len(&self) -> u32 {
        self.base() as u32
    }

    /// The row shown at screen row `r` (with the view offset).
    pub fn visible_row(&self, r: u32) -> &[Cell] {
        let i = self.base() - self.view as usize + r as usize;
        &self.hist[i]
    }

    /// Moves the view by `delta` rows (positive = back in time), clamped. True when it moved.
    pub fn scroll_view(&mut self, delta: i64) -> bool {
        let max = self.base() as i64;
        let v = (self.view as i64 + delta).clamp(0, max) as u32;
        if v == self.view {
            return false;
        }
        self.view = v;
        self.full = true;
        true
    }
    pub fn view_to_bottom(&mut self) -> bool {
        self.scroll_view(-(self.view as i64))
    }

    fn trim(&mut self) {
        while self.hist.len() > self.rows as usize + self.limit {
            self.hist.pop_front();
        }
        self.view = self.view.min(self.base() as u32);
    }
    pub fn set_limit(&mut self, rows: usize) {
        self.limit = rows;
        self.trim();
        self.full = true;
    }

    fn newline(&mut self) {
        self.col = 0;
        self.lf(false);
    }

    /// Linux's `lf`: down a row, scrolling the region at its bottom. `cont`: the row below continues the open line.
    fn lf(&mut self, cont: bool) {
        if cont {
            self.line_rows = self.line_rows.saturating_add(1);
            self.line_below = self.line_below.saturating_sub(1);
        } else {
            self.line_rows = 0;
            self.line_below = 0;
        }
        if self.row + 1 == self.bot {
            self.scroll_up(self.top, self.bot, 1, true);
        } else if self.row + 1 < self.rows {
            self.row += 1;
        }
    }

    /// One character at the cursor with the modes (wrap, insert).
    fn put(&mut self, c: char) {
        if self.col >= self.cols && !self.autowrap {
            self.col = self.cols - 1;
        }
        if self.insert && self.col < self.cols {
            self.insert_chars(1);
        }
        let a = self.cell_attr();
        self.place(Cell { ch: c, attr: a });
    }

    fn place(&mut self, cell: Cell) {
        if self.col >= self.cols {
            // A wrap, not a new line: the row joins the open line.
            self.col = 0;
            self.lf(true);
        }
        let (col, row) = (self.col, self.row);
        *self.cell_mut(col, row) = cell;
        self.touch(row);
        self.col += 1;
    }

    pub fn clear(&mut self) {
        let b = self.base();
        for r in 0..self.rows as usize {
            self.hist[b + r].fill(Cell::BLANK);
        }
        self.col = 0;
        self.row = 0;
        self.view = 0;
        self.line_rows = 0;
        self.line_below = 0;
        self.full = true;
    }

    pub fn write(&mut self, s: &str) {
        for c in s.chars() {
            self.write_char(c);
        }
    }

    pub fn write_char(&mut self, c: char) {
        let wrap_cr = core::mem::take(&mut self.wrap_cr);
        match core::mem::replace(&mut self.esc, Esc::None) {
            Esc::None => {}
            Esc::Start => return self.esc_char(c),
            Esc::Csi(q) => return self.csi_char(q, c),
            Esc::Charset => return,
            Esc::OscStart => {
                self.esc = match c {
                    'P' => Esc::Palette(7),
                    '0'..='9' => Esc::Osc,
                    _ => Esc::None,
                };
                return;
            }
            Esc::Osc => {
                match c {
                    '\u{7}' => {}
                    '\u{1b}' => self.esc = Esc::Start,
                    _ => self.esc = Esc::Osc,
                }
                return;
            }
            Esc::Palette(n) => {
                if n > 1 {
                    self.esc = Esc::Palette(n - 1);
                }
                return;
            }
            Esc::Skip => {
                if !('@'..='~').contains(&c) {
                    self.esc = Esc::Skip;
                }
                return;
            }
        }
        match c {
            '\n' => self.newline(),
            // Line feed only (Linux's VT: LF, VT and FF move down and keep the column; `write_program` sends a
            // program's newline as VT, so ONLCR decides about the carriage return).
            '\u{b}' | '\u{c}' => self.lf(wrap_cr),
            '\r' => {
                self.wrap_cr = wrap_cr || self.col >= self.cols;
                self.col = 0;
            }
            '\t' => self.col = (((self.col / 8) + 1) * 8).min(self.cols),
            // From a pending wrap this lands on the last column (unlike Linux), which the monitor's `\x08 \x08`
            // erase at the right edge relies on.
            '\u{8}' => self.col = self.col.min(self.cols).saturating_sub(1),
            '\u{1b}' => self.esc = Esc::Start,
            '\u{7}' => {}
            c if (c as u32) < 0x20 || c == '\u{7f}' => {}
            c => self.put(c),
        }
    }

    /// Writes `line` with colour `attr` as a line of its own **above the open line** (what was written since the
    /// last newline: the monitor's prompt and the half-typed command, a program's prompt, also when it wrapped over
    /// several rows or a line editor moved the cursor up into it), then draws the open line again below it with
    /// its own colours and puts the cursor back where it was inside it. This is how log lines arrive while Georg
    /// types: the typed text is never torn apart, and since the echo of every key goes through the same model under
    /// the same lock, no key can slip in between.
    /// An escape sequence a program left half-written stays pending for its next write.
    pub fn write_above(&mut self, line: &str, attr: u8) {
        let esc = core::mem::replace(&mut self.esc, Esc::None);
        let (keep, reverse, insert) = (self.attr, self.reverse, self.insert);
        (self.reverse, self.insert, self.wrap_cr) = (false, false, false);
        let b = self.base();
        let cur = b + self.row as usize;
        let first = cur.saturating_sub(self.line_rows as usize).max(b);
        let last = (cur + self.line_below as usize).min(self.hist.len() - 1);
        let cols = self.cols as usize;
        let col = self.col.min(self.cols) as usize;
        // The last row up to its text, on the cursor's row at least up to the cursor (`\r` progress lines).
        let mut end = self.hist[last].iter().rposition(|c| *c != Cell::BLANK).map(|i| i + 1).unwrap_or(0);
        if last == cur {
            end = end.max(col);
        }
        let mut open: Vec<Cell> = Vec::new();
        for row in self.hist.range(first..last) {
            open.extend_from_slice(row);
        }
        open.extend_from_slice(&self.hist[last][..end]);
        let at = (cur - first) * cols + col;
        for r in first..=last {
            self.hist[r].fill(Cell::BLANK);
            self.touch((r - b) as u32);
        }
        self.row = (first - b) as u32;
        self.col = 0;
        self.line_rows = 0;
        self.line_below = 0;
        self.attr = attr;
        for c in line.chars() {
            self.write_char(c);
        }
        // A control character in `line` could have started a sequence: it ends with the line.
        self.esc = Esc::None;
        self.newline();
        for c in &open {
            self.place(*c);
        }
        (self.attr, self.reverse, self.insert) = (keep, reverse, insert);
        // Back to the cursor's place inside the open line (cells right of it and rows below were carried too).
        let back = open.len().saturating_sub(at);
        if back > 0 {
            let t = (self.row as usize * cols + self.col as usize).saturating_sub(back);
            // A pending wrap stays one: the end of its row rather than the start of the next.
            let (row, c) = if col == cols && t > 0 { ((t - 1) / cols, (t - 1) % cols + 1) } else { (t / cols, t % cols) };
            let up = self.row - row as u32;
            self.line_rows = self.line_rows.saturating_sub(up);
            self.line_below = up;
            self.row = row as u32;
            self.col = c as u32;
        } else if col < cols && self.col >= self.cols {
            // The cursor was at the start of a row the open line had just reached (`\r\n` at the right edge).
            self.col = 0;
            self.lf(true);
        }
        self.esc = esc;
    }

    /// The text of the open line (from its first wrapped row up to the cursor), for tests.
    pub fn open_line(&self) -> String {
        let b = self.base();
        let cur = b + self.row as usize;
        let first = cur.saturating_sub(self.line_rows as usize).max(b);
        let mut s = String::new();
        for row in self.hist.range(first..cur) {
            s.extend(row.iter().map(|c| c.ch));
        }
        s.extend(self.hist[cur][..self.col.min(self.cols) as usize].iter().map(|c| c.ch));
        s
    }

    /// Takes the changed screen rows (and resets the record).
    pub fn take_dirty(&mut self) -> (bool, u32, Vec<u32>) {
        let full = core::mem::replace(&mut self.full, false);
        let scrolled = core::mem::replace(&mut self.scrolled, 0);
        let rows = self.dirty.iter().enumerate().filter(|(_, d)| **d).map(|(i, _)| i as u32).collect();
        self.dirty.fill(false);
        (full, scrolled, rows)
    }

    /// Takes the reports (cursor position, status, identity) a program asked for, for the terminal's input.
    pub fn take_reply(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.reply)
    }

    /// The screen as text (the live screen, not the view), trailing blanks trimmed, empty rows at the end dropped.
    pub fn text_lines(&self) -> Vec<String> {
        let b = self.base();
        let mut v: Vec<String> = (0..self.rows as usize)
            .map(|r| {
                let s: String = self.hist[b + r].iter().map(|c| c.ch).collect();
                String::from(s.trim_end())
            })
            .collect();
        while v.last().is_some_and(String::is_empty) {
            v.pop();
        }
        v
    }

    /// The cursor cell on the shown picture (None while looking back or while a program hid it).
    pub fn cursor(&self) -> Option<(u32, u32)> {
        (self.view == 0 && self.cursor_on).then_some((self.col.min(self.cols - 1), self.row))
    }
}
