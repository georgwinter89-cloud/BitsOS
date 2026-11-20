use super::*;
use crate::page_schema::{Spec, Ty, row_of};

/// A fake settingsd list: the keys the sofa rows show, as settingsd's `describe` gives them.
fn fake_rows() -> Vec<Row> {
    let spec = |key: &str, ty: Ty, default: &str, safety: &str| Spec {
        key: key.into(),
        ty,
        default: Some(default.into()),
        de: key.into(),
        names: None,
        restart: "none".into(),
        safety: safety.into(),
    };
    let choice = |v: &[&str]| Ty::Choice(v.iter().map(|s| s.to_string()).collect());
    vec![
        spec("kernel.snd_output", choice(&["auto", "jack", "hdmi", "usb"]), "auto", "normal"),
        spec("net.wifi.ssid", Ty::Text { max: 32 }, "", "normal"),
        spec("net.wifi.psk", Ty::Text { max: 63 }, "", "secret"),
        spec("sofa.motion", Ty::Bool, "true", "confirm"),
    ]
    .iter()
    .map(row_of)
    .collect()
}

fn input<'a>(v: &'a Values, rows: &'a [Row]) -> Input<'a> {
    Input { values: v, rows, screen: (2560, 1440), link: Some("WLAN"), pads: vec![("DragonRise Inc. Generic USB Joystick".into(), "Tabelle")], date: "Mi 21.10.".into(), generation: Some(130) }
}

#[test]
fn the_rows_come_from_the_settings_list_and_the_sofa_keys() {
    let rows = fake_rows();
    let mut v = Values::default();
    v.set("kernel.snd_output", "hdmi");
    v.set("net.wifi.ssid", "Wohnzimmer");
    v.set("net.wifi.psk", "never shown");
    v.set("sofa.start_volume", "30");
    let i = input(&v, &rows);
    let bt = rows_of(0, &i);
    assert_eq!(bt.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(), ["Auflösung", "Bildausschnitt", "Lautstärke beim Start", "Tonausgabe"]);
    assert_eq!(bt[0].ctl, Ctl::Arrows { value: "2560 × 1440".into(), greyed: true });
    assert_eq!(bt[1].ctl, Ctl::Arrows { value: "100 %".into(), greyed: false }, "sofa.overscan from the sofa's own definitions (its default)");
    assert_eq!(bt[2].ctl, Ctl::Arrows { value: "30 %".into(), greyed: false });
    assert_eq!(bt[3].ctl, Ctl::Arrows { value: "HDMI".into(), greyed: false });
    let net = rows_of(2, &i);
    assert_eq!(net[0].ctl, Ctl::Value("Verbunden".into()));
    assert_eq!(net[1].ctl, Ctl::Value("Wohnzimmer".into()));
    assert_eq!(net[2].ctl, Ctl::Value(DOTS.into()), "a secret is never read back");
    assert!(!format!("{net:?}").contains("never shown"));
    let sofa = rows_of(3, &i);
    assert_eq!(sofa[0].ctl, Ctl::Switch(true));
    assert_eq!(sofa[1].ctl, Ctl::Value("An".into()), "a confirm key is read-only");
    assert!(sofa[1].line.contains("nur im Desktop"));
    assert_eq!(sofa[2].ctl, Ctl::Arrows { value: "Zuletzt gespielt".into(), greyed: false });
    let pads = rows_of(1, &i);
    assert_eq!(pads.len(), 3, "one pad, Tasten zuordnen, Home-Taste");
    assert_eq!((pads[0].line.as_str(), &pads[1].act), ("Belegung: Tabelle", &Act::Learn));
    let none = Input { pads: Vec::new(), ..input(&v, &rows) };
    assert_eq!(rows_of(1, &none)[0].title, "Kein Gamepad");
    assert!(rows_of(4, &i).is_empty(), "Konten is text only");
    assert_eq!(rows_of(5, &i).iter().map(|r| &r.act).collect::<Vec<_>>(), [&Act::Info, &Act::Desktop, &Act::Reboot, &Act::Off]);
    let empty: Vec<Row> = Vec::new();
    let bare = rows_of(0, &input(&v, &empty));
    assert_eq!(bare[3].ctl, Ctl::Value("–".into()), "a key settingsd never described is only shown");
}

fn rows_of(s: usize, i: &Input) -> Vec<SRow> {
    rows(s, i)
}

#[test]
fn arrows_step_numbers_and_choices_without_wrapping() {
    let rows = fake_rows();
    let vol = row_for("sofa.start_volume", &rows).unwrap();
    assert_eq!(change(&vol, "20", 1), Some(Json::Num(25.0)));
    assert_eq!(change(&vol, "22", -1), Some(Json::Num(20.0)));
    assert_eq!(change(&vol, "100", 1), None);
    assert_eq!(change(&vol, "", -1), Some(Json::Num(15.0)), "empty: from the default");
    let over = row_for("sofa.overscan", &rows).unwrap();
    assert_eq!(change(&over, "90", -1), None);
    assert_eq!(change(&over, "95", 1), Some(Json::Num(96.0)));
    let out = row_for("kernel.snd_output", &rows).unwrap();
    assert_eq!(change(&out, "auto", -1), None);
    assert_eq!(change(&out, "auto", 1), Some(Json::str("jack")));
    assert_eq!(change(&out, "usb", 1), None);
    let start = row_for("sofa.start_on_pad", &rows).unwrap();
    assert_eq!(change(&start, "true", -1), Some(Json::Bool(false)));
    assert_eq!(change(&start, "true", 1), None);
    assert_eq!(change(&row_for("sofa.motion", &rows).unwrap(), "true", -1), None, "confirm keys never change here");
}

#[test]
fn the_learn_screen_takes_each_code_once() {
    let mut l = Learn { pad: 1, got: Vec::new() };
    assert_eq!(l.wants().map(|(_, n)| n), Some("a"));
    assert!(l.press(0x122));
    assert!(!l.press(0x122), "taken");
    for c in [0x121, 0x123, 0x120, 0x124, 0x125, 0x128] {
        assert!(l.press(c));
    }
    assert_eq!(l.wants().map(|(_, n)| n), Some("start"));
    assert!(l.press(0x129));
    assert_eq!((l.wants(), l.got.len()), (None, 8));
    assert!(!l.press(0x12a), "done");
}

#[test]
fn the_quick_menu_and_einstellungen_draw_at_2560x1440_with_every_glyph() {
    use crate::sofa::Frame;
    use crate::sofa_draw::{Ink, font, measure};
    use crate::sofa_layout as lay;
    use crate::text::{Typeface, Weight};
    let ink = Ink::dark();
    let f = Frame::of(2560, 1440, 100);
    let (bw, bh) = f.board();
    let k = f.k as f32;
    let items = lay::quick(bw, bh, true, &["Fernseher (HDMI)"], &measure);
    let look = crate::sofa_quick_draw::QuickLook {
        frame: f,
        bw,
        bh,
        clock: "20:16".into(),
        running: Some("Dungeon Keeper 2".into()),
        volume: Some(30),
        volume_text: "30 % · HDMI".into(),
        outputs: vec![("Fernseher (HDMI)".into(), true)],
        tiles: vec![("WLAN", "Kabel · verbunden".into()), ("Gamepad", "Verbunden · Kabel".into()), ("Bildschirm", "2560 × 1440".into()), ("Leistung", "CPU 12 %".into())],
        note: None,
        confirm: None,
        items: items.clone(),
        focus: Some("q:resume".into()),
    };
    let panel = f.r(crate::layout::Rect { x: bw - lay::QUICK_W, y: 0, w: lay::QUICK_W, h: bh });
    let o = crate::layout::Rect { w: 2560 - panel.x, y: 0, h: 1440, ..panel };
    let c = crate::sofa_quick_draw::draw(&look, o, &ink);
    let r = f.r(items[0].rect);
    assert_eq!(c.pixel(r.x - o.x + 8, r.y + r.h / 2), crate::canvas::pack(ink.accent), "Weiterspielen in accent");
    let s = f.r(items[2].rect);
    assert_eq!(c.pixel(s.x - o.x + 4, s.y + s.h / 2), crate::canvas::pack(ink.accent), "the slider's filled part");
    let rows = fake_rows();
    let v = Values::default();
    let srows = rows_of(3, &input(&v, &rows));
    let items = lay::settings(bw, srows.len());
    let look = crate::sofa_settings_draw::SettingsLook { frame: f, bw, bh, section: 3, rows: srows, items: items.clone(), focus: Some("row:0".into()), note: Some("Gelernt: die neue Belegung gilt.".into()), learn: None, confirm: None };
    let o = f.r(crate::layout::Rect { x: 0, y: lay::HEADER_H, w: bw, h: bh - lay::HEADER_H - lay::FOOTER_H });
    let c = crate::sofa_settings_draw::draw_settings(&look, o, &ink);
    let side = f.r(items[3].rect);
    assert_eq!(c.pixel(side.x + 4, side.y - o.y + side.h / 2), crate::canvas::pack(ink.accent_soft), "Sofa-Modus is the active section");
    let row = f.r(items[6].rect);
    assert_eq!(c.pixel(row.x + row.w - 40, row.y - o.y + row.h / 2), crate::canvas::pack(ink.accent), "the switch is on");
    let texts = "Schnellmenü LÄUFT GERADE Weiterspielen Spiel beenden Lautstärke 30 % · HDMI Fernseher (HDMI) Kopfhörer (Klinke) Kopfhörer (USB) \
        WLAN GAMEPAD BILDSCHIRM LEISTUNG 2560 × 1440 CPU 41 % · 68 °C Verbunden · lädt · 80 % Keins verbunden Kein Netz Zum normalen Desktop Neu starten \
        Ausschalten Wirklich beenden? Nicht gespeicherter Fortschritt geht verloren. Alle Programme werden beendet. Ja Nein Neustart aus dem Sofa-Modus \
        kommt noch Bild und Ton Steuerung Netz Sofa-Modus Konten System Auflösung Bildausschnitt Für Fernseher, die den Rand abschneiden \
        Tonausgabe Tasten zuordnen Home-Taste Drücke LB (links oben) 3 von 8 gelernt Abbrechen: Esc · Rechtsklick ‹ › •••••••• – Größe A–Z \
        Steam meldest du in Steam selbst an. „Rückgängig“ Rücktaste nur im Desktop änderbar (mit Rückfrage)";
    for fnt in [font(Typeface::Figtree, 20.0, Weight::SemiBold, k), font(Typeface::Figtree, 17.0, Weight::Regular, k), font(Typeface::JetBrainsMono, 22.0, Weight::Medium, k), font(Typeface::Bricolage, 48.0, Weight::SemiBold, k)] {
        for ch in texts.chars().filter(|c| *c != ' ') {
            assert!(fnt.draws(ch), "{ch:?} missing");
        }
    }
}

#[test]
fn the_system_row_names_the_generation() {
    assert_eq!(parse_generation("generation=130 reason=NEWEST_OK reload=0 version=0.1.0\n"), Some(130));
    assert_eq!(parse_generation("nothing"), None);
    assert_eq!(generation_text(Some(130)), "Generation 130");
    assert_eq!(generation_text(Some(0)), "Generation unbekannt");
    assert_eq!(generation_text(None), "Generation unbekannt");
}
