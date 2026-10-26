use super::*;
use crate::page_schema::{Spec, Ty, builtin, model};

/// The built-in rows plus a few keys of other kinds (a long choice, a number, a colour, a text, a secret).
pub fn test_model() -> Model {
    let mut s = builtin();
    let more = |key: &str, ty: Ty, safety: &str| Spec { key: key.into(), ty, safety: safety.into(), ..builtin()[0].clone() };
    let ch = |v: &[&str]| Ty::Choice(v.iter().map(|x| x.to_string()).collect());
    s.push(more("kernel.console_log", ch(&["off", "error", "warn", "info", "debug", "all"]), "normal"));
    s.push(more("kernel.snd_output", ch(&["auto", "jack", "hdmi", "usb"]), "normal"));
    s.push(more("theme.glow", Ty::Int { min: 0, max: 100, unit: "%".into(), word: None }, "normal"));
    s.push(more("theme.dark.bg", Ty::Color, "normal"));
    s.push(more("claude.name", Ty::Text { max: 64 }, "normal"));
    s.push(more("net.wifi.psk", Ty::Text { max: 64 }, "secret"));
    model(&s)
}

fn row(m: &Model, key: &str) -> usize {
    m.rows.iter().position(|r| r.key == key).unwrap()
}

#[test]
fn page_addresses_name_the_keys_both_ways() {
    assert_eq!(prefix_of("aussehen/akzent"), "theme.accent");
    assert_eq!(prefix_of("aussehen"), "theme");
    assert_eq!(prefix_of("aussehen/accent"), "theme.accent");
    assert_eq!(prefix_of("aufbau/statusleiste"), "theme.layout.statusbar");
    assert_eq!(prefix_of("favoriten"), FAVOURITES);
    assert_eq!(prefix_of("theme.layout"), "theme.layout");
    assert_eq!(prefix_of("kernel/snd_output"), "kernel.snd_output");
    assert_eq!(prefix_of(""), "");
    for r in &test_model().rows {
        assert_eq!(prefix_of(address_of_key(&r.key).strip_prefix("einstellungen/").unwrap()), r.key, "{}", r.key);
    }
    assert_eq!(address_of_key("theme.accent"), "einstellungen/aussehen/akzent");
    assert_eq!(address_of_key("net.wifi.ssid"), "einstellungen/net/wifi/ssid");
}

#[test]
fn controls_fit_and_hit_at_both_widths() {
    let m = test_model();
    for c in [Rect { x: 220, y: 140, w: 1500, h: 4000 }, Rect { x: 220, y: 140, w: 520, h: 6000 }] {
        let l = layout(c, &m, &Inputs { favourites: 2, scroll: 0 });
        assert_eq!(l.narrow, c.w < 700);
        for (p, b) in &l.parts {
            assert!(b.x >= c.x && b.x + b.w <= c.x + c.w, "{p:?} {b:?} in {c:?}");
            let (mx, my) = ((b.x + b.w / 2) as f64, (b.y + b.h / 2) as f64);
            assert_eq!(l.hit(mx, my), Some(*p), "{p:?}");
            if let Part::Choice(r, _) | Part::Less(r) | Part::More(r) | Part::Field(r) | Part::Switch(r) = p {
                let rb = l.rows[*r];
                assert!(b.y >= rb.y && b.y + b.h <= rb.y + rb.h, "{p:?} inside its row {rb:?}");
            }
        }
        let names: Vec<String> = l.parts.iter().map(|(p, _)| part_name(&m.rows, *p)).collect();
        for want in [
            "undo",
            "swatch=#ffb020",
            "hex",
            "choice=theme.mode=light",
            "switch=theme.layout.statusbar",
            "fav-remove=1",
            "fav-up=1",
            "more=theme.scale",
            "field=theme.scale",
            "less=theme.glow",
            "choice=kernel.snd_output=hdmi",
            "choice=kernel.console_log=all",
            "field=theme.dark.bg",
            "field=claude.name",
        ] {
            assert!(names.iter().any(|n| n == want), "{want} in {names:?}");
        }
        assert!(!names.iter().any(|n| n == "fav-up=0" || n.ends_with("net.wifi.psk")), "no control for a secret");
        assert_eq!(l.hit((c.x + 2) as f64, (c.y + 2) as f64), Some(Part::Body));
        assert_eq!(l.hit((c.x - 2) as f64, (c.y + 2) as f64), None);
        let heads: Vec<&str> = l.heads.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(heads[..3], ["AUSSEHEN", "AUFBAU", "FAVORITEN"]);
        // Six choices do not fit beside the label: they go under it (and wrap when narrow).
        let log = row(&m, "kernel.console_log");
        assert!(l.rows[log].h >= ROW_H_NARROW);
        assert_eq!(l.rows[row(&m, "theme.mode")].h, if l.narrow { ROW_H_NARROW } else { ROW_H });
    }
}

#[test]
fn the_hand_made_rows_keep_their_place() {
    // The mock-up's rows lie where they did before WY-9 (the references of 77-settings.sh show them).
    let c = Rect { x: 0, y: 0, w: 1300, h: 900 };
    let m = test_model();
    let l = layout(c, &m, &Inputs { favourites: 2, scroll: 0 });
    let (pad, top) = (40, HEAD_AREA + 16 + HEAD_H + 10);
    for (i, k) in ["theme.mode", "theme.accent", "theme.scale", "theme.motion", "theme.effects"].iter().enumerate() {
        assert_eq!(l.rows[row(&m, k)], Rect { x: pad, y: top + i as i32 * ROW_H, w: 920, h: ROW_H }, "{k}");
    }
    let more = l.part(Part::More(row(&m, "theme.scale"))).unwrap();
    assert_eq!((more.x, more.w), (pad + 920 - 20 - CTL_H, CTL_H), "the + is right-aligned as before");
}

#[test]
fn scrolling_hides_parts_above_and_finds_rows() {
    let c = Rect { x: 0, y: 100, w: 1200, h: 400 };
    let m = test_model();
    let l0 = layout(c, &m, &Inputs { favourites: 2, scroll: 0 });
    assert!(l0.max_scroll() > 0, "a 400 px page scrolls");
    let to = l0.scroll_to("theme.layout", &m.rows, 0).unwrap();
    assert!(to > 0);
    let l = layout(c, &m, &Inputs { favourites: 2, scroll: to });
    assert_eq!(l.rows[row(&m, "theme.layout.address_bar")].y, c.y + HEAD_AREA + 16, "the first layout row is right below the head");
    assert!(l.part(Part::Undo).is_some(), "Rückgängig stays in the head");
    assert!(l.part(Part::Swatch(0)).is_none(), "the swatches scrolled under the head");
    assert!(l.scrolled && !l0.scrolled);
    assert_eq!(l0.scroll_to("theme.mode", &m.rows, 0), Some(0), "a row in view: no scroll");
    assert_eq!(l0.row_top("theme.accent", &m.rows, 0), Some(l0.rows[1].y - c.y));
    assert!(l0.row_top("nothing.here", &m.rows, 0).is_none());
    let snd = row(&m, "kernel.snd_output");
    let to = l0.scroll_to("kernel.snd_output", &m.rows, 0).unwrap();
    let l = layout(c, &m, &Inputs { favourites: 2, scroll: to });
    assert!(l.part(Part::Choice(snd, 2)).is_some(), "the jump shows the row's controls");
    assert_eq!(part_name(&m.rows, Part::Choice(snd, 2)), "choice=kernel.snd_output=hdmi");
    assert_eq!(part_name(&m.rows, Part::Choice(999, 0)), "choice", "a stale part names nothing");
}

#[test]
fn values_from_the_controls() {
    let s = |n, up| number_step(n, up, 100, 300, SCALE_STEP);
    assert_eq!((s(125, true), s(125, false), s(130, false), s(130, true), s(100, false), s(300, true)), (150, 100, 125, 150, 100, 300));
    assert_eq!(number_step(-3, true, -10, 10, 5), 0);
    assert_eq!(number_step(i64::MAX, true, 0, i64::MAX, 7), i64::MAX, "no overflow");
    assert_eq!(number("42", 0, 100, "%"), Ok(42));
    assert_eq!(number(" 42 % ", 0, 100, "%"), Ok(42));
    assert!(number("101", 0, 100, "%").unwrap_err().contains("von 0 bis 100 %"));
    assert!(number("zwölf", 0, 9, "").is_err());
    assert_eq!(colour("3DDC97").as_deref(), Some("#3ddc97"));
    assert_eq!(colour(" #ffb020 ").as_deref(), Some("#ffb020"));
    for bad in ["", "#12345", "#gggggg", "#1234567", "nonsense"] {
        assert_eq!(colour(bad), None, "{bad}");
    }
    let v: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
    assert_eq!(favs_without(&v, 1), ["a", "c"]);
    assert_eq!(favs_up(&v, 2), ["a", "c", "b"]);
    assert_eq!(favs_up(&v, 0), v);
    assert_eq!(favs_with(&v, " d,e "), Some(vec!["a".into(), "b".into(), "c".into(), "d e".into()]));
    assert_eq!(favs_with(&v, "b"), None);
    assert_eq!(favs_with(&v, "  "), None);
}
