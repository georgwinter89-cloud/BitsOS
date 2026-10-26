use super::*;
use crate::tabs;

fn t() -> &'static Theme {
    crate::theme::test_theme()
}
const F: Rect = Rect { x: 100, y: 50, w: 400, h: 300 };
const TILED2: Place = Place::Tiled { tile: 0, index: 0, depth: 2, top: 0 };

fn centre(r: Rect) -> (f64, f64) {
    (r.x as f64 + r.w as f64 / 2.0, r.y as f64 + r.h as f64 / 2.0)
}

#[test]
fn header_parts() {
    let (cx, cy) = centre(close_button(F, t()));
    assert_eq!(header_part(F, TILED2, t(), cx, cy), Part::Close);
    let (fx, fy) = centre(float_button(F, t()));
    assert_eq!(header_part(F, TILED2, t(), fx, fy), Part::Float);
    let s = tabs::strip(F, 2, 0, t());
    for (i, &r) in s.tabs.iter().enumerate() {
        assert_eq!(header_part(F, TILED2, t(), r.x as f64 + 4.0, F.y as f64 + 1.0), Part::Tab(i), "full height");
        let (xx, xy) = centre(tabs::close_rect(r, t()));
        assert_eq!(header_part(F, TILED2, t(), xx, xy), Part::TabClose(i));
    }
    let end = s.tabs[1].x + s.tabs[1].w;
    assert_eq!(header_part(F, TILED2, t(), end as f64 + 2.0, 60.0), Part::Body, "between tabs and buttons");
    // Below the header (a border) is the body too.
    assert_eq!(header_part(F, TILED2, t(), 101.0, 200.0), Part::Body);
    // A floating window has one tab.
    let float = Place::Floating { index: 0 };
    let one = tabs::strip(F, 1, 0, t());
    assert_eq!(header_part(F, float, t(), one.tabs[0].x as f64 + 4.0, 60.0), Part::Tab(0));
}

#[test]
fn overflow_chip_and_reorder_slots() {
    let narrow = Rect { w: 240, ..F };
    let place = Place::Tiled { tile: 0, index: 0, depth: 6, top: 5 };
    let s = tabs::strip(narrow, 6, 5, t());
    let (more, _) = s.more.unwrap();
    let (mx, my) = centre(more);
    assert_eq!(header_part(narrow, place, t(), mx, my), Part::More);
    let first = s.tabs[0];
    assert_eq!(header_part(narrow, place, t(), first.x as f64 + 4.0, my), Part::Tab(s.first));
    // Dragging page 0 of a three-tab strip.
    let s = tabs::strip(F, 3, 0, t());
    let x = |i: usize| s.tabs[i].x as f64 + s.tabs[i].w as f64 * 0.75;
    assert_eq!(reorder_target(&s, 0, 0, x(0)), Drop::None, "its own place");
    assert_eq!(reorder_target(&s, 0, 0, x(1)), Drop::Reorder { tile: 0, slot: 2, to: 1 });
    assert_eq!(reorder_target(&s, 0, 0, x(2)), Drop::Reorder { tile: 0, slot: 3, to: 2 });
    assert_eq!(reorder_target(&s, 0, 2, s.tabs[0].x as f64), Drop::Reorder { tile: 0, slot: 0, to: 0 });
}

#[test]
fn float_edges_and_corners() {
    let e = |x, y| resize_edges(F, t(), x, y);
    assert_eq!(e(97.0, 200.0), Some(Edges { left: true, ..Default::default() }));
    assert_eq!(e(503.0, 200.0), Some(Edges { right: true, ..Default::default() }));
    assert_eq!(e(300.0, 349.0), Some(Edges { bottom: true, ..Default::default() }));
    assert_eq!(e(502.0, 352.0), Some(Edges { right: true, bottom: true, ..Default::default() }));
    assert_eq!(e(104.0, 46.0), Some(Edges { left: true, top: true, ..Default::default() }));
    assert_eq!(e(300.0, 200.0), None, "inside is the client");
    assert_eq!(e(300.0, 30.0), None, "far outside is nothing");
}

#[test]
fn resizing_keeps_the_opposite_edge_and_a_minimum() {
    let br = Edges { right: true, bottom: true, ..Default::default() };
    assert_eq!(resized(F, br, 50.0, 20.0, (160, 100)), Rect { w: 450, h: 320, ..F });
    let tl = Edges { left: true, top: true, ..Default::default() };
    let r = resized(F, tl, 1000.0, 1000.0, (160, 100));
    assert_eq!((r.x + r.w, r.y + r.h, r.w, r.h), (500, 350, 160, 100));
}

fn three() -> Vec<Rect> {
    // Like Workspace::tile_frames of three equal tiles in 1000x500 with gap 10.
    vec![
        Rect { x: 10, y: 10, w: 317, h: 480 },
        Rect { x: 337, y: 10, w: 317, h: 480 },
        Rect { x: 664, y: 10, w: 326, h: 480 },
    ]
}

#[test]
fn gaps_between_tiles() {
    let f = three();
    assert_eq!(gap_at(&f, t(), 332.0, 200.0), Some(0));
    assert_eq!(gap_at(&f, t(), 659.0, 200.0), Some(1));
    assert_eq!(gap_at(&f, t(), 200.0, 200.0), None);
    assert_eq!(gap_at(&f, t(), 332.0, 5.0), None, "above the tiles is empty space");
}

#[test]
fn drop_zones() {
    let f = three();
    // From the single window in tile 0.
    assert_eq!(drop_target(&f, 0, 1, t(), 500.0, 200.0), Drop::Stack(1));
    assert_eq!(drop_target(&f, 0, 1, t(), 345.0, 200.0), Drop::None, "left of tile 1 is where it already is");
    assert_eq!(drop_target(&f, 0, 1, t(), 640.0, 200.0), Drop::Beside(1, Side::Right));
    assert_eq!(drop_target(&f, 0, 1, t(), 900.0, 200.0), Drop::Stack(2));
    assert_eq!(drop_target(&f, 0, 1, t(), 150.0, 200.0), Drop::None, "onto itself");
    assert_eq!(drop_target(&f, 0, 1, t(), 995.0, 200.0), Drop::OwnTile(3));
    // From a stack in tile 1: its own edges take it out beside, its own centre does nothing.
    assert_eq!(drop_target(&f, 1, 2, t(), 500.0, 200.0), Drop::None);
    assert_eq!(drop_target(&f, 1, 2, t(), 345.0, 200.0), Drop::Beside(1, Side::Left));
    assert_eq!(drop_target(&f, 1, 2, t(), 332.0, 200.0), Drop::OwnTile(1), "the gap is empty space");
}

#[test]
fn drop_previews() {
    let f = three();
    let area = Rect { x: 0, y: 0, w: 1000, h: 500 };
    assert_eq!(drop_preview(&f, Drop::Stack(1), area, 10), Some(f[1]));
    let left = drop_preview(&f, Drop::Beside(2, Side::Left), area, 10).unwrap();
    assert_eq!((left.x, left.w), (664, 163));
    let bar = drop_preview(&f, Drop::OwnTile(1), area, 10).unwrap();
    assert!(bar.x < 332 && bar.x + bar.w > 332);
    assert_eq!(drop_preview(&f, Drop::None, area, 10), None);
}

#[test]
fn a_floating_header_has_a_maximise_button_and_its_tab_ends_before_it() {
    let float = Place::Floating { index: 0 };
    let m = max_button(F, t());
    assert_eq!(m.x + m.w + 2, float_button(F, t()).x);
    let (mx, my) = centre(m);
    assert_eq!(header_part(F, float, t(), mx, my), Part::Max);
    assert_ne!(header_part(F, TILED2, t(), mx, my), Part::Max, "tiles have no maximise button");
    let narrow = Rect { w: 200, ..F };
    let s = tabs::strip(tab_frame(narrow, true, t()), 1, 0, t());
    assert!(s.tabs[0].x + s.tabs[0].w <= max_button(narrow, t()).x, "the tab ends before the button");
}

#[test]
fn double_clicks_are_quick_and_near() {
    use std::time::Duration;
    let ms = Duration::from_millis;
    let prev = Some((ms(1000), (100.0, 50.0)));
    assert!(is_double(prev, ms(1300), (102.0, 51.0), 6.0));
    assert!(!is_double(prev, ms(1401), (100.0, 50.0), 6.0), "too late");
    assert!(!is_double(prev, ms(1100), (120.0, 50.0), 6.0), "too far");
    assert!(!is_double(None, ms(1100), (100.0, 50.0), 6.0));
}
