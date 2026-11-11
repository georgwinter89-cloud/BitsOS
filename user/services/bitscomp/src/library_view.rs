//! PK-L1b: what the program library's overlay shows and how its focus moves, as pure functions (host tests in
//! library_view_tests.rs). The rows (desk-PK67-plan.md §3 "What it shows"): "Favoriten" from `desktop.favourites`,
//! "Zuletzt benutzt" from the start page's recent list counted by program (up to RECENT_MAX), "Alle Programme" A-Z;
//! while a search is typed, one row of hits, best first. The geometry is in logical px from the Feld board's sizes
//! (board px, `Theme::bl`), so the look (library_draw.rs) and the hit tests read the same rectangles.

use crate::layout::Rect;
use crate::library::{Entry, Index};
use crate::theme::Theme;

pub const RECENT_MAX: usize = 8;
pub const FAVOURITES: &str = "Favoriten";
pub const RECENT: &str = "Zuletzt benutzt";
pub const ALL: &str = "Alle Programme";
pub const HITS: &str = "Treffer";

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub title: &'static str,
    /// Entry ids, in the order shown.
    pub ids: Vec<String>,
}

/// The library entry an address of the favourites or the recent list names: its own address or id, a window's
/// app_id word (`programm/libreoffice-writer` is Writer by its StartupWMClass), any settings page (Einstellungen).
pub fn entry_of<'a>(ix: &'a Index, addr: &str) -> Option<&'a Entry> {
    let a = crate::address::normalise(addr);
    if a == "einstellungen" || a.starts_with("einstellungen/") {
        return ix.by_address("einstellungen");
    }
    ix.by_address(&a).or_else(|| {
        let word = a.strip_prefix("programm/")?;
        ix.entries.iter().find(|e| e.owns(word))
    })
}

/// The programs of `addrs` in their order, each once, at most `cap`; addresses of no program are left out.
pub fn programs(ix: &Index, addrs: &[String], cap: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for a in addrs {
        if out.len() >= cap {
            break;
        }
        if let Some(e) = entry_of(ix, a)
            && !out.contains(&e.id)
        {
            out.push(e.id.clone());
        }
    }
    out
}

/// The rows to show; empty rows are left out.
pub fn rows(ix: &Index, favourites: &[String], recent: &[String], query: &str) -> Vec<Row> {
    let all = if query.trim().is_empty() {
        vec![
            Row { title: FAVOURITES, ids: programs(ix, favourites, usize::MAX) },
            Row { title: RECENT, ids: programs(ix, recent, RECENT_MAX) },
            Row { title: ALL, ids: ix.entries.iter().map(|e| e.id.clone()).collect() },
        ]
    } else {
        vec![Row { title: HITS, ids: ix.matches(query).iter().map(|e| e.id.clone()).collect() }]
    };
    all.into_iter().filter(|r| !r.ids.is_empty()).collect()
}

/// Whether `id` is among the favourites (by any address that names it).
pub fn pinned(ix: &Index, favourites: &[String], id: &str) -> bool {
    favourites.iter().any(|a| entry_of(ix, a).is_some_and(|e| e.id == id))
}

/// The favourites with `id` added at the end (its address), unchanged when it is there already.
pub fn pin(ix: &Index, favourites: &[String], id: &str) -> Vec<String> {
    let mut out = favourites.to_vec();
    if let Some(e) = ix.entries.iter().find(|e| e.id == id)
        && !pinned(ix, favourites, id)
    {
        out.push(e.address.clone());
    }
    out
}

/// The favourites without every address that names `id`; the others keep their order.
pub fn unpin(ix: &Index, favourites: &[String], id: &str) -> Vec<String> {
    favourites.iter().filter(|a| entry_of(ix, a).is_none_or(|e| e.id != id)).cloned().collect()
}

/// The focus (row, entry id) as (row, index) in `rows`: the id in its row, else in any row (a search or rescan
/// moved it), else the first tile; None when nothing is shown.
pub fn locate(rows: &[Row], focus: Option<&(usize, String)>) -> Option<(usize, usize)> {
    let first = (!rows.is_empty() && !rows[0].ids.is_empty()).then_some((0, 0));
    let Some((r, id)) = focus else { return first };
    let find = |k: usize| rows.get(k).and_then(|row| row.ids.iter().position(|x| x == id)).map(|i| (k, i));
    find(*r).or_else(|| (0..rows.len()).find_map(find)).or(first)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    Left,
    Right,
    Up,
    Down,
    /// Tab and Shift+Tab: the first tile of the next (previous) row, wrapping around.
    NextRow,
    PrevRow,
}

/// The focus `(row, index)` after `m` in a grid of `cols` columns; arrows cross into the next or previous row.
pub fn step(lens: &[usize], cols: usize, at: (usize, usize), m: Move) -> (usize, usize) {
    let cols = cols.max(1);
    let n = lens.len();
    if n == 0 || lens.contains(&0) {
        return (0, 0);
    }
    let (r, i) = (at.0.min(n - 1), at.1.min(lens[at.0.min(n - 1)].saturating_sub(1)));
    let len = lens[r];
    match m {
        Move::Right if i + 1 < len => (r, i + 1),
        Move::Right if r + 1 < n => (r + 1, 0),
        Move::Left if i > 0 => (r, i - 1),
        Move::Left if r > 0 => (r - 1, lens[r - 1] - 1),
        Move::Down if i + cols < len => (r, i + cols),
        // A shorter last line below: its last tile.
        Move::Down if i / cols < (len - 1) / cols => (r, len - 1),
        Move::Down if r + 1 < n => (r + 1, (i % cols).min(lens[r + 1] - 1)),
        Move::Up if i >= cols => (r, i - cols),
        Move::Up if r > 0 => {
            let p = lens[r - 1];
            (r - 1, ((p - 1) / cols * cols + i % cols).min(p - 1))
        }
        Move::NextRow => ((r + 1) % n, 0),
        Move::PrevRow => ((r + n - 1) % n, 0),
        _ => (r, i),
    }
}

/// The overlay's rectangles (logical px).
#[derive(Clone, Debug, PartialEq)]
pub struct Geo {
    /// What is dimmed: the screen below the top bar (the top bar's "Programme" button stays to close it).
    pub dim: Rect,
    pub panel: Rect,
    pub search: Rect,
    /// The tiles' area (scrolled inside it).
    pub body: Rect,
    /// The key hints under the tiles.
    pub hints: Rect,
    pub cols: usize,
    pub cell: (i32, i32),
    pub head: i32,
    pub gap: i32,
}

/// The Feld board's panel (1120x800 board px), centred under the top bar and kept on the screen.
pub fn geo(full: Rect, top_h: i32, t: &Theme) -> Geo {
    let b = |v: f32| t.bl(v);
    let dim = Rect { x: full.x, y: full.y + top_h, w: full.w, h: (full.h - top_h).max(1) };
    let margin = b(24.0);
    let w = b(1120.0).min(dim.w - 2 * margin).max(1);
    let h = b(800.0).min(dim.h - 2 * margin).max(1);
    let panel = Rect { x: dim.x + (dim.w - w) / 2, y: dim.y + (dim.h - h) / 2, w, h };
    let pad = b(32.0);
    let search = Rect { x: panel.x, y: panel.y, w: panel.w, h: b(96.0).min(h) };
    let hints = Rect { x: panel.x, y: panel.y + panel.h - b(48.0), w: panel.w, h: b(48.0) };
    let top = search.y + search.h + b(12.0);
    let body = Rect { x: panel.x + pad, y: top, w: (panel.w - 2 * pad).max(1), h: (hints.y - top).max(1) };
    let (cell, gap) = ((b(144.0), b(150.0)), b(8.0));
    let cols = ((body.w + gap) / (cell.0 + gap)).max(1) as usize;
    Geo { dim, panel, search, body, hints, cols, cell, head: b(36.0), gap }
}

/// Where each row's heading and tiles lie in the scrolled content (y from the body's top, before scrolling), and
/// the content's height.
pub fn place(g: &Geo, lens: &[usize]) -> (Vec<(Rect, Vec<Rect>)>, i32) {
    let mut y = 0;
    let mut out = Vec::with_capacity(lens.len());
    for &n in lens {
        let head = Rect { x: g.body.x, y, w: g.body.w, h: g.head };
        y += g.head;
        let tiles = (0..n)
            .map(|i| {
                let (c, l) = ((i % g.cols) as i32, (i / g.cols) as i32);
                Rect { x: g.body.x + c * (g.cell.0 + g.gap), y: y + l * (g.cell.1 + g.gap), w: g.cell.0, h: g.cell.1 }
            })
            .collect();
        let lines = n.div_ceil(g.cols) as i32;
        y += lines * (g.cell.1 + g.gap) + g.gap;
        out.push((head, tiles));
    }
    (out, y)
}

/// The scroll offset that keeps `r` (content coordinates) inside a body of height `h`, its row's heading too when
/// it is in the row's first line; never past the content's end.
pub fn scroll_to(scroll: i32, r: Rect, head: Option<Rect>, h: i32, content: i32) -> i32 {
    let top = head.map_or(r.y, |hd| hd.y.min(r.y));
    let s = if top < scroll {
        top
    } else if r.y + r.h > scroll + h {
        // A body lower than one tile shows the tile's top.
        (r.y + r.h - h).min(r.y)
    } else {
        scroll
    };
    s.clamp(0, (content - h).max(0))
}

/// The scroll offset after the wheel (`steps`, positive = down), kept in the content.
pub fn wheel(scroll: i32, steps: f64, unit: i32, h: i32, content: i32) -> i32 {
    (scroll + (steps * unit as f64).round() as i32).clamp(0, (content - h).max(0))
}

#[cfg(test)]
#[path = "library_view_tests.rs"]
mod tests;
