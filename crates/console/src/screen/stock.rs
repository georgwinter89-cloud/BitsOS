//! The memory a write may need, made before the VT's lock is taken.
//!
//! The model lives behind `console.vt`, a spin lock, where the heap can neither reclaim nor compact: a line
//! that scrolled made a row there and, while the scrollback filled, doubled the list of rows (49 KiB at 2000
//! rows, 98 KiB at 4000). Now a scroll takes its row from the oldest scrollback row
//! (once the window is full, for good), else from the rows its writer brought; the list of rows and the buffer
//! `write_above` carries the open line in are swapped for larger ones the writer made:
//!
//! ```text
//! let mut stock = Stock::new();          // before the lock: what it replaces is freed after the unlock
//! loop {
//!     let mut vt = lock();
//!     if !vt.screen.prepare(&mut stock, text, false) {
//!         drop(vt);
//!         stock.grow();
//!         continue;
//!     }
//!     vt.screen.write(text);
//!     vt.screen.unprepare(&mut stock);
//!     break;
//! }
//! ```
//!
//! A write without `prepare` (the host tests of the model, a caller that could not make memory) still works:
//! the model makes what it lacks itself and counts it (`took_unprepared`).

use super::{Cell, Esc, Screen};
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

/// What the last `prepare` missed.
#[derive(Clone, Copy, Default)]
struct Want {
    rows: usize,
    cols: usize,
    hist: usize,
    open: usize,
}

pub struct Stock {
    /// Blank rows of `cols` cells, or the rows the screen had left.
    rows: Vec<Vec<Cell>>,
    cols: usize,
    /// A longer list for the screen's rows, or the list it replaced.
    hist: VecDeque<Vec<Cell>>,
    /// A longer buffer for the open line, or the one it replaced.
    open: Vec<Cell>,
    want: Want,
}

impl Default for Stock {
    fn default() -> Self {
        Self::new()
    }
}

impl Stock {
    pub const fn new() -> Stock {
        Stock { rows: Vec::new(), cols: 0, hist: VecDeque::new(), open: Vec::new(), want: Want { rows: 0, cols: 0, hist: 0, open: 0 } }
    }

    /// With no lock held: makes what the last `prepare` missed. As the pushes it stands in for, it cannot fail
    /// (no memory ends the kernel), but here the heap may wait for memory first.
    pub fn grow(&mut self) {
        let w = core::mem::take(&mut self.want);
        if w.rows > 0 {
            // What is stale goes first, so both are never held at once.
            self.rows = Vec::new();
            let mut rows = Vec::with_capacity(w.rows);
            rows.resize_with(w.rows, || vec![Cell::BLANK; w.cols]);
            (self.rows, self.cols) = (rows, w.cols);
        }
        if w.hist > 0 {
            self.hist = VecDeque::new();
            self.hist = VecDeque::with_capacity(w.hist);
        }
        if w.open > 0 {
            self.open = Vec::new();
            self.open = Vec::with_capacity(w.open);
        }
    }
}

impl Screen {
    /// The open line's rows in `hist` (first, last) and the cells of the last one that belong to it: the last
    /// row up to its text, on the cursor's row at least up to the cursor (`\r` progress lines).
    pub(super) fn open_span(&self) -> (usize, usize, usize) {
        let b = self.base();
        let cur = b + self.row as usize;
        let first = cur.saturating_sub(self.line_rows as usize).max(b);
        let last = (cur + self.line_below as usize).min(self.hist.len() - 1);
        let mut end = self.hist[last].iter().rposition(|c| *c != Cell::BLANK).map(|i| i + 1).unwrap_or(0);
        if last == cur {
            end = end.max(self.col.min(self.cols) as usize);
        }
        (first, last, end)
    }

    /// At most this many rows are added to the scrollback by `write(text)` or (`above`) `write_above(text, _)`
    /// as the screen is now: every line feed, wrap and escape sequence that can feed a line, less the rows the
    /// cursor has below it (unless an escape sequence may move it).
    fn scroll_bound(&self, text: &str, above: bool) -> usize {
        let cols = self.cols as usize;
        let (mut feeds, mut cells, mut escapes) = (0usize, 0usize, 0usize);
        for c in text.chars() {
            match c {
                '\n' | '\u{b}' | '\u{c}' => feeds += 1,
                '\u{1b}' => escapes += 1,
                '\t' => cells += 8,
                c if c >= ' ' => cells += 1,
                _ => {}
            }
        }
        // A sequence left half-written by an earlier write ends in this one (`write_above` sets it aside).
        if !above && self.esc != Esc::None {
            escapes += 1;
        }
        let whole = self.top == 0 && self.bot == self.rows && self.alt.is_none();
        if escapes == 0 && !whole {
            // Only the whole main screen scrolls into the scrollback, and nothing here changes what scrolls.
            return 0;
        }
        let b = self.base();
        let (start_row, start_col, after) = if above {
            // The line from the open line's first row, a new line, the open line placed again.
            let (first, last, end) = self.open_span();
            (first - b, 0, 1 + ((last - first) * cols + end) / cols)
        } else {
            (self.row as usize, self.col.min(self.cols) as usize, 0)
        };
        // An escape sequence may feed a line itself (IND, NEL) and may put the cursor where the next cell wraps.
        let lines = feeds + (start_col + cells) / cols + 2 * escapes + after;
        let below = if escapes == 0 && start_row < self.bot as usize { self.bot as usize - 1 - start_row } else { 0 };
        lines.saturating_sub(below)
    }

    /// Under the VT's lock, before `write(text)` or (`above`) `write_above(text, _)`: whether the screen has the
    /// memory the write can need, taking it out of `stock` when it is there. False: drop the lock, `stock.grow()`,
    /// lock and ask again (the screen may have changed meanwhile: everything is worked out anew).
    pub fn prepare(&mut self, stock: &mut Stock, text: &str, above: bool) -> bool {
        let cols = self.cols as usize;
        // Scrolls beyond the window's free rows reuse the oldest row.
        let free = (self.rows as usize + self.limit).saturating_sub(self.hist.len());
        let new = self.scroll_bound(text, above).min(free);
        let mut ok = true;
        if self.spare.len() < new {
            if stock.rows.len() >= new && stock.cols >= cols {
                core::mem::swap(&mut self.spare, &mut stock.rows);
            } else {
                (stock.want.rows, stock.want.cols) = (new, cols);
                ok = false;
            }
        }
        let len = self.hist.len();
        if self.hist.capacity() - len < new {
            if stock.hist.capacity() >= len + new {
                stock.hist.extend(self.hist.drain(..));
                core::mem::swap(&mut self.hist, &mut stock.hist);
            } else {
                // Doubling, but never past the window: the list is not asked for again at every line.
                stock.want.hist = (len + new).max((len * 2).min(len + free));
                ok = false;
            }
        }
        if above {
            let (first, last, _) = self.open_span();
            let open = (last - first + 1) * cols;
            if self.open_buf.capacity() < open {
                if stock.open.capacity() >= open {
                    core::mem::swap(&mut self.open_buf, &mut stock.open);
                } else {
                    stock.want.open = open;
                    ok = false;
                }
            }
        }
        ok
    }

    /// Under the lock, after the write: the rows it did not use go back to `stock` (freed by its owner after
    /// the unlock), so none pile up here.
    pub fn unprepare(&mut self, stock: &mut Stock) {
        if stock.rows.is_empty() {
            // What the stock holds is the screen's old, empty list (`prepare` swapped): nothing is allocated.
            core::mem::swap(&mut self.spare, &mut stock.rows);
        } else {
            self.spare.clear();
        }
    }

    /// A row for the bottom of the scrollback's screen: the oldest row when the window is full, else one the
    /// writer brought, else (nothing prepared) a new one.
    pub(super) fn scrollback_row(&mut self) -> Vec<Cell> {
        if self.hist.len() >= self.rows as usize + self.limit
            && let Some(r) = self.hist.pop_front()
        {
            return r;
        }
        if let Some(r) = self.spare.pop() {
            return r;
        }
        self.unprepared += 1;
        Vec::new()
    }

    /// Times the model had to make memory itself since this was last asked (0 for writes that were prepared).
    pub fn took_unprepared(&mut self) -> u32 {
        core::mem::take(&mut self.unprepared)
    }
}

#[cfg(test)]
mod tests;
