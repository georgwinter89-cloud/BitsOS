//! The report on the firmware screen: pages of the console's text grid. 2560x1440 with the console font is 160 x 45
//! cells, and the Gaming-PC's report is longer than one screen, so it is paged with a visible "Seite i von n" that
//! turns by itself every [`TURN_S`] seconds and on any key. Nothing that says a driver gave up is ever cut: long rows
//! wrap onto the next screen row, a wrapped row never splits across two pages, and when the report has to shorten
//! only plain device rows go (the file and MCP keep them all).

use crate::{Group, Report};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Seconds a page stays before the next one comes by itself.
pub const TURN_S: u64 = 20;

/// More pages than this and the device list is shortened (Georg photographs each page; five is about 100 rows).
pub const MAX_PAGES: usize = 5;

/// Words that mark a row Georg must see whole: a driver that gave up or stopped, the device protection that stops
/// everything, a request to photograph.
pub const KEEP_MARKS: &[&str] = &["hat aufgegeben", "läuft NICHT", "angehalten bei Schritt", "Karte angehalten", "EINGESCHALTET", "fotografieren", "zu alt", crate::rows::UNREACHABLE];

/// A row Georg must see whole, never left out.
pub fn must_keep(row: &str) -> bool {
    KEEP_MARKS.iter().any(|m| row.contains(m))
}

/// Rows the page may leave out when it has to shorten: plain device lines of the device list (two spaces in).
fn droppable(g: Group, row: &str) -> bool {
    g == Group::Devices && row.starts_with("  ") && !must_keep(row)
}

/// `text` cut into rows of at most `cols` characters at spaces; a continuation row starts with four spaces. A word
/// longer than a row is split where the row ends: nothing is ever dropped.
pub fn wrap(text: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(20);
    let mut out = Vec::new();
    let body = text.trim_start_matches(' ');
    let lead = (text.len() - body.len()).min(cols / 4);
    let mut cur: String = core::iter::repeat_n(' ', lead).collect();
    let mut n = lead;
    let indent: String = core::iter::repeat_n(' ', lead + 4).collect();
    let mut first = true;
    for word in body.split(' ') {
        let w = word.chars().count();
        let gap = usize::from(!first);
        first = false;
        if n + gap + w <= cols {
            if gap == 1 {
                cur.push(' ');
            }
            cur.push_str(word);
            n += gap + w;
            continue;
        }
        if n > lead {
            out.push(core::mem::take(&mut cur));
            cur.push_str(&indent);
            n = indent.len();
        }
        let mut rest: Vec<char> = word.chars().collect();
        while n + rest.len() > cols {
            let room = cols - n;
            let (a, b) = rest.split_at(room);
            cur.extend(a);
            out.push(core::mem::take(&mut cur));
            cur.push_str(&indent);
            n = indent.len();
            rest = b.to_vec();
        }
        cur.extend(rest.iter());
        n += rest.len();
    }
    if n > lead || out.is_empty() {
        out.push(cur);
    }
    out
}

/// One block of screen rows that stays together on a page.
struct Atom {
    rows: Vec<String>,
}

/// The pages for a grid of `cols` x `rows` cells: each page is the full list of its screen rows, head and foot
/// included. At least one page.
pub fn layout(r: &Report, cols: u32, rows: u32) -> Vec<Vec<String>> {
    let (cols, rows) = (cols.max(20) as usize, rows.max(8) as usize);
    let mut leave_out = 0usize;
    loop {
        let pages = layout_leaving(r, cols, rows, leave_out);
        let can_drop = r.rows.iter().filter(|x| droppable(x.group, &x.text)).count();
        if pages.len() <= MAX_PAGES || leave_out >= can_drop {
            return pages;
        }
        leave_out += 1;
    }
}

/// Head (2 rows) and foot (a blank row and the foot's wrapped rows) around the body.
const HEAD: usize = 2;

fn foot_rows(cols: usize) -> usize {
    1 + wrap("Die Seiten blättern alle 20 Sekunden von selbst um. Eine Taste: nächste Seite. Nach der letzten Seite bringt eine Taste die Eingabe.", cols).len()
}

/// The question's rows (VB-1) with the blank row above them.
fn ask_rows(r: &Report, cols: usize) -> Vec<String> {
    let mut out = Vec::new();
    for l in &r.ask {
        if out.is_empty() {
            out.push(String::new());
        }
        out.extend(wrap(l, cols));
    }
    out
}

fn layout_leaving(r: &Report, cols: usize, rows: usize, leave_out: usize) -> Vec<Vec<String>> {
    let ask = ask_rows(r, cols);
    let body = rows.saturating_sub(HEAD + foot_rows(cols) + ask.len()).max(3);
    // The last `leave_out` droppable rows go, and one row says so.
    let droppables: Vec<usize> = r.rows.iter().enumerate().filter(|(_, x)| droppable(x.group, &x.text)).map(|(i, _)| i).collect();
    let gone: Vec<usize> = droppables.iter().rev().take(leave_out).copied().collect();
    let mut groups: Vec<(Group, Vec<Atom>)> = Vec::new();
    for g in Group::ALL {
        let mut atoms: Vec<Atom> = Vec::new();
        for (i, row) in r.rows.iter().enumerate().filter(|(_, x)| x.group == g) {
            if gone.contains(&i) {
                continue;
            }
            atoms.push(Atom { rows: wrap(&format!("  {}", row.text), cols) });
        }
        if g == Group::Devices && !gone.is_empty() {
            atoms.push(Atom { rows: wrap(&format!("  ... {} weitere Geräte ohne Auffälligkeit: vollständig in der Datei auf dem Stick und über MCP", gone.len()), cols) });
        }
        if !atoms.is_empty() {
            groups.push((g, atoms));
        }
    }
    let mut pages: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    for (g, atoms) in groups {
        let title = String::from(g.title());
        let need: usize = 1 + atoms.iter().map(|a| a.rows.len()).sum::<usize>() + usize::from(!cur.is_empty());
        // A group that fits on a fresh page but not on this one starts a new page.
        if !cur.is_empty() && cur.len() + need > body && need - 1 <= body {
            pages.push(core::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(String::new());
        }
        cur.extend(wrap(&title, cols));
        for a in atoms {
            if cur.len() + a.rows.len() > body && cur.len() > 1 {
                pages.push(core::mem::take(&mut cur));
                cur.extend(wrap(&format!("{title} (weiter)"), cols));
            }
            // An atom taller than a page (cannot happen with 160 columns) still goes whole: the page grows.
            cur.extend(a.rows);
        }
    }
    if !cur.is_empty() || pages.is_empty() {
        pages.push(cur);
    }
    let n = pages.len();
    pages.into_iter().enumerate().map(|(i, b)| frame(&r.head, b, &ask, i + 1, n, cols)).collect()
}

fn frame(head: &str, body: Vec<String>, ask: &[String], i: usize, n: usize, cols: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(body.len() + HEAD + foot_rows(cols));
    let right = format!("Seite {i} von {n}");
    let room = cols.saturating_sub(right.chars().count() + 2);
    let mut h: String = head.chars().take(room).collect();
    let pad = cols.saturating_sub(h.chars().count() + right.chars().count());
    h.extend(core::iter::repeat_n(' ', pad));
    h.push_str(&right);
    out.push(h);
    out.push(String::new());
    out.extend(body);
    out.extend(ask.iter().cloned());
    out.push(String::new());
    out.extend(wrap(
        if n > 1 {
            "Die Seiten blättern alle 20 Sekunden von selbst um. Eine Taste: nächste Seite. Nach der letzten Seite bringt eine Taste die Eingabe."
        } else {
            "Eine Taste bringt die Eingabe (zum Beispiel ls /boot)."
        },
        cols,
    ));
    out
}
