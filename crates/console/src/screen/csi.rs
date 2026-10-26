//! Escape and CSI sequences as Linux's VT reads them (`do_con_trol`, drivers/tty/vt/vt.c): parsing, dispatch,
//! SGR, modes and reports. The editing they do is in `edit`.

use super::{DEFAULT_ATTR, Esc, FG_INDEX, REPLY_MAX, Screen, attr};
use alloc::format;

/// A bound on hostile input: a sequence with more parameters than any VT knows is dropped.
const MAX_PARAMS: usize = 16;

/// A CSI sequence being read: its parameters, the private marker (`?`, `>`, ...) and whether an intermediate
/// byte (`SP`, `!`, `$`, ...) came, which makes it one Linux does not know.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(super) struct Csi {
    p: [u32; MAX_PARAMS],
    n: usize,
    in_num: bool,
    mark: u8,
    inter: bool,
}

impl Screen {
    pub(super) fn esc_char(&mut self, c: char) {
        match c {
            '[' => self.esc = Esc::Csi(Csi::default()),
            ']' => self.esc = Esc::OscStart,
            '(' | ')' | '*' | '+' | '#' | '%' => self.esc = Esc::Charset,
            '7' => self.save_cursor(),
            '8' => self.restore_cursor(),
            'c' => self.reset(),
            'D' => self.lf(false),
            'E' => self.newline(),
            'M' => self.reverse_index(),
            'Z' => self.respond("\x1b[?6c"),
            _ => {}
        }
    }

    pub(super) fn csi_char(&mut self, mut q: Csi, c: char) {
        match c {
            '0'..='9' => {
                if !q.in_num {
                    if q.n == MAX_PARAMS {
                        self.esc = Esc::Skip;
                        return;
                    }
                    q.p[q.n] = 0;
                    q.n += 1;
                    q.in_num = true;
                }
                let p = &mut q.p[q.n - 1];
                *p = p.saturating_mul(10).saturating_add(c as u32 - '0' as u32);
            }
            // `:` separates sub-parameters (38:5:n); read like `;` so the rest is never printed.
            ';' | ':' => {
                if !q.in_num {
                    if q.n == MAX_PARAMS {
                        self.esc = Esc::Skip;
                        return;
                    }
                    q.p[q.n] = 0;
                    q.n += 1;
                }
                q.in_num = false;
            }
            '<'..='?' => q.mark = c as u8,
            ' '..='/' => q.inter = true,
            '@'..='~' => return self.csi(c, &q),
            // ESC inside a sequence starts a new one (Linux); anything else ends it unseen.
            '\u{1b}' => {
                self.esc = Esc::Start;
                return;
            }
            _ => return,
        }
        self.esc = Esc::Csi(q);
    }

    fn csi(&mut self, f: char, q: &Csi) {
        let p = &q.p[..q.n];
        let raw = |i: usize| p.get(i).copied().unwrap_or(0);
        let n = |i: usize| raw(i).max(1);
        if q.inter {
            return;
        }
        if q.mark == b'?' {
            if f == 'h' || f == 'l' {
                for &v in p {
                    self.dec_mode(v, f == 'h');
                }
            }
            return;
        }
        if q.mark != 0 {
            return;
        }
        let (x, y) = (self.x(), self.row);
        match f {
            'm' => self.sgr(p),
            'A' => self.goto(x, y.saturating_sub(n(0))),
            'B' | 'e' => self.goto(x, y.saturating_add(n(0))),
            'C' | 'a' => self.goto(x.saturating_add(n(0)), y),
            'D' => self.goto(x.saturating_sub(n(0)), y),
            'E' => self.goto(0, y.saturating_add(n(0))),
            'F' => self.goto(0, y.saturating_sub(n(0))),
            'G' | '`' => self.goto(n(0) - 1, y),
            'd' => self.goto_origin(x, n(0) - 1),
            'H' | 'f' => self.goto_origin(n(1) - 1, n(0) - 1),
            'J' => self.erase_display(raw(0)),
            'K' => self.erase_line(raw(0)),
            'X' => self.erase_chars(n(0)),
            'P' => self.delete_chars(n(0)),
            '@' => self.insert_chars(n(0)),
            'L' => self.insert_lines(n(0)),
            'M' => self.delete_lines(n(0)),
            'S' => self.scroll_lines(n(0), true),
            'T' => self.scroll_lines(n(0), false),
            'r' => self.set_region(n(0), if raw(1) == 0 { self.rows } else { raw(1) }),
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),
            'n' => self.status_report(raw(0)),
            'c' if raw(0) == 0 => self.respond("\x1b[?6c"),
            'h' | 'l' => {
                if p.contains(&4) {
                    self.insert = f == 'h';
                }
            }
            _ => {}
        }
    }

    fn sgr(&mut self, p: &[u32]) {
        if p.is_empty() {
            self.attr = DEFAULT_ATTR;
            self.reverse = false;
        }
        let mut i = 0;
        while i < p.len() {
            let v = p[i];
            let (fg, bg) = (self.attr & 15, self.attr >> 4);
            match v {
                0 => {
                    self.attr = DEFAULT_ATTR;
                    self.reverse = false;
                }
                1 => self.attr = attr(fg | 8, bg),
                22 => self.attr = attr(fg & 7, bg),
                7 => self.reverse = true,
                27 => self.reverse = false,
                30..=37 => self.attr = attr((v - 30) as u8 | fg & 8, bg),
                39 => self.attr = attr(FG_INDEX, bg),
                40..=47 => self.attr = attr(fg, (v - 40) as u8),
                49 => self.attr = attr(fg, super::BG_INDEX),
                90..=97 => self.attr = attr((v - 90) as u8 + 8, bg),
                100..=107 => self.attr = attr(fg, (v - 100) as u8 + 8),
                // 256 colours: the first 16 are the palette; 38;2 / 48;2 (RGB) are skipped whole.
                38 | 48 => {
                    let c = match (p.get(i + 1), p.get(i + 2)) {
                        (Some(5), Some(&c)) => {
                            i += 2;
                            (c < 16).then_some(c as u8)
                        }
                        (Some(2), _) => {
                            i += 4;
                            None
                        }
                        _ => None,
                    };
                    if let Some(c) = c {
                        self.attr = if v == 38 { attr(c, bg) } else { attr(fg, c) };
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn dec_mode(&mut self, v: u32, on: bool) {
        match v {
            6 => {
                self.origin = on;
                self.goto_origin(0, 0);
            }
            7 => self.autowrap = on,
            25 => self.cursor_on = on,
            1049 => self.alt_screen(on, true),
            47 | 1047 => self.alt_screen(on, false),
            _ => {}
        }
    }

    fn status_report(&mut self, v: u32) {
        match v {
            5 => self.respond("\x1b[0n"),
            6 => {
                let y = if self.origin { self.row.saturating_sub(self.top) } else { self.row };
                let s = format!("\x1b[{};{}R", y + 1, self.x() + 1);
                self.respond(&s);
            }
            _ => {}
        }
    }

    fn respond(&mut self, s: &str) {
        if self.reply.len() + s.len() <= REPLY_MAX {
            self.reply.extend_from_slice(s.as_bytes());
        }
    }
}
