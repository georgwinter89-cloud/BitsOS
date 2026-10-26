use super::*;

fn t() -> &'static Theme {
    crate::theme::test_theme()
}
const F: Rect = Rect { x: 100, y: 50, w: 1000, h: 300 };

fn room(f: Rect) -> (i32, i32) {
    super::room(f, t())
}

#[test]
fn tabs_share_the_room_evenly_up_to_the_maximum() {
    let s = strip(F, 1, 0, t());
    assert_eq!(s.tabs.len(), 1);
    assert_eq!(s.tabs[0].w, t().tab_max);
    assert_eq!(s.tabs[0].x, room(F).0);
    let s = strip(F, 5, 2, t());
    let (x0, x1) = room(F);
    let w = (x1 - x0 - 4 * t().tab_gap) / 5;
    assert!(w < t().tab_max && w >= t().tab_min);
    assert!(s.tabs.iter().all(|r| r.w == w), "even split");
    assert!(s.tabs.windows(2).all(|p| p[1].x == p[0].x + w + t().tab_gap));
    assert!(s.tabs[4].x + s.tabs[4].w <= x1, "tabs stay left of the buttons");
    assert_eq!(s.more, None);
    // The shown tab reaches down to the content, the strip starts below the header's top.
    assert_eq!(s.tabs[0].y + s.tabs[0].h, F.y + t().header_height);
}

#[test]
fn too_many_tabs_shrink_to_the_minimum_then_overflow() {
    let (x0, x1) = room(F);
    let fit = ((x1 - x0 - t().more_width - t().tab_gap + t().tab_gap) / (t().tab_min + t().tab_gap)) as usize;
    let n = fit + 4;
    let s = strip(F, n, 0, t());
    assert_eq!(s.tabs.len(), fit);
    assert!(s.tabs.iter().all(|r| r.w >= t().tab_min && r.w == s.tabs[0].w));
    assert!((fit as i32 + 1) * (t().tab_min + t().tab_gap) > x1 - x0 - t().more_width, "one more would not fit");
    let (more, hidden) = s.more.unwrap();
    assert_eq!(hidden, 4);
    assert!(more.x > s.tabs[fit - 1].x + s.tabs[fit - 1].w, "the chip follows the last tab");
    assert!(more.x + more.w <= x1);
    // The shown tab is always drawn, as the last one when it lies beyond.
    let s = strip(F, n, n - 1, t());
    assert_eq!(s.first, n - fit);
    assert!(s.tab(n - 1).is_some());
    assert!(s.tab(0).is_none());
    let s = strip(F, n, 2, t());
    assert_eq!(s.first, 0);
}

#[test]
fn narrow_headers_keep_one_tab() {
    let narrow = Rect { w: 120, ..F };
    let s = strip(narrow, 1, 0, t());
    assert_eq!(s.tabs.len(), 1);
    assert!(s.tabs[0].w > 0 && s.tabs[0].w < t().tab_min);
    let s = strip(narrow, 3, 1, t());
    assert_eq!(s.tabs.len(), 1);
    assert_eq!(s.first, 1);
    assert_eq!(s.more.map(|m| m.1), Some(2));
}

#[test]
fn close_button_and_text_room_inside_the_tab() {
    let tab = strip(F, 2, 0, t()).tabs[0];
    let x = close_rect(tab, t());
    assert_eq!(x.x + x.w + t().tab_inner, tab.x + tab.w);
    assert!(x.y >= tab.y && x.y + x.h <= tab.y + tab.h);
    let (a, b) = text_room(tab, t());
    assert_eq!(a, tab.x + t().tab_inner + t().tab_dot + 8, "the text starts after the dot");
    assert!(b < x.x);
}

#[test]
fn labels_fall_back() {
    assert_eq!(label("Grüße", "foot"), "Grüße");
    assert_eq!(label("", "foot"), "foot");
    assert_eq!(label(" ", ""), "Fenster");
}

#[test]
fn insert_slots_and_marker() {
    let s = strip(F, 3, 0, t());
    let c = |i: usize| s.tabs[i].x as f64 + s.tabs[i].w as f64 / 2.0;
    assert_eq!(slot_at(&s, s.tabs[0].x as f64), 0);
    assert_eq!(slot_at(&s, c(0) + 1.0), 1);
    assert_eq!(slot_at(&s, c(2) + 1.0), 3);
    let bar = slot_bar(&s, 1, t()).unwrap();
    assert!(bar.x <= s.tabs[1].x && bar.x + bar.w >= s.tabs[0].x + s.tabs[0].w);
    let end = slot_bar(&s, 3, t()).unwrap();
    assert!(end.x >= s.tabs[2].x + s.tabs[2].w - 1);
}
