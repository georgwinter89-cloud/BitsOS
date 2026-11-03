use super::*;
use crate::chrome::Part as C;
use crate::settings::Values;

fn theme(pairs: &[(&str, &str)]) -> Theme {
    let mut v = Values::default();
    for (k, val) in pairs {
        v.set(k, val);
    }
    Theme::build(&v)
}

fn inputs(size: (i32, i32), n: usize) -> Inputs {
    Inputs {
        size,
        names: (1..=n).map(crate::desk::name).collect(),
        active: 0,
        min_labels: Vec::new(),
        absicht: false,
    }
}

fn inside(a: Rect, b: Rect) -> bool {
    a.x >= b.x && a.y >= b.y && a.x + a.w <= b.x + b.w && a.y + a.h <= b.y + b.h
}

fn apart(a: Rect, b: Rect) -> bool {
    a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y
}

fn mid(r: Rect) -> (f64, f64) {
    (r.x as f64 + r.w as f64 / 2.0, r.y as f64 + r.h as f64 / 2.0)
}

#[test]
fn the_buehne_is_the_default_and_classic_stays_a_preset() {
    use crate::theme::Preset;
    assert_eq!(theme(&[]).layout.preset, Preset::Buehne);
    assert_eq!(theme(&[("theme.layout.preset", "classic")]).layout.preset, Preset::Classic);
    assert_eq!(theme(&[("theme.layout.preset", "Klassisch")]).layout.preset, Preset::Classic);
    assert_eq!(theme(&[("theme.layout.preset", "irgendwas")]).layout.preset, Preset::Buehne);
    // Classic keeps today's tile metrics exactly; the Bühne spaces its blocks as the boards (24 px, radius 28).
    let (c, bu) = (theme(&[("theme.layout.preset", "classic")]), theme(&[]));
    assert_eq!((c.gap, c.radius), (12, 12));
    assert_eq!((bu.gap, bu.radius), (bu.bl(24.0), bu.bl(28.0)));
    assert_eq!((bu.gap, bu.radius), (26, 30));
}

#[test]
fn mini_pc_2560x1440_is_the_board_times_four_thirds() {
    let t = theme(&[]);
    assert_eq!(t.scale, 1.25);
    let l = layout(&t, &inputs((2048, 1152), 3));
    let k = |px: i32| t.bl(px as f32);
    assert_eq!(l.header, Rect { x: 0, y: 0, w: 2048, h: k(56) });
    // The stage at (48, 80) 1360x720 and the right column at x 1440, 432 wide, as on the Main board.
    assert_eq!((l.stage.x, l.stage.y), (k(48), k(80)));
    assert!((l.stage.w - k(1360)).abs() <= 1 && (l.stage.h - k(720)).abs() <= 1, "{:?}", l.stage);
    let (map, next) = l.column.expect("the column");
    assert_eq!((map.x, map.y, map.w, map.h), (2048 - k(48) - k(432), k(80), k(432), k(316)));
    assert!((next.y - k(416)).abs() <= 1 && (next.y + next.h - k(668)).abs() <= 1, "{next:?}");
    let strip = l.strip.unwrap();
    assert!((strip.y - k(824)).abs() <= 1 && strip.h == k(128) && strip.x == k(48), "{strip:?}");
    let row = l.bottom.unwrap();
    assert!((row.y - k(980)).abs() <= 1 && row.h == k(64));
    assert!((l.feld.x - k(480)).abs() <= 1 && (l.feld.w - k(960)).abs() <= 1, "{:?}", l.feld);
    // Without a headline the tiles take the whole stage (the gap keeps them off its edge).
    assert_eq!(l.work, l.stage);
    // The header's parts in a row, inside it, none overlapping.
    let mut row = vec![l.mark];
    row.extend(&l.chips[..l.shown]);
    row.extend([l.ws_new, l.address, l.blocks, l.ring, l.sun, l.bell, l.clock]);
    for w in row.windows(2) {
        assert!(w[0].x + w[0].w <= w[1].x, "{:?} before {:?}", w[0], w[1]);
    }
    assert!(row.iter().all(|r| inside(*r, l.header)));
    assert_eq!(l.shown, 3);
    assert!(l.more.is_none());
    for r in [l.stage, map, next, strip, l.feld, l.hint] {
        assert!(apart(r, l.header));
    }
    assert!(apart(l.stage, map) && apart(l.stage, next) && apart(l.stage, strip) && apart(strip, l.feld));
}

#[test]
fn the_headline_takes_its_room_from_the_tiles() {
    let t = theme(&[]);
    let mut i = inputs((2048, 1152), 2);
    i.absicht = true;
    let l = layout(&t, &i);
    let hl = l.headline.expect("headline");
    assert!(inside(hl, l.stage) && apart(hl, l.work));
    assert_eq!(l.work.y, hl.y + hl.h);
    assert_eq!(l.work.y + l.work.h, l.stage.y + l.stage.h);
}

#[test]
fn hits_reach_wy_parts_and_the_buehne_parts() {
    let t = theme(&[]);
    let mut i = inputs((2048, 1152), 3);
    i.min_labels = vec!["foot".into(), "Brave".into()];
    let l = layout(&t, &i);
    let c = l.chrome();
    for k in 0..3 {
        let (x, y) = mid(l.chips[k]);
        assert_eq!(c.hit(x, y), Some(C::Ws(k)));
    }
    assert_eq!(c.ws_shown(), 3);
    let (x, y) = mid(l.ws_new);
    assert_eq!(c.hit(x, y), Some(C::WsNew));
    let (x, y) = mid(l.ring);
    assert_eq!(c.hit(x, y), Some(C::Ring));
    let (x, y) = mid(l.home.unwrap());
    assert_eq!(c.hit(x, y), Some(C::Home));
    assert_eq!(l.min_items.len(), 2);
    for (k, r) in l.min_items.iter().enumerate() {
        let (x, y) = mid(*r);
        assert_eq!(c.hit(x, y), Some(C::Min(k)));
    }
    // The context address is read-only: no click starts editing there; the suggestions open under it.
    let (x, y) = mid(l.address);
    assert_eq!(c.hit(x, y), None);
    assert_eq!((c.address.y, c.address.h), (l.header.h, 0));
    assert_eq!(c.address, l.suggest);
    assert!(c.address.w >= t.bl(560.0) && c.address.x + c.address.w <= 2048);
    for (r, p) in [(l.sun, Part::Sun), (l.bell, Part::Bell), (l.feld, Part::Feld), (l.strip.unwrap(), Part::Strip)] {
        let (x, y) = mid(r);
        assert_eq!(l.hit(x, y), Some(p));
        assert_eq!(c.hit(x, y), None, "{p:?} is no WY part");
        assert!(c.covers(x, y), "{p:?} is chrome: no click reaches a tile through it");
    }
    // The tiles' area is untouched by every part, and nothing folds over it.
    let (x, y) = mid(l.work);
    assert_eq!((c.hit(x, y), l.hit(x, y)), (None, None));
    assert!(!c.covers(x, y));
    assert_eq!(c.overlay(), None);
    for (x, y) in [(0.0, 500.0), (2047.0, 500.0)] {
        assert_eq!(c.edge_at(x, y), None);
    }
    assert!(c.side_docked && c.list_docked && c.list_shown && c.favs.is_none());
}

#[test]
fn many_workspaces_fold_into_plus_n() {
    let t = theme(&[]);
    let l = layout(&t, &inputs((2048, 1152), 40));
    assert!(l.shown > 2 && l.shown < 40, "{}", l.shown);
    let more = l.more.expect("+N");
    let c = l.chrome();
    assert_eq!(c.ws_shown(), l.shown);
    assert_eq!(c.ws_items.len(), 40);
    let (x, y) = mid(more);
    assert_eq!(l.hit(x, y), Some(Part::More));
    assert!(more.x + more.w <= l.ws_new.x && l.ws_new.x + l.ws_new.w <= l.address.x);
}

#[test]
fn a_folded_column_or_bottom_gives_the_stage_the_room() {
    let t = theme(&[("theme.layout.claude_sidebar", "false"), ("theme.layout.statusbar", "false")]);
    let l = layout(&t, &inputs((2048, 1152), 2));
    assert!(l.column.is_none() && l.strip.is_none() && l.bottom.is_none() && l.home.is_none());
    assert_eq!(l.stage.x + l.stage.w, 2048 - t.bl(48.0));
    assert_eq!(l.stage.y + l.stage.h, 1152 - t.bl(36.0));
    let c = l.chrome();
    assert!(c.side.is_none() && !c.side_docked && c.status.is_none());
    assert_eq!(c.ws_shown(), 2);
    assert_eq!(c.overlay(), None);
    assert_eq!(c.edge_at(2047.0, 500.0), None, "a folded column is not revealed over the tiles");
}

#[test]
fn small_and_odd_outputs_lay_out_without_overlap_or_panic() {
    let t = theme(&[("theme.scale", "100")]);
    let l = layout(&t, &inputs((1280, 800), 4));
    assert!(l.column.is_none(), "1280 is too narrow for the column");
    assert!(l.work.w > 600 && l.work.h > 200, "{:?}", l.work);
    assert!(apart(l.stage, l.bottom.unwrap()) && apart(l.feld, l.hint));
    // 1600x900 at 125 % (1280x720 logical): the empty strip gives its room to the tiles, the bottom row stays.
    let l = layout(&theme(&[]), &inputs((1280, 720), 2));
    assert!(l.strip.is_none() && l.bottom.is_some(), "{:?}", l.strip);
    assert!(l.work.h > 450 && apart(l.stage, l.bottom.unwrap()), "{:?}", l.work);
    for size in [(1, 1), (0, 0), (300, 200), (640, 480), (5000, 3000), (2048, 300)] {
        let l = layout(&t, &inputs(size, 3));
        let c = l.chrome();
        assert!(l.work.w >= 1 && l.work.h >= 1);
        let _ = (c.hit(10.0, 10.0), l.hit(10.0, 10.0), c.ws_shown());
    }
}

#[test]
fn slugs_keys_colours_and_the_hint_line() {
    assert_eq!(slug("Steuer 2026"), "steuer-2026");
    assert_eq!(slug("Arbeitsraum 3"), "arbeitsraum-3");
    assert_eq!(slug("  Grüße & Maß!  "), "gruesse-mass");
    assert_eq!(slug("###"), "raum");
    let names: Vec<String> = ["Arbeitsraum 1", "Büro", "Buero", "Ölfeld Ä 7", "Büro"].iter().map(|s| s.to_string()).collect();
    assert_eq!(absicht_key("Arbeitsraum 1", &names), "desktop.absicht.arbeitsraum-1");
    // Two names with one slug get two keys; the very same name shares its key.
    let (a, b) = (absicht_key("Büro", &names), absicht_key("Buero", &names));
    assert!(a != b && a.starts_with("desktop.absicht.buero-") && b.starts_with("desktop.absicht.buero-"), "{a} {b}");
    assert_eq!(absicht_key("Büro", &names[..2]), "desktop.absicht.buero", "no collision: the plain key");
    // A valid settingsd key name (segments of a-z 0-9 _ -).
    let k = absicht_key("Ölfeld Ä 7", &names);
    for k in [k, a, b] {
        assert!(k.split('.').all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')), "{k}");
    }
    assert_eq!((colour_of("Arbeitsraum 1"), colour_of("Arbeitsraum 2"), colour_of("Arbeitsraum 3")), (0, 1, 2));
    assert_eq!(colour_of("Arbeitsraum 4"), 0);
    assert!(colour_of("Urlaub") < 3);
    assert_eq!(hint_text(0, 0), "0 Teile auf der Bühne");
    assert_eq!(hint_text(1, 0), "1 Teil auf der Bühne");
    assert_eq!(hint_text(4, 1), "4 Teile auf der Bühne · 1 Programm hängt");
    assert_eq!(hint_text(2, 3), "2 Teile auf der Bühne · 3 Programme hängen");
}
