//! GM-6a: the sofa mode's focus model (plan/spec/sofa-mode.md §4), pure: one focused element on the whole screen,
//! moved by the arrow keys, the D-pad and the stick (and set by the mouse's hover, `at`). A move goes to the nearest
//! element in that direction by the elements' centres, preferring the same row or column, inside the element's
//! area; at the area's edge it goes over to the next area (content -> header pills up, header -> content down,
//! content -> left column left, the column -> content right) and never wraps on a page; an element off the row
//! taken sideways only within 45 degrees of the move. The on-screen keyboard's keys wrap per row instead and
//! keep the column by x going up or down (§2.6). B goes back one level (`Nav::back`). Coordinates are board px (1920x1080
//! boards, `sofa_layout`); host-tested on every screen's layout in `sofa_focus_tests.rs`.

use crate::layout::Rect;
use crate::pad::Dir;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    /// The header's pills, search and Menü button.
    Header,
    /// The left column (Sammlungen, the settings sections).
    Side,
    /// The screen's own tiles, cards and rows.
    Content,
    /// The quick menu's panel (an overlay: no way out by moving).
    Panel,
    /// The on-screen keyboard and the search hits above it (wraps per row).
    Keys,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: String,
    pub rect: Rect,
    pub area: Area,
}

impl Item {
    pub fn new(id: impl Into<String>, rect: Rect, area: Area) -> Item {
        Item { id: id.into(), rect, area }
    }
}

fn centre(r: Rect) -> (f64, f64) {
    (r.x as f64 + r.w as f64 / 2.0, r.y as f64 + r.h as f64 / 2.0)
}

fn overlap(a: i32, al: i32, b: i32, bl: i32) -> bool {
    a < b + bl && b < a + al
}

/// Which area a move in `d` goes over to when its own area has nothing there.
fn next_area(from: Area, d: Dir, to: Area) -> bool {
    matches!(
        (from, d, to),
        (Area::Content | Area::Side, Dir::Up, Area::Header)
            | (Area::Header, Dir::Down, Area::Content | Area::Side)
            | (Area::Content, Dir::Left, Area::Side)
            | (Area::Side, Dir::Right, Area::Content)
    )
}

/// Whether `a` lies inside `b` (a button on a tile, GM-6b's Details on the hero).
fn inside(a: Rect, b: Rect) -> bool {
    a != b && a.x >= b.x && a.y >= b.y && a.x + a.w <= b.x + b.w && a.y + a.h <= b.y + b.h
}

/// The element a move in `d` from `items[from]` lands on; None at the edge. An element lying on another (a
/// button on a tile) is reached by Right from that one and left by Left back to it; its other moves go as its
/// tile's do, past the tile's own buttons.
pub fn step(items: &[Item], from: usize, d: Dir) -> Option<usize> {
    let f = items.get(from)?;
    let base = match items.iter().position(|it| inside(f.rect, it.rect)) {
        Some(c) if d == Dir::Left => return Some(c),
        Some(c) => c,
        None => {
            if d == Dir::Right
                && let Some(j) = items.iter().position(|it| inside(it.rect, f.rect))
            {
                return Some(j);
            }
            from
        }
    };
    let tile = items[base].rect;
    if !items.iter().any(|it| inside(it.rect, tile)) {
        return step_flat(items, from, d);
    }
    let rest: Vec<usize> = (0..items.len()).filter(|i| !inside(items[*i].rect, tile)).collect();
    let view: Vec<Item> = rest.iter().map(|i| items[*i].clone()).collect();
    let at = rest.iter().position(|i| *i == base)?;
    step_flat(&view, at, d).map(|j| rest[j])
}

fn step_flat(items: &[Item], from: usize, d: Dir) -> Option<usize> {
    let f = items.get(from)?;
    let (fx, fy) = centre(f.rect);
    let horizontal = matches!(d, Dir::Left | Dir::Right);
    let ahead = |c: (f64, f64)| match d {
        Dir::Left => c.0 < fx - 0.5,
        Dir::Right => c.0 > fx + 0.5,
        Dir::Up => c.1 < fy - 0.5,
        Dir::Down => c.1 > fy + 0.5,
    };
    let lined = |r: Rect| if horizontal { overlap(r.y, r.h, f.rect.y, f.rect.h) } else { overlap(r.x, r.w, f.rect.x, f.rect.w) };
    let by_centres = |r: Rect| {
        let c = centre(r);
        if horizontal { ((c.0 - fx).abs(), (c.1 - fy).abs()) } else { ((c.1 - fy).abs(), (c.0 - fx).abs()) }
    };
    // The free space between the two along the move, and across it (0 when they overlap).
    let gap = |r: Rect| {
        let g = match d {
            Dir::Right => r.x - (f.rect.x + f.rect.w),
            Dir::Left => f.rect.x - (r.x + r.w),
            Dir::Down => r.y - (f.rect.y + f.rect.h),
            Dir::Up => f.rect.y - (r.y + r.h),
        };
        g.max(0) as f64
    };
    let side_gap = |r: Rect| {
        let (a, al, b, bl) = if horizontal { (r.y, r.h, f.rect.y, f.rect.h) } else { (r.x, r.w, f.rect.x, f.rect.w) };
        (b - (a + al)).max(a - (b + bl)).max(0) as f64
    };
    let keys_down = f.area == Area::Keys && !horizontal;
    // In the same row (column) the nearest by the space between, then by centres; off it the distance of the
    // centres along the move plus twice the one across. Up and down on the keyboard: the next row, the key
    // nearest by x (a wide key such as Leerzeichen is under every key it spans).
    let key = |r: Rect| {
        let (along, across) = by_centres(r);
        if keys_down {
            (0u8, gap(r), side_gap(r), across)
        } else if lined(r) {
            (0, gap(r), across, 0.0)
        } else {
            (1, along + 2.0 * across, across, 0.0)
        }
    };
    // An element off the row (column) counts only when it lies wholly beyond the focused one's edge, and sideways
    // only within 45 degrees: Right at a row's end does not jump down to a button far below. Up and down find the
    // next row whatever its x.
    let beyond = |r: Rect| match d {
        Dir::Right => r.x >= f.rect.x + f.rect.w,
        Dir::Left => r.x + r.w <= f.rect.x,
        Dir::Down => r.y >= f.rect.y + f.rect.h,
        Dir::Up => r.y + r.h <= f.rect.y,
    };
    let cone = |r: Rect| {
        let (along, across) = by_centres(r);
        lined(r) || (beyond(r) && (!horizontal || along >= across))
    };
    let best = |ok: &dyn Fn(&Item) -> bool| {
        items
            .iter()
            .enumerate()
            .filter(|(i, it)| *i != from && ok(it) && ahead(centre(it.rect)) && cone(it.rect))
            .min_by(|a, b| key(a.1.rect).partial_cmp(&key(b.1.rect)).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
    };
    if f.area == Area::Keys {
        if horizontal {
            // A key row: the next key in it, else round to the row's other end.
            let row = |it: &Item| it.area == Area::Keys && lined(it.rect);
            return best(&row).or_else(|| {
                let far = items.iter().enumerate().filter(|(i, it)| *i != from && row(it));
                let pick = |a: &(usize, &Item), b: &(usize, &Item)| centre(a.1.rect).0.partial_cmp(&centre(b.1.rect).0).unwrap_or(std::cmp::Ordering::Equal);
                if d == Dir::Right { far.min_by(pick) } else { far.max_by(pick) }.map(|(i, _)| i)
            });
        }
        return best(&|it: &Item| it.area == Area::Keys);
    }
    best(&|it: &Item| it.area == f.area).or_else(|| best(&|it: &Item| next_area(f.area, d, it.area)))
}

/// The element under the board point (x, y): the mouse's hover sets the focus there.
pub fn at(items: &[Item], x: f64, y: f64) -> Option<usize> {
    // The innermost: a button on a tile before the tile.
    let area = |r: Rect| r.w as i64 * r.h as i64;
    items.iter().enumerate().filter(|(_, it)| it.rect.contains(x, y)).min_by_key(|(i, it)| (area(it.rect), *i)).map(|(i, _)| i)
}

pub fn index(items: &[Item], id: &str) -> Option<usize> {
    items.iter().position(|it| it.id == id)
}

/// The screens of section 2; `Medien` is the library on its media collections (§2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Screen {
    #[default]
    Start,
    Bibliothek,
    Medien,
    Einstellungen,
    Suche,
}

impl Screen {
    /// The header's pills, left to right.
    pub const PILLS: [Screen; 4] = [Screen::Start, Screen::Bibliothek, Screen::Medien, Screen::Einstellungen];

    pub fn word(self) -> &'static str {
        match self {
            Screen::Start => "start",
            Screen::Bibliothek => "bibliothek",
            Screen::Medien => "medien",
            Screen::Einstellungen => "einstellungen",
            Screen::Suche => "suche",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Screen::Start => "Start",
            Screen::Bibliothek => "Bibliothek",
            Screen::Medien => "Medien",
            Screen::Einstellungen => "Einstellungen",
            Screen::Suche => "Suche",
        }
    }

    pub fn of_word(w: &str) -> Option<Screen> {
        [Screen::Start, Screen::Bibliothek, Screen::Medien, Screen::Einstellungen, Screen::Suche].into_iter().find(|s| s.word() == w)
    }

    /// The pill that shows as active on this screen (none on the search screen, which has no header).
    pub fn pill(self) -> Option<String> {
        (self != Screen::Suche).then(|| format!("pill:{}", self.word()))
    }
}

/// Where the shell is: the screen, the one the search came from, the quick menu over it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Nav {
    pub screen: Screen,
    pub from: Screen,
    pub quick: bool,
}

/// What B did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Back {
    /// The quick menu closed.
    Quick,
    /// Another screen shows.
    Screen(Screen),
    /// Einstellungen: from a row back to its section in the left column.
    Column,
    /// On Start: nothing (B never leaves the sofa mode).
    Stay,
}

impl Nav {
    pub fn go(&mut self, s: Screen) {
        if s == Screen::Suche && self.screen != Screen::Suche {
            self.from = self.screen;
        }
        self.screen = s;
    }

    /// B, Esc, a right click; `area` is the focused element's area.
    pub fn back(&mut self, area: Option<Area>) -> Back {
        if self.quick {
            self.quick = false;
            return Back::Quick;
        }
        let to = match self.screen {
            Screen::Start => return Back::Stay,
            Screen::Einstellungen if area == Some(Area::Content) => return Back::Column,
            Screen::Suche => self.from,
            Screen::Bibliothek | Screen::Medien | Screen::Einstellungen => Screen::Start,
        };
        self.screen = to;
        Back::Screen(to)
    }

    /// LB/RB on Start: the previous/next header pill (no wrap); None at the end or off the pills.
    pub fn pill_step(&self, by: i32) -> Option<Screen> {
        let i = Screen::PILLS.iter().position(|s| *s == self.screen)? as i32 + by;
        Screen::PILLS.get(usize::try_from(i).ok()?).copied()
    }
}

#[cfg(test)]
#[path = "sofa_focus_tests.rs"]
mod tests;
