//! What the escape sequences do to the cells and the cursor, after Linux's VT (drivers/tty/vt/vt.c: `gotoxy`,
//! `csi_J`, `csi_K`, `csi_X`, `csi_P`, `csi_at`, `csi_L`, `csi_M`, `con_scroll`, `save_cur`, `reset_terminal`).
//! Every change marks its rows dirty; the open line (`line_rows`/`line_below`) follows the cursor while it stays
//! inside it and is forgotten when the cursor leaves or the rows move.

use super::{Cell, DEFAULT_ATTR, Esc, FG_INDEX, SAVED0, Saved, Screen};
use alloc::vec;
use alloc::vec::Vec;

impl Screen {
    /// The cursor's column on the grid (a pending wrap sits on the last column, as Linux's `need_wrap`).
    pub(super) fn x(&self) -> u32 {
        self.col.min(self.cols - 1)
    }

    /// The attribute new cells get (SGR 7 swaps the colours).
    pub(super) fn cell_attr(&self) -> u8 {
        if self.reverse { self.attr >> 4 | self.attr << 4 } else { self.attr }
    }

    /// Erased cells keep the background colour (Linux's `video_erase_char`).
    fn erase_cell(&self) -> Cell {
        Cell { ch: ' ', attr: self.cell_attr() & 0xF0 | FG_INDEX }
    }

    fn touch_rows(&mut self, from: u32, to: u32) {
        for r in from..to {
            self.touch(r);
        }
    }

    fn forget_line(&mut self) {
        self.line_rows = 0;
        self.line_below = 0;
    }

    /// Linux's `gotoxy`: clamped to the screen, or to the scroll region in origin mode.
    pub(super) fn goto(&mut self, x: u32, y: u32) {
        let (min, max) = if self.origin { (self.top, self.bot) } else { (0, self.rows) };
        self.col = x.min(self.cols - 1);
        self.set_row(y.clamp(min, max - 1));
    }

    /// Linux's `gotoxay`: `y` counts from the region's top in origin mode.
    pub(super) fn goto_origin(&mut self, x: u32, y: u32) {
        self.goto(x, if self.origin { y.saturating_add(self.top) } else { y });
    }

    fn set_row(&mut self, r: u32) {
        if r < self.row {
            let up = self.row - r;
            if up <= self.line_rows {
                self.line_rows -= up;
                self.line_below += up;
            } else {
                self.forget_line();
            }
        } else if r > self.row {
            let down = r - self.row;
            if down <= self.line_below {
                self.line_below -= down;
                self.line_rows += down;
            } else {
                self.forget_line();
            }
        }
        self.row = r;
    }

    fn fill_row(&mut self, row: u32, from: u32, to: u32) {
        let (e, b) = (self.erase_cell(), self.base());
        let to = to.min(self.cols);
        if from < to {
            self.hist[b + row as usize][from as usize..to as usize].fill(e);
        }
        self.touch(row);
    }

    /// EL: 0 from the cursor to the end (nothing on a pending wrap), 1 from the start through the cursor, 2 all.
    pub(super) fn erase_line(&mut self, mode: u32) {
        let (row, cols) = (self.row, self.cols);
        match mode {
            0 => self.fill_row(row, self.col, cols),
            1 => self.fill_row(row, 0, self.x() + 1),
            2 => self.fill_row(row, 0, cols),
            _ => {}
        }
    }

    /// ED: 0 from the cursor to the end, 1 from the start through the cursor, 2 all, 3 all and the scrollback.
    /// The cursor stays (Linux).
    pub(super) fn erase_display(&mut self, mode: u32) {
        let (row, rows, cols) = (self.row, self.rows, self.cols);
        match mode {
            0 => {
                self.erase_line(0);
                for r in row + 1..rows {
                    self.fill_row(r, 0, cols);
                }
                self.line_below = 0;
            }
            1 => {
                for r in 0..row {
                    self.fill_row(r, 0, cols);
                }
                self.erase_line(1);
                self.forget_line();
            }
            2 | 3 => {
                if mode == 3 {
                    while self.hist.len() > rows as usize {
                        self.hist.pop_front();
                    }
                    self.view = 0;
                }
                for r in 0..rows {
                    self.fill_row(r, 0, cols);
                }
                self.full = true;
                self.forget_line();
            }
            _ => {}
        }
    }

    /// ECH: `n` cells from the cursor become blanks.
    pub(super) fn erase_chars(&mut self, n: u32) {
        let (x, row) = (self.x(), self.row);
        self.fill_row(row, x, x.saturating_add(n));
        self.col = x;
    }

    /// DCH: `n` cells at the cursor go, the rest of the row moves left.
    pub(super) fn delete_chars(&mut self, n: u32) {
        let (x, row, b, e) = (self.x() as usize, self.row, self.base(), self.erase_cell());
        let r = &mut self.hist[b + row as usize];
        let n = (n as usize).min(r.len() - x);
        r[x..].rotate_left(n);
        let len = r.len();
        r[len - n..].fill(e);
        self.col = x as u32;
        self.touch(row);
    }

    /// ICH: `n` blanks at the cursor, the rest of the row moves right (and off the edge).
    pub(super) fn insert_chars(&mut self, n: u32) {
        let (x, row, b, e) = (self.x() as usize, self.row, self.base(), self.erase_cell());
        let r = &mut self.hist[b + row as usize];
        let n = (n as usize).min(r.len() - x);
        r[x..].rotate_right(n);
        r[x..x + n].fill(e);
        self.col = x as u32;
        self.touch(row);
    }

    /// IL: `n` blank rows at the cursor's row, the rows below move down inside the scroll region.
    pub(super) fn insert_lines(&mut self, n: u32) {
        let (row, bot) = (self.row, self.bot);
        self.scroll_down(row, bot, n);
        self.col = self.x();
        self.forget_line();
    }

    /// DL: `n` rows at the cursor's row go, the rows below move up inside the scroll region.
    pub(super) fn delete_lines(&mut self, n: u32) {
        let (row, bot) = (self.row, self.bot);
        self.scroll_up(row, bot, n, false);
        self.col = self.x();
        self.forget_line();
    }

    /// SU / SD: the scroll region moves up or down by `n` rows (never into the scrollback).
    pub(super) fn scroll_lines(&mut self, n: u32, up: bool) {
        let (top, bot) = (self.top, self.bot);
        if up {
            self.scroll_up(top, bot, n, false);
        } else {
            self.scroll_down(top, bot, n);
        }
        self.forget_line();
    }

    /// Rows `top..bot` move up by `n`. `keep`: the whole main screen scrolling puts the top rows into the
    /// scrollback (a line feed at the bottom); the alternate screen and the editing sequences never do.
    pub(super) fn scroll_up(&mut self, top: u32, bot: u32, n: u32, keep: bool) {
        let bot = bot.min(self.rows);
        if top >= bot {
            return;
        }
        let n = n.min(bot - top);
        let blank = vec![self.erase_cell(); self.cols as usize];
        let whole = top == 0 && bot == self.rows;
        if whole && keep && self.alt.is_none() {
            for _ in 0..n {
                self.hist.push_back(blank.clone());
                self.trim();
                if self.view > 0 {
                    // Looking back: the picture stays where it is (Linux keeps the view too).
                    self.view = (self.view + 1).min(self.base() as u32);
                } else {
                    self.note_scroll(1);
                }
            }
            return;
        }
        let b = self.base();
        for _ in 0..n {
            self.hist.remove(b + top as usize);
            self.hist.insert(b + bot as usize - 1, blank.clone());
        }
        if whole && self.view == 0 {
            self.note_scroll(n);
        } else {
            self.touch_rows(top, bot);
        }
    }

    /// Rows `top..bot` move down by `n`.
    fn scroll_down(&mut self, top: u32, bot: u32, n: u32) {
        let bot = bot.min(self.rows);
        if top >= bot {
            return;
        }
        let n = n.min(bot - top);
        let blank = vec![self.erase_cell(); self.cols as usize];
        let b = self.base();
        for _ in 0..n {
            self.hist.remove(b + bot as usize - 1);
            self.hist.insert(b + top as usize, blank.clone());
        }
        self.touch_rows(top, bot);
    }

    /// The whole live screen moved up by `n`: the renderer moves pixels and draws the new rows.
    fn note_scroll(&mut self, n: u32) {
        if n >= self.rows {
            self.full = true;
            return;
        }
        self.scrolled += n;
        self.dirty.rotate_left(n as usize);
        for d in self.dirty.iter_mut().rev().take(n as usize) {
            *d = true;
        }
    }

    /// DECSTBM: rows `t..=b` (1-based) scroll, at least two of them; the cursor goes home.
    pub(super) fn set_region(&mut self, t: u32, b: u32) {
        if t < b && b <= self.rows {
            (self.top, self.bot) = (t - 1, b);
            self.goto_origin(0, 0);
        }
    }

    /// Linux's `ri`: up a row, scrolling the region down at its top.
    pub(super) fn reverse_index(&mut self) {
        if self.row == self.top {
            let (top, bot) = (self.top, self.bot);
            self.scroll_down(top, bot, 1);
            self.forget_line();
        } else if self.row > 0 {
            self.set_row(self.row - 1);
        }
    }

    pub(super) fn save_cursor(&mut self) {
        self.saved = Saved {
            col: self.col,
            row: self.row,
            attr: self.attr,
            reverse: self.reverse,
            line_rows: self.line_rows,
            line_below: self.line_below,
        };
    }

    pub(super) fn restore_cursor(&mut self) {
        let s = self.saved;
        (self.attr, self.reverse) = (s.attr, s.reverse);
        self.col = s.col.min(self.cols - 1);
        self.row = s.row.min(self.rows - 1);
        self.line_rows = s.line_rows.min(self.row);
        self.line_below = s.line_below.min(self.rows - 1 - self.row);
    }

    /// `ESC c` (Linux's `reset_terminal`): modes and colours to their defaults, the screen cleared, the cursor
    /// home; the scrollback stays.
    pub(super) fn reset(&mut self) {
        self.alt = None;
        (self.top, self.bot) = (0, self.rows);
        (self.attr, self.reverse) = (DEFAULT_ATTR, false);
        (self.autowrap, self.insert, self.origin, self.cursor_on) = (true, false, false, true);
        self.erase_display(2);
        (self.col, self.row) = (0, 0);
        self.saved = SAVED0;
        self.esc = Esc::None;
    }

    /// ?1049 / ?1047 / ?47: the alternate screen (a full-screen program's), without scrollback; leaving it brings
    /// back the main screen as it was. `cursor`: the cursor is saved on entering and restored on leaving (?1049).
    pub(super) fn alt_screen(&mut self, on: bool, cursor: bool) {
        if on == self.alt.is_some() {
            return;
        }
        let b = self.base();
        if on {
            if cursor {
                self.save_cursor();
            }
            self.alt = Some(self.hist.range(b..).cloned().collect());
            for r in b..self.hist.len() {
                self.hist[r].fill(Cell::BLANK);
            }
        } else {
            let mut main: Vec<Vec<Cell>> = self.alt.take().unwrap_or_default();
            main.resize(self.rows as usize, Vec::new());
            for (i, mut r) in main.into_iter().take(self.rows as usize).enumerate() {
                r.resize(self.cols as usize, Cell::BLANK);
                self.hist[b + i] = r;
            }
        }
        self.forget_line();
        self.view = 0;
        self.full = true;
        if !on && cursor {
            self.restore_cursor();
        }
    }
}
