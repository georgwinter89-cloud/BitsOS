use super::*;

const AREA: Rect = Rect { x: 0, y: 0, w: 1000, h: 500 };

fn ws(n: u32) -> Workspace<u32> {
    let mut ws = Workspace::default();
    for i in 1..=n {
        ws.add(i);
    }
    ws
}

fn visible(ws: &Workspace<u32>) -> Vec<u32> {
    ws.arrange(AREA, FULL, 10).into_iter().filter(|p| p.visible).map(|p| p.win).collect()
}

#[test]
fn tiles_split_the_width_without_overlap() {
    let ws = ws(3);
    let p = ws.arrange(AREA, FULL, 10);
    assert_eq!(p.len(), 3);
    assert_eq!(p[0].frame.x, 10);
    for pair in p.windows(2) {
        assert_eq!(pair[0].frame.x + pair[0].frame.w + 10, pair[1].frame.x);
    }
    assert_eq!(p[2].frame.x + p[2].frame.w, 990);
    assert!(p[2].focused);
}

#[test]
fn stack_and_flip_like_a_book() {
    let mut ws = ws(3);
    // Window 3 is focused; stacking puts it onto tile 2's stack.
    assert!(ws.stack_focused());
    assert_eq!(ws.tiles.len(), 2);
    assert_eq!(ws.tiles[1].stack, vec![2, 3]);
    assert_eq!(visible(&ws), vec![1, 3]);
    assert!(ws.flip(1, 1));
    assert_eq!(visible(&ws), vec![1, 2]);
    assert!(ws.flip(1, 1));
    assert_eq!(visible(&ws), vec![1, 3]);
    assert!(ws.unstack_focused());
    assert_eq!(ws.tiles.len(), 3);
    assert_eq!(visible(&ws), vec![1, 2, 3]);
}

#[test]
fn stacking_the_first_tile_goes_right() {
    let mut ws = ws(2);
    ws.focus = Focus::Tile(0);
    assert!(ws.stack_focused());
    assert_eq!(ws.tiles.len(), 1);
    assert_eq!(ws.tiles[0].stack, vec![2, 1]);
    assert_eq!(ws.focused(), Some(&1));
}

#[test]
fn float_and_back() {
    let mut ws = ws(2);
    ws.toggle_float(&1, AREA);
    assert_eq!(ws.tiles.len(), 1);
    assert_eq!(ws.floating.len(), 1);
    assert_eq!(ws.focused(), Some(&1));
    let p = ws.arrange(AREA, FULL, 10);
    // The floating window is last, so it is drawn on top.
    assert_eq!(p.last().unwrap().win, 1);
    assert_eq!(p[0].frame.w, 980);
    ws.toggle_float(&1, AREA);
    assert_eq!(ws.tiles.len(), 2);
    assert!(ws.floating.is_empty());
}

#[test]
fn removing_keeps_focus_valid() {
    let mut ws = ws(3);
    ws.focus = Focus::Tile(1);
    ws.remove(&2);
    assert_eq!(ws.focused(), Some(&3));
    ws.remove(&3);
    assert_eq!(ws.focused(), Some(&1));
    ws.remove(&1);
    assert_eq!(ws.focused(), None);
    assert!(ws.tiles.is_empty() && ws.floating.is_empty());
}

#[test]
fn removing_from_a_stack_keeps_the_shown_page() {
    let mut ws = ws(3);
    ws.stack_focused(); // tile 1 = [2, 3], 3 shown
    ws.remove(&2);
    assert_eq!(ws.tiles[1].stack, vec![3]);
    assert_eq!(ws.tiles[1].top, 0);
}

#[test]
fn focus_window_turns_the_stack_and_raises_floats() {
    let mut ws = ws(4);
    ws.stack_focused(); // tile 2 = [3, 4]
    ws.focus_window(&3);
    assert_eq!(ws.tiles[2].top, 0);
    ws.toggle_float(&1, AREA);
    ws.toggle_float(&2, AREA);
    ws.focus_window(&1);
    assert_eq!(ws.floating.last().unwrap().win, 1);
    assert_eq!(ws.focus, Focus::Float(1));
}

#[test]
fn drag_to_stack_beside_and_own_tile() {
    use crate::zones::{Drop, Side};
    let mut ws = ws(3);
    // Tile 0 onto tile 2's centre: tile 0 vanishes, the stack is the new tile 1.
    assert!(ws.move_window(&1, Drop::Stack(2)));
    assert_eq!(ws.tiles.len(), 2);
    assert_eq!(ws.tiles[1].stack, vec![3, 1]);
    assert_eq!(ws.focused(), Some(&1));
    // Out of the stack onto empty space at the left end.
    assert!(ws.move_window(&1, Drop::OwnTile(0)));
    assert_eq!(visible(&ws), vec![1, 2, 3]);
    // Beside: window 3 to the left of tile 0.
    assert!(ws.move_window(&3, Drop::Beside(0, Side::Left)));
    assert_eq!(visible(&ws), vec![3, 1, 2]);
    // A single window onto its own tile changes nothing.
    assert!(!ws.move_window(&3, Drop::Stack(0)));
    assert!(!ws.move_window(&3, Drop::None));
}

#[test]
fn gap_drag_moves_the_split_and_keeps_minimums() {
    let mut ws = ws(2);
    let before = ws.tile_frames(AREA, 10);
    assert!(ws.set_split(0, 300.0, AREA, 10, 120));
    let after = ws.tile_frames(AREA, 10);
    // The pointer sits in the middle of the gap: 300 - 10 (tile start) - 5 (half gap).
    assert_eq!(after[0].w, 285);
    assert_eq!(after[0].w + after[1].w, before[0].w + before[1].w);
    assert!(ws.set_split(0, 5.0, AREA, 10, 120));
    assert_eq!(ws.tile_frames(AREA, 10)[0].w, 120);
    assert!(!ws.set_split(1, 500.0, AREA, 10, 120), "no tile right of the last one");
}

#[test]
fn reorder_keeps_the_shown_window() {
    use crate::zones::Drop;
    let mut ws = ws(1);
    for w in [2, 3] {
        ws.add(w);
        ws.stack_focused();
    }
    assert_eq!(ws.tiles[0].stack, vec![1, 2, 3]);
    ws.focus_window(&1);
    assert!(ws.move_window(&1, Drop::Reorder { tile: 0, slot: 3, to: 2 }));
    assert_eq!(ws.tiles[0].stack, vec![2, 3, 1]);
    assert_eq!(ws.focused(), Some(&1));
    assert!(ws.move_window(&3, Drop::Reorder { tile: 0, slot: 0, to: 0 }));
    assert_eq!(ws.tiles[0].stack, vec![3, 2, 1]);
    assert_eq!(ws.focused(), Some(&1), "moving a hidden tab keeps the shown one");
    assert!(!ws.move_window(&3, Drop::Reorder { tile: 0, slot: 0, to: 5 }));
    assert!(!ws.move_window(&3, Drop::Reorder { tile: 1, slot: 0, to: 0 }));
}

const FULL: Rect = Rect { x: 0, y: 0, w: 1600, h: 900 };

#[test]
fn new_floats_open_centred_over_the_tiles_and_never_on_top_of_each_other() {
    let mut ws = ws(3);
    let area = Rect { x: 212, y: 90, w: 1000, h: 500 };
    ws.toggle_float(&1, area);
    let r = ws.floating[0].rect;
    assert_eq!((r.w, r.h), (600, 300));
    assert_eq!((r.x + r.w / 2, r.y + r.h / 2), (area.x + area.w / 2, area.y + area.h / 2), "centred over the tiles");
    ws.toggle_float(&2, area);
    assert_eq!(ws.floating[1].rect, Rect { x: r.x + 24, y: r.y + 24, ..r }, "the second one does not hide the first");
    // Once the first moved away, the centre is free again.
    ws.floating[0].rect.x = 0;
    ws.toggle_float(&3, area);
    assert_eq!(ws.floating[2].rect, r);
}

#[test]
fn the_focused_float_is_drawn_on_top_and_a_click_raises() {
    let mut ws = ws(4);
    for w in [1, 2, 3] {
        ws.toggle_float(&w, AREA);
    }
    let floats = |ws: &Workspace<u32>| ws.arrange(AREA, FULL, 10).into_iter().filter(|p| matches!(p.place, Place::Floating { .. })).map(|p| p.win).collect::<Vec<_>>();
    assert_eq!(floats(&ws), vec![1, 2, 3], "bottom first, the focused (last floated) on top");
    // Keys move the focus without turning the order, but the focused one is drawn on top.
    ws.focus = Focus::Float(0);
    assert_eq!(floats(&ws), vec![2, 3, 1]);
    // A full cycle of the focus keys visits every window once.
    let mut seen = Vec::new();
    for _ in 0..4 {
        ws.focus_step(1);
        seen.push(*ws.focused().unwrap());
    }
    seen.sort();
    assert_eq!(seen, vec![1, 2, 3, 4]);
    // A click (focus_window) raises for good.
    ws.focus_window(&2);
    assert_eq!(ws.floating.iter().map(|f| f.win).collect::<Vec<_>>(), vec![1, 3, 2]);
    assert_eq!(floats(&ws), vec![1, 3, 2]);
}

#[test]
fn a_float_keeps_48_px_of_its_header_on_the_screen() {
    let r = Rect { x: 100, y: 100, w: 400, h: 300 };
    let c = |x, y| clamp_float(Rect { x, y, ..r }, FULL, 38);
    assert_eq!(c(100, 100), r, "inside: unchanged");
    assert_eq!(c(-1000, 100).x, REACH - 400, "far left: its right 48 px stay");
    assert_eq!(c(5000, 100).x, 1600 - REACH, "far right: its left 48 px stay");
    assert_eq!(c(100, -50).y, 0, "the header never leaves at the top");
    assert_eq!(c(100, 5000).y, 900 - 38, "nor at the bottom");
    // Over the bars is fine: the whole screen is the float's.
    assert_eq!(c(0, 0), Rect { x: 0, y: 0, ..r });
    assert_eq!(clamp_float(r, FULL, 38).w, 400, "never resized");
    // Resizing: the top edge stops at the screen, the header stays reachable.
    let up = clamp_resized(Rect { y: -40, h: 340, ..r }, FULL, 38);
    assert_eq!((up.y, up.h), (0, 300));
    let left = clamp_resized(Rect { x: 1580, w: 300, ..r }, FULL, 38);
    assert_eq!((left.x, left.x + left.w), (1600 - REACH, 1880));
    let narrow = clamp_resized(Rect { x: -500, w: 520, ..r }, FULL, 38);
    assert_eq!(narrow.x + narrow.w, REACH);
}

#[test]
fn odd_sizes_never_panic_and_keep_the_header_on_a_real_screen() {
    let big = i32::MAX / 2;
    let sizes = [i32::MIN, -big, -1000, -1, 0, 1, 37, 47, 48, 49, 1600, 5000, big, i32::MAX];
    let places = [i32::MIN, -big, -5000, -1, 0, 1, 899, 1599, 5000, big, i32::MAX];
    let screens = [FULL, Rect { x: 0, y: 0, w: 30, h: 20 }, Rect { x: 0, y: 0, w: 0, h: 0 }, Rect { x: -100, y: 50, w: -10, h: -5 }];
    for &full in &screens {
        let real = full.w > 0 && full.h > 0;
        for header in [-1, 0, 37, 38, 2000] {
            for &w in &sizes {
                for &x in &places {
                    let r = Rect { x, y: x, w, h: w };
                    let (c, z) = (clamp_float(r, full, header), clamp_resized(r, full, header));
                    if !real || !(1..=full.h).contains(&header) || w <= 0 {
                        continue;
                    }
                    for (what, f) in [("moved", c), ("resized", z)] {
                        assert!(f.y >= full.y && f.y + header <= full.y + full.h, "{what} {r:?} on {full:?}: {f:?}");
                        let seen = (f.x as i64 + f.w as i64).min((full.x + full.w) as i64) - (f.x as i64).max(full.x as i64);
                        let want = (REACH as i64).min(f.w as i64).min(full.w as i64);
                        assert!(seen >= want, "{what} {r:?} on {full:?}: only {seen} px of its header on the screen");
                    }
                }
            }
        }
    }
}

#[test]
fn maximise_covers_the_screen_and_restores_the_old_place() {
    let mut ws = ws(2);
    ws.toggle_float(&1, AREA);
    let old = ws.floating[0].rect;
    assert!(!ws.toggle_max(&2, FULL), "a tile is not maximised by the layout");
    assert!(ws.toggle_max(&1, FULL));
    assert!(ws.is_max(&1));
    assert_eq!(ws.floating[0].rect, FULL);
    assert_eq!(ws.arrange(AREA, FULL, 10).last().unwrap().frame, FULL, "over everything, the whole screen");
    // A new screen size: a maximised window follows, the others are clamped.
    let big = Rect { w: 2048, h: 1152, ..FULL };
    ws.fit_floats(big, 38);
    assert_eq!(ws.floating[0].rect, big);
    assert!(ws.toggle_max(&1, big));
    assert!(!ws.is_max(&1));
    assert_eq!(ws.floating[0].rect, old, "back to its old place");
    // Unfloating a maximised window forgets it.
    ws.toggle_max(&1, FULL);
    ws.toggle_float(&1, AREA);
    assert!(ws.floating.is_empty() && !ws.is_max(&1));
}

#[test]
fn a_dragged_maximised_window_comes_back_under_the_pointer() {
    let old = Rect { x: 300, y: 200, w: 400, h: 300 };
    // Grabbed at three quarters of the screen's width, 10 px into the header.
    let r = restored_at(old, FULL, 1200.0, 10.0, 10.0);
    assert_eq!((r.w, r.h), (400, 300));
    assert_eq!((r.x, r.y), (1200 - 300, 0), "the pointer holds it at three quarters of its width");
    assert!(r.contains(1200.0, 10.0));
}

#[test]
fn fullscreen_covers_everything_and_restores_tile_and_float_exactly() {
    let mut ws = ws(3);
    ws.grow_focused(0.5);
    ws.toggle_float(&3, AREA);
    ws.toggle_max(&3, FULL);
    // A tile goes fullscreen: it alone is shown, over everything, with the screen as its frame.
    assert!(!ws.set_fullscreen(&1, true), "not focused: refused");
    ws.focus_window(&1);
    let before: Vec<_> = ws.arrange(AREA, FULL, 10).into_iter().map(|p| (p.win, p.frame, p.visible)).collect();
    assert!(ws.set_fullscreen(&1, true));
    assert!(!ws.set_fullscreen(&1, true), "asked twice: no change");
    let p = ws.arrange(AREA, FULL, 10);
    let last = p.last().unwrap();
    assert!(last.win == 1 && last.fullscreen && last.visible && last.frame == FULL);
    assert_eq!(visible(&ws), vec![1], "everything else is hidden under it");
    assert!(!p.iter().any(|q| q.win != 1 && q.fullscreen));
    // Leaving gives back every frame, weight and the maximised float as they were.
    assert!(ws.set_fullscreen(&1, false));
    assert!(!ws.set_fullscreen(&1, false));
    let after: Vec<_> = ws.arrange(AREA, FULL, 10).into_iter().map(|p| (p.win, p.frame, p.visible)).collect();
    let mut b = before.clone();
    b.sort_by_key(|x| x.0);
    let mut a = after;
    a.sort_by_key(|x| x.0);
    assert_eq!(a, b);
    // The maximised float goes fullscreen and comes back maximised.
    ws.focus_window(&3);
    assert!(ws.set_fullscreen(&3, true));
    assert_eq!(ws.arrange(AREA, FULL, 10).last().unwrap().frame, FULL);
    ws.set_fullscreen(&3, false);
    assert!(ws.is_max(&3) && ws.floating[0].restore.is_some());
}

#[test]
fn fullscreen_ends_when_the_focus_moves_or_the_window_goes() {
    let mut ws = ws(2);
    ws.focus_window(&1);
    ws.set_fullscreen(&1, true);
    assert_eq!(ws.fullscreen_shown(), Some(&1));
    assert_eq!(ws.settle_fullscreen(), None, "still focused: stays");
    ws.focus_window(&2);
    assert_eq!(ws.fullscreen_shown(), None, "not shown once the focus left");
    assert_eq!(visible(&ws), vec![1, 2]);
    assert_eq!(ws.settle_fullscreen(), Some(1));
    assert!(ws.fullscreen.is_none());
    // A second window taking over; then it closes.
    ws.focus_window(&1);
    ws.set_fullscreen(&1, true);
    ws.focus_window(&2);
    ws.set_fullscreen(&2, true);
    assert_eq!(ws.fullscreen, Some(2));
    ws.remove(&2);
    assert!(ws.fullscreen.is_none());
    assert!(!ws.set_fullscreen(&9, true), "a window of another workspace");
}

#[test]
fn a_window_without_the_focus_cannot_take_the_screen() {
    let mut ws = ws(2);
    ws.focus_window(&2);
    ws.stack_focused();
    ws.focus_window(&1);
    assert_eq!(visible(&ws), vec![1]);
    assert!(!ws.set_fullscreen(&2, true), "a hidden stack page asking is refused");
    assert_eq!(visible(&ws), vec![1]);
    assert_eq!(ws.focused(), Some(&1));
    assert_eq!(ws.fullscreen, None);
}

#[test]
fn a_window_opened_under_a_fullscreen_one_stays_behind() {
    let mut ws = ws(2);
    ws.focus_window(&1);
    assert!(ws.set_fullscreen(&1, true));
    ws.add_behind(3);
    assert_eq!(ws.focused(), Some(&1), "the fullscreen window keeps the focus");
    assert_eq!(ws.settle_fullscreen(), None, "and its fullscreen");
    assert_eq!(visible(&ws), vec![1]);
    assert!(ws.find(&3).is_some());
    // Without a fullscreen the new window takes the focus as before.
    ws.set_fullscreen(&1, false);
    ws.add_behind(4);
    assert_eq!(ws.focused(), Some(&4));
}
