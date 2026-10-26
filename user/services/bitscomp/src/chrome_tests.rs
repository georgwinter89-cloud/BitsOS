use super::*;
use crate::reveal::Edge;
use crate::settings::Values;

fn inputs(size: (i32, i32), workspaces: usize) -> Inputs {
    Inputs {
        size,
        favourites: vec!["programm/terminal".into(), "einstellungen/aussehen".into()],
        workspaces,
        reveal: None,
    }
}

fn theme(pairs: &[(&str, &str)]) -> Theme {
    let mut v = Values::default();
    for (k, val) in pairs {
        v.set(k, val);
    }
    Theme::build(&v)
}

fn inside(a: Rect, b: Rect) -> bool {
    a.x >= b.x && a.y >= b.y && a.x + a.w <= b.x + b.w && a.y + a.h <= b.y + b.h
}

fn apart(a: Rect, b: Rect) -> bool {
    a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y
}

#[test]
fn mini_pc_2560x1440_at_125_percent() {
    let t = theme(&[]);
    assert_eq!(t.scale, 1.25);
    // 2560x1440 physical at 125 % is 2048x1152 logical.
    let c = layout(&t, &inputs((2048, 1152), 3));
    assert_eq!(c.top, Rect { x: 0, y: 0, w: 2048, h: 56 });
    assert_eq!(c.favs, Some(Rect { x: 0, y: 56, w: 2048, h: 34 }));
    assert_eq!(c.list, Rect { x: 0, y: 90, w: 212, h: 1152 - 90 - 32 });
    assert_eq!(c.side, Some(Rect { x: 2048 - 300, y: 90, w: 300, h: 1030 }));
    assert_eq!(c.status, Some(Rect { x: 0, y: 1120, w: 2048, h: 32 }));
    assert_eq!(c.work, Rect { x: 212, y: 90, w: 2048 - 212 - 300, h: 1030 });
    // The top bar's parts in a row, none overlapping, all inside it.
    let row = [c.mark, c.back, c.forward, c.address, c.ring];
    for w in row.windows(2) {
        assert!(w[0].x + w[0].w <= w[1].x, "{:?} before {:?}", w[0], w[1]);
    }
    assert!(row.iter().all(|r| inside(*r, c.top)));
    assert!(c.address.w > 1000, "the address bar takes the room: {}", c.address.w);
    assert_eq!(c.ws_items.len(), 3);
    assert_eq!(c.ws_shown(), 3);
    assert!(c.ws_items.iter().all(|r| r.h == WS_FULL && inside(*r, c.list)));
    assert!(apart(c.ws_new, c.home) && inside(c.home, c.list));
    assert_eq!(c.fav_items.len(), 2);
    assert!(c.fav_items.iter().all(|r| inside(*r, c.favs.unwrap()) && r.x >= c.work.x));
}

#[test]
fn small_1280x800_at_100_percent() {
    let t = theme(&[("theme.scale", "100")]);
    let c = layout(&t, &inputs((1280, 800), 1));
    assert_eq!(c.work, Rect { x: 212, y: 90, w: 1280 - 212 - 300, h: 800 - 90 - 32 });
    assert!(c.address.w > 400);
    for p in [c.top, c.list, c.work, c.side.unwrap(), c.status.unwrap()] {
        assert!(inside(p, Rect { x: 0, y: 0, w: 1280, h: 800 }), "{p:?}");
    }
    assert!(apart(c.list, c.work) && apart(c.work, c.side.unwrap()) && apart(c.work, c.status.unwrap()));
}

#[test]
fn layout_keys_and_the_fold_give_the_room_to_the_work_area() {
    let base = layout(&theme(&[]), &inputs((2048, 1152), 1)).work;
    let t = theme(&[("theme.layout.favourites_bar", "false"), ("theme.layout.statusbar", "false")]);
    let c = layout(&t, &inputs((2048, 1152), 1));
    assert_eq!((c.favs, c.status, c.fav_items.len()), (None, None, 0));
    assert_eq!(c.work, Rect { y: 56, h: 1152 - 56, ..base });
    let t = theme(&[("theme.layout.claude_sidebar", "false")]);
    assert_eq!(layout(&t, &inputs((2048, 1152), 1)).work.w, base.w + 300);
    assert_eq!(layout(&t, &inputs((2048, 1152), 1)).side, None);
    // A tile's own address bar changes the tiles, not the chrome.
    assert_eq!(layout(&theme(&[("theme.layout.address_bar", "tile")]), &inputs((2048, 1152), 1)).work, base);
}

#[test]
fn hits_find_the_parts_and_leave_the_work_area_alone() {
    let c = layout(&theme(&[]), &inputs((2048, 1152), 2));
    let mid = |r: Rect| ((r.x + r.w / 2) as f64, (r.y + r.h / 2) as f64);
    for (r, p) in [(c.address, Part::Address), (c.back, Part::Back), (c.ring, Part::Ring), (c.ws_items[1], Part::Ws(1)), (c.ws_new, Part::WsNew), (c.home, Part::Home), (c.fav_items[0], Part::Fav(0))] {
        let (x, y) = mid(r);
        assert_eq!(c.hit(x, y), Some(p));
    }
    let (x, y) = mid(c.work);
    assert_eq!(c.hit(x, y), None);
    assert!(!c.covers(x, y) && c.covers(5.0, 5.0));
}

#[test]
fn many_workspaces_get_lower_entries_and_never_leave_the_list() {
    let t = theme(&[("theme.scale", "100")]);
    let c = layout(&t, &inputs((1280, 800), 40));
    assert!(c.ws_items[0].h < WS_FULL && c.ws_items[0].h >= WS_LOW);
    let shown = c.ws_shown();
    assert!(shown > 5 && shown < 40, "{shown}");
    assert!(c.ws_items[..shown].iter().all(|r| r.y + r.h <= c.home.y));
    let (x, y) = ((c.ws_items[39].x + 5) as f64, (c.ws_items[39].y + 5) as f64);
    assert_ne!(c.hit(x, y), Some(Part::Ws(39)), "an entry below the list's end is not hit");
}

#[test]
fn both_side_bars_fold_away_and_the_tiles_take_the_width() {
    let base = layout(&theme(&[]), &inputs((2048, 1152), 2));
    assert!(base.list_docked && base.list_shown && base.side_docked && base.ws_button.is_none());
    // The fold button sits at the list's top edge, beside the title, inside the list.
    assert!(inside(base.ws_fold, base.list) && apart(base.ws_fold, base.ws_title) && base.ws_fold.y < base.ws_items[0].y);
    let (fx, fy) = ((base.ws_fold.x + 14) as f64, (base.ws_fold.y + 14) as f64);
    assert_eq!(base.hit(fx, fy), Some(Part::WsFold));
    let t = theme(&[("theme.layout.workspaces_bar", "false"), ("theme.layout.claude_sidebar", "false")]);
    let c = layout(&t, &inputs((2048, 1152), 2));
    assert_eq!(c.work, Rect { x: 0, y: 90, w: 2048, h: 1030 }, "the tiles take the whole width");
    assert!(!c.list_shown && c.side.is_none() && c.overlay().is_none());
    // Folded, nothing of the list reacts; the top bar has the workspaces button left of the mark.
    let (x, y) = ((c.ws_items[0].x + 20) as f64, (c.ws_items[0].y + 20) as f64);
    assert_eq!(c.hit(x, y), None);
    assert!(!c.covers(x, y));
    let b = c.ws_button.expect("workspaces button");
    assert!(inside(b, c.top) && b.x + b.w <= c.mark.x && b.x == 16);
    assert_eq!(c.hit((b.x + 10) as f64, (b.y + 10) as f64), Some(Part::WsButton));
    let row = [b, c.mark, c.back, c.forward, c.address, c.ring];
    assert!(row.windows(2).all(|w| w[0].x + w[0].w <= w[1].x), "the top bar's parts stay in a row");
    // The favourites start at the work area's left edge.
    assert!(c.fav_items[0].x == 16);
}

#[test]
fn a_revealed_bar_lies_over_the_tiles_and_is_hit() {
    let t = theme(&[("theme.layout.workspaces_bar", "false"), ("theme.layout.claude_sidebar", "false")]);
    let left = layout(&t, &Inputs { reveal: Some(Edge::Left), ..inputs((2048, 1152), 2) });
    assert!(left.list_shown && !left.list_docked);
    assert_eq!(left.work.x, 0, "a revealed list takes no width");
    assert_eq!(left.overlay(), Some(left.list));
    let (x, y) = ((left.ws_items[1].x + 20) as f64, (left.ws_items[1].y + 20) as f64);
    assert_eq!(left.hit(x, y), Some(Part::Ws(1)));
    assert!(left.covers(x, y), "the revealed list covers the tiles under it");
    let (fx, fy) = ((left.ws_fold.x + 14) as f64, (left.ws_fold.y + 14) as f64);
    assert_eq!(left.hit(fx, fy), Some(Part::WsFold), "its fold button pins it");
    let right = layout(&t, &Inputs { reveal: Some(Edge::Right), ..inputs((2048, 1152), 2) });
    let side = right.side.expect("revealed sidebar");
    assert_eq!(right.overlay(), Some(side));
    assert_eq!(right.work.w, 2048);
    assert!(right.covers((side.x + 10) as f64, (side.y + 10) as f64));
}

#[test]
fn only_folded_bars_have_an_edge() {
    let docked = layout(&theme(&[]), &inputs((2048, 1152), 1));
    assert_eq!(docked.edge_at(0.0, 500.0), None);
    assert_eq!(docked.edge_at(2047.5, 500.0), None);
    let t = theme(&[("theme.layout.workspaces_bar", "false"), ("theme.layout.claude_sidebar", "false")]);
    let c = layout(&t, &inputs((2048, 1152), 1));
    assert_eq!(c.edge_at(0.0, 500.0), Some(Edge::Left));
    assert_eq!(c.edge_at(0.9, 500.0), Some(Edge::Left));
    assert_eq!(c.edge_at(1.0, 500.0), None, "one logical pixel from the edge is not against it");
    assert_eq!(c.edge_at(2047.2, 500.0), Some(Edge::Right));
    assert_eq!(c.edge_at(0.0, 20.0), None, "the top bar's edge is not the list's");
    assert_eq!(c.edge_at(0.0, 1140.0), None, "nor the status bar's");
}
