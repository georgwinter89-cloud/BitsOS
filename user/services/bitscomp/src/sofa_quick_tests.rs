use super::*;
use crate::status::Block;

#[test]
fn the_slider_steps_by_five_on_the_grid() {
    assert_eq!(step_volume(30, 1), 35);
    assert_eq!(step_volume(30, -1), 25);
    assert_eq!(step_volume(32, 1), 35);
    assert_eq!(step_volume(32, -1), 30);
    assert_eq!(step_volume(100, 1), 100);
    assert_eq!(step_volume(0, -1), 0);
    assert_eq!(step_volume(3, -1), 0);
    assert_eq!(step_volume(200, -1), 95);
}

#[test]
fn the_quick_menus_words() {
    assert_eq!(volume_text(Some(30), Some("hdmi")), "30 % · HDMI");
    assert_eq!(volume_text(Some(30), None), "30 %");
    assert_eq!(volume_text(None, Some("jack")), "nicht da");
    assert_eq!(header_word(Some(7)), "Ton 7 %");
    assert_eq!(header_word(None), crate::sofa_app::VOLUME_UNKNOWN);
    assert_eq!(active_output(Some("auto"), Some("hdmi")).as_deref(), Some("hdmi"));
    assert_eq!(active_output(Some("jack"), Some("hdmi")).as_deref(), Some("jack"));
    assert_eq!(active_output(None, None), None);
    assert_eq!(output_label("hdmi"), "Fernseher (HDMI)");
    assert_eq!(output_label("usb"), "Kopfhörer (USB)");
    assert_eq!(wlan_text(Some("WLAN"), Some("FRITZ!Box")), "FRITZ!Box · verbunden");
    assert_eq!(wlan_text(Some("Kabel"), Some("FRITZ!Box")), "Kabel · verbunden");
    assert_eq!(wlan_text(Some("getrennt"), Some(" ")), "Kein Netz");
    assert_eq!(wlan_text(None, Some("Wohnzimmer")), "Wohnzimmer · getrennt");
    let b = |c: Option<u8>, s: &str| Battery { capacity: c, status: s.into() };
    assert_eq!(pad_text(1, Some(&b(Some(80), "Discharging"))), "Verbunden · 80 %");
    assert_eq!(pad_text(1, Some(&b(Some(40), "Charging"))), "Verbunden · lädt · 40 %");
    assert_eq!(pad_text(1, Some(&b(None, "Unknown"))), "Verbunden", "no value yet: never Kabel, never 0 %");
    assert_eq!(pad_text(1, Some(&b(Some(0), "Unknown"))), "Verbunden");
    assert_eq!(pad_text(1, Some(&b(None, "Discharging"))), "Verbunden · Kabel");
    assert_eq!(pad_text(2, None), "Verbunden · Kabel");
    assert_eq!(pad_text(0, Some(&b(Some(80), "Full"))), "Keins verbunden");
    let blocks = vec![Block { label: "CPU".into(), value: "41 %".into() }, Block { label: "Temperatur".into(), value: "68 °C".into() }];
    assert_eq!(power_text(&blocks), "CPU 41 % · 68 °C");
    assert_eq!(power_text(&blocks[..1]), "CPU 41 %");
    assert_eq!(power_text(&[]), "CPU –");
    assert_eq!(screen_text(2560, 1440), "2560 × 1440");
}

#[test]
fn the_questions_say_what_the_board_says() {
    assert_eq!(format!("{} {}", Confirm::End.title(), Confirm::End.text()), "Wirklich beenden? Nicht gespeicherter Fortschritt geht verloren.");
    assert_eq!(Confirm::Off.text(), "Alle Programme werden beendet.");
    assert_eq!(Confirm::Reboot.text(), Confirm::Off.text());
}

#[test]
fn the_quick_menu_and_its_question_walk_by_the_pad() {
    use crate::sofa_focus::{self as focus, Area};
    let m = &crate::sofa_draw::measure;
    let items = lay::quick(1920, 1080, true, &["Fernseher (HDMI)", "Kopfhörer (Klinke)"], m);
    let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, ["q:resume", "q:end", "q:volume", "q:out:0", "q:out:1", "q:wlan", "q:pad", "q:screen", "q:power", "q:desktop", "q:reboot", "q:off"]);
    assert!(items.iter().all(|i| i.area == Area::Panel && i.rect.x >= 1920 - lay::QUICK_W && i.rect.x + i.rect.w <= 1920));
    let at = |id: &str| focus::index(&items, id).unwrap();
    assert_eq!(focus::step(&items, at("q:resume"), Dir::Right), Some(at("q:end")));
    assert_eq!(focus::step(&items, at("q:end"), Dir::Down), Some(at("q:volume")));
    let chip = focus::step(&items, at("q:volume"), Dir::Down).map(|i| items[i].id.as_str());
    assert!(chip.is_some_and(|c| c.starts_with("q:out:")), "{chip:?}");
    assert_eq!(focus::step(&items, at("q:out:1"), Dir::Left), Some(at("q:out:0")));
    assert_eq!(focus::step(&items, at("q:off"), Dir::Down), None, "no way out of the panel");
    let c = lay::confirm(1920, 1080, true);
    assert_eq!(focus::step(&c, 1, Dir::Left), Some(0));
    let b = lay::confirm_box(1920, 1080, false);
    assert!(c.iter().all(|i| i.rect.x >= 1920 - lay::QUICK_W) && b.x > 0 && b.x + b.w < 1920);
}
