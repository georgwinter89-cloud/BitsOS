//! The circle menu (plan/spec/desktop.md §1 and §5): right click on a header or on empty space, eight fixed
//! directions around the click point (0 = up, then clockwise). Drawn by `menu_draw` (WY-5: holographic rings).

use crate::layout::Rect;
use crate::theme::Theme;
use crate::wm::Action;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Item {
    pub label: &'static str,
    pub action: Action,
    pub enabled: bool,
}

pub type Items = [Option<Item>; 8];

pub struct Menu<W> {
    /// Logical pixels.
    pub centre: (f64, f64),
    /// The window the menu was opened on; `None` on empty space.
    pub target: Option<W>,
    pub items: Items,
    pub hover: Option<usize>,
    /// The right button is still down from opening: its release may pick an item.
    pub held: bool,
    /// Where the pointer was when the menu opened; near the output's edges that is not the centre.
    pub opened_at: (f64, f64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuHit {
    Centre,
    Item(usize),
    Outside,
}

fn item(label: &'static str, action: Action, enabled: bool) -> Option<Item> {
    Some(Item { label, action, enabled })
}

/// `maximised`: for a floating window whether it is maximised (WY-5c: its menu has "Maximieren" or
/// "Wiederherstellen"); None for a tiled one.
pub fn window_items(floating: bool, maximised: Option<bool>, can_stack: bool, in_stack: bool) -> Items {
    let mut m: Items = [None; 8];
    m[0] = item(if floating { "Kachel" } else { "Schweben" }, Action::Float, true);
    if let Some(max) = maximised.filter(|_| floating) {
        m[3] = item(if max { "Wiederherstellen" } else { "Maximieren" }, Action::Maximise, true);
    }
    m[1] = item("Auf Stapel legen", Action::Stack, can_stack);
    m[2] = item("Breiter", Action::Grow(0.25), !floating);
    m[4] = item("Schließen", Action::Close, true);
    m[5] = item("Programm beenden", Action::Kill, true);
    m[6] = item("Schmaler", Action::Grow(-0.25), !floating);
    m[7] = item("Aus Stapel nehmen", Action::Unstack, in_stack);
    m
}

/// On empty space: the workspace actions (desktop.md §1); `can_remove`: this workspace is empty and not the only one.
pub fn empty_items(can_remove: bool) -> Items {
    let mut m: Items = [None; 8];
    m[0] = item("Neues Terminal", Action::Terminal, true);
    m[2] = item("Neuer Arbeitsraum", Action::NewWorkspace, true);
    m[4] = item("bitscomp beenden", Action::Quit, true);
    m[6] = item("Arbeitsraum entfernen", Action::RemoveWorkspace, can_remove);
    m
}

fn unit(dir: usize) -> (f64, f64) {
    let a = dir as f64 * std::f64::consts::FRAC_PI_4;
    (a.sin(), -a.cos())
}

/// An item's box relative to the centre: boxes right of the centre start on the ring, left ones end on it,
/// the top and bottom ones are centred on it.
pub fn item_rect(dir: usize, t: &Theme) -> Rect {
    let (ux, uy) = unit(dir);
    let r = t.menu_radius as f64;
    let (w, h) = t.menu_item_size;
    let (ax, ay) = (ux * r, uy * r);
    let x = if ux > 0.1 {
        ax
    } else if ux < -0.1 {
        ax - w as f64
    } else {
        ax - w as f64 / 2.0
    };
    Rect { x: x.round() as i32, y: (ay - h as f64 / 2.0).round() as i32, w, h }
}

/// Half width and half height of the whole menu, rounded up to a multiple of 2 so its image maps 1:1 at 125 %.
pub fn extent(t: &Theme) -> (i32, i32) {
    let up = |v: i32| (v + 1) / 2 * 2;
    let rings = t.menu_radius + RING_OUT + 2;
    (up((t.menu_radius + t.menu_item_size.0).max(rings)), up((t.menu_radius + t.menu_item_size.1 / 2).max(rings)))
}

/// The holographic rings (Details.dc.html): the dashed outer ring and the thin inner one, from the item ring.
pub const RING_OUT: i32 = 30;
pub const RING_IN: i32 = 30;

/// Keeps the whole menu on the output.
pub fn clamp_centre(x: f64, y: f64, area: Rect, t: &Theme) -> (f64, f64) {
    let (ex, ey) = extent(t);
    let fit = |v: f64, lo: i32, len: i32, e: i32| {
        if len < 2 * e { (lo + len / 2) as f64 } else { v.clamp((lo + e) as f64, (lo + len - e) as f64) }
    };
    (fit(x, area.x, area.w, ex).round(), fit(y, area.y, area.h, ey).round())
}

/// Item boxes are hit directly; inside the ring the direction counts, so a quick flick picks an item too.
pub fn hit(centre: (f64, f64), items: &Items, t: &Theme, x: f64, y: f64) -> MenuHit {
    let (dx, dy) = (x - centre.0, y - centre.1);
    if let Some(d) = (0..8).find(|&d| items[d].is_some() && item_rect(d, t).contains(dx, dy)) {
        return MenuHit::Item(d);
    }
    let dist = dx.hypot(dy);
    if dist < t.menu_dead as f64 {
        return MenuHit::Centre;
    }
    if dist > t.menu_radius as f64 {
        return MenuHit::Outside;
    }
    let d = (dx.atan2(-dy) / std::f64::consts::FRAC_PI_4).round().rem_euclid(8.0) as usize;
    if items[d].is_some() { MenuHit::Item(d) } else { MenuHit::Centre }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: (f64, f64) = (500.0, 400.0);

    #[test]
    fn boxes_do_not_overlap() {
        let r: Vec<Rect> = (0..8).map(|d| item_rect(d, crate::theme::test_theme())).collect();
        for a in 0..8 {
            for b in a + 1..8 {
                let (p, q) = (r[a], r[b]);
                let apart = p.x + p.w <= q.x || q.x + q.w <= p.x || p.y + p.h <= q.y || q.y + q.h <= p.y;
                assert!(apart, "items {a} and {b} overlap: {p:?} {q:?}");
            }
        }
    }

    #[test]
    fn directions_and_dead_centre() {
        let items = window_items(false, None, true, false);
        let t = crate::theme::test_theme();
        assert_eq!(hit(C, &items, t, C.0 + 3.0, C.1 - 2.0), MenuHit::Centre);
        assert_eq!(hit(C, &items, t, C.0, C.1 - 60.0), MenuHit::Item(0));
        assert_eq!(hit(C, &items, t, C.0 + 60.0, C.1), MenuHit::Item(2));
        assert_eq!(hit(C, &items, t, C.0, C.1 + 60.0), MenuHit::Item(4));
        // South-east has no item: inside the ring that is nothing, far out it is outside.
        assert_eq!(hit(C, &items, t, C.0 + 45.0, C.1 + 45.0), MenuHit::Centre);
        assert_eq!(hit(C, &items, t, C.0 + 300.0, C.1 + 300.0), MenuHit::Outside);
        // The far end of the "Breiter" box still picks it.
        let r = item_rect(2, t);
        assert_eq!(hit(C, &items, t, C.0 + (r.x + r.w - 2) as f64, C.1), MenuHit::Item(2));
    }

    #[test]
    fn menus_have_the_planned_entries() {
        let w = window_items(true, Some(false), false, false);
        assert_eq!(w[0].unwrap().label, "Kachel");
        assert!(!w[1].unwrap().enabled && !w[2].unwrap().enabled && !w[7].unwrap().enabled);
        assert_eq!((w[5].unwrap().label, w[5].unwrap().action), ("Programm beenden", Action::Kill));
        assert_eq!((w[3].unwrap().label, w[3].unwrap().action), ("Maximieren", Action::Maximise));
        assert_eq!(window_items(true, Some(true), false, false)[3].unwrap().label, "Wiederherstellen");
        assert!(window_items(false, Some(true), true, false)[3].is_none(), "a tile has no maximise item");
        let e = empty_items(false);
        assert_eq!(e.iter().flatten().count(), 4);
        assert_eq!(e[0].unwrap().action, Action::Terminal);
        assert_eq!(e[2].unwrap().action, Action::NewWorkspace);
        assert!(!e[6].unwrap().enabled && empty_items(true)[6].unwrap().enabled);
    }

    #[test]
    fn every_label_fits_its_box() {
        let t = crate::theme::test_theme();
        let font = crate::menu_draw::item_font(t);
        let w = ((t.menu_item_size.0 - 2 * crate::menu_draw::ITEM_PAD) as f64 * t.scale).round() as i32;
        for it in window_items(false, None, true, true).iter().chain(window_items(true, Some(true), false, false).iter()).chain(empty_items(true).iter()).flatten() {
            assert!(font.width(it.label) <= w, "{} is {} px, the box {w}", it.label, font.width(it.label));
        }
    }

    #[test]
    fn menu_stays_on_the_output() {
        let area = Rect { x: 0, y: 0, w: 1000, h: 600 };
        let (ex, ey) = extent(crate::theme::test_theme());
        assert_eq!(clamp_centre(5.0, 5.0, area, crate::theme::test_theme()), (ex as f64, ey as f64));
        assert_eq!(clamp_centre(500.0, 300.0, area, crate::theme::test_theme()), (500.0, 300.0));
    }
}
