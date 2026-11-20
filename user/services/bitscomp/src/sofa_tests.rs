use super::*;
use crate::pad::{Btn, Press};
use crate::state::App;
use smithay::reexports::wayland_server::Display;

fn ctx(active: bool, program: bool, quick: bool, start_on_pad: bool) -> Ctx {
    Ctx { active, program, quick, start_on_pad }
}

const ALL: [Way; 10] = [Way::Home, Way::WinEsc, Way::Tile, Way::Address, Way::PadPlug, Way::Boot, Way::CtlOn, Way::CtlOff, Way::Desktop, Way::Workspace];

#[test]
fn every_way_in_enters_from_the_desktop() {
    for w in [Way::Home, Way::WinEsc, Way::Tile, Way::Address, Way::CtlOn] {
        for pad in [true, false] {
            assert_eq!(decide(ctx(false, false, false, pad), w), Step::Enter, "{w:?}");
        }
    }
    // A pad plugged in (or there at the start) enters only with sofa.start_on_pad on.
    for w in [Way::PadPlug, Way::Boot] {
        assert_eq!(decide(ctx(false, false, false, true), w), Step::Enter, "{w:?} with the setting on");
        assert_eq!(decide(ctx(false, false, false, false), w), Step::Stay, "{w:?} with the setting off");
    }
    // A way out on the desktop changes nothing.
    for w in [Way::CtlOff, Way::Desktop, Way::Workspace] {
        assert_eq!(decide(ctx(false, false, false, true), w), Step::Stay, "{w:?}");
    }
}

#[test]
fn every_way_out_leaves_and_home_over_a_program_opens_the_quick_menu() {
    // On a sofa screen: Home, Win+Esc, ctl off, Zum normalen Desktop and a workspace key leave.
    for w in [Way::Home, Way::WinEsc, Way::CtlOff, Way::Desktop, Way::Workspace] {
        assert_eq!(decide(ctx(true, false, false, true), w), Step::Leave, "{w:?}");
    }
    // A second way in, or a pad plugged in, stays.
    for w in [Way::Tile, Way::Address, Way::CtlOn, Way::PadPlug, Way::Boot] {
        for (program, quick) in [(false, false), (true, false), (true, true)] {
            assert_eq!(decide(ctx(true, program, quick, true), w), Step::Stay, "{w:?}");
        }
    }
    // Home over a program in front: the quick menu, Home again closes it (the program goes on).
    assert_eq!(decide(ctx(true, true, false, true), Way::Home), Step::QuickOpen);
    assert_eq!(decide(ctx(true, true, true, true), Way::Home), Step::QuickClose);
    // A quick menu opened on a sofa screen (Menü) closes by Home too.
    assert_eq!(decide(ctx(true, false, true, true), Way::Home), Step::QuickClose);
    // Win+Esc leaves even over a program (the keyboard's way to the desktop), the quick menu open or not.
    for quick in [false, true] {
        assert_eq!(decide(ctx(true, true, quick, true), Way::WinEsc), Step::Leave);
    }
    // No way closes, pauses or restarts anything: the only steps there are.
    for w in ALL {
        for a in [false, true] {
            for p in [false, true] {
                let s = decide(ctx(a, p, false, true), w);
                assert!(matches!(s, Step::Enter | Step::Leave | Step::QuickOpen | Step::QuickClose | Step::Stay));
                assert!(!a || s != Step::Enter, "already on: {w:?}");
                assert!(a || !matches!(s, Step::Leave | Step::QuickOpen | Step::QuickClose), "off: {w:?}");
            }
        }
    }
}

#[test]
fn the_scale_is_k_of_the_boards_with_the_overscan_inset() {
    let f = Frame::of(2560, 1440, 100);
    assert_eq!((f.x, f.y, f.w, f.h), (0, 0, 2560, 1440));
    assert!((f.k - 4.0 / 3.0).abs() < 1e-9, "k = 4/3 at 2560x1440");
    assert_eq!(f.board(), (1920, 1080));
    // Board px x 4/3: the header 96 -> 128, the hero 1180x470 -> 1573x627 (sofa-mode.md §2).
    assert_eq!(f.r(Rect { x: 0, y: 0, w: 1920, h: 96 }).h, 128);
    let hero = f.r(Rect { x: 64, y: 144, w: 1180, h: 470 });
    // Edges are rounded (64 and 1244 board px -> 85 and 1659), so the width is 1574 where §2 says about 1573.
    assert_eq!((hero.x, hero.y, hero.w, hero.h), (85, 192, 1574, 627));
    // 90 % overscan: the layout shrinks around the centre, k with it; the display mode is not touched.
    let o = Frame::of(2560, 1440, 90);
    assert_eq!((o.x, o.y, o.w, o.h), (128, 72, 2304, 1296));
    assert!((o.k - 1.2).abs() < 1e-9);
    assert_eq!(o.board(), (1920, 1080));
    // Out of range values are kept inside 90..=100.
    assert_eq!(Frame::of(1920, 1080, 50), Frame::of(1920, 1080, 90));
    // Another shape: k from the short side, the long side gets more board.
    let w = Frame::of(2560, 1600, 100);
    assert!((w.k - 4.0 / 3.0).abs() < 1e-9);
    assert_eq!(w.board(), (1920, 1200));
    let (bx, by) = f.board_point(128.0, 64.0);
    assert!((bx - 96.0).abs() < 1e-9 && (by - 48.0).abs() < 1e-9);
}

#[test]
fn the_prefs_follow_the_keys_and_keep_their_defaults_on_bad_values() {
    let d = Prefs::default();
    assert!(d.start_on_pad && d.motion && d.chord);
    assert_eq!((d.sort.as_str(), d.start_volume, d.overscan, d.pad_map.as_str()), ("zuletzt", 20, 100, ""));
    let mut v = Values::default();
    for (k, val) in [("sofa.start_on_pad", "false"), ("sofa.sort", "groesse"), ("sofa.overscan", "95"), ("sofa.home_chord", "aus"), ("sofa.start_volume", "35")] {
        v.set(k, val);
    }
    let p = Prefs::of(&v);
    assert!(!p.start_on_pad && !p.chord);
    assert_eq!((p.sort.as_str(), p.overscan, p.start_volume), ("groesse", 95, 35));
    for (k, val) in [("sofa.overscan", "80"), ("sofa.sort", "zufall"), ("sofa.start_volume", "x"), ("sofa.home_chord", "ja")] {
        v.set(k, val);
    }
    let p = Prefs::of(&v);
    assert_eq!((p.overscan, p.sort.as_str(), p.start_volume, p.chord), (100, "zuletzt", 20, true), "bad values: defaults");
    v.set("theme.motion", "off");
    assert!(!Prefs::of(&v).motion, "theme.motion off turns the lift off");
}

#[test]
fn the_keys_of_section_6_are_typed_settingsd_definitions() {
    let defs = defs();
    let names: Vec<&str> = defs.iter().filter_map(|d| d.get("key").and_then(Json::as_str)).collect();
    assert_eq!(names, KEYS);
    for d in &defs {
        let s = d.get("schema").unwrap();
        let ty = s.get("type").and_then(Json::as_str).unwrap();
        let def = d.get("default").unwrap();
        match ty {
            "bool" => assert!(def.as_bool().is_some()),
            "int" => {
                let (lo, hi, v) = (s.get("min").and_then(Json::as_i64).unwrap(), s.get("max").and_then(Json::as_i64).unwrap(), def.as_i64().unwrap());
                assert!(lo <= v && v <= hi, "{d}");
            }
            "choice" => {
                let c: Vec<&str> = s.get("choices").and_then(Json::as_array).unwrap().iter().filter_map(Json::as_str).collect();
                let de = s.get("choices_de").and_then(Json::as_array).unwrap();
                assert_eq!(c.len(), de.len(), "a German name per value");
                assert!(c.contains(&def.as_str().unwrap()), "{d}");
            }
            "text" => assert_eq!(s.get("max_bytes").and_then(Json::as_i64), Some(4096)),
            o => panic!("type {o}"),
        }
        // §6's table, key by key.
        let key = d.get("key").and_then(Json::as_str).unwrap();
        let choices = || s.get("choices").and_then(Json::as_array).map(|a| a.iter().filter_map(Json::as_str).collect::<Vec<_>>());
        let range = || (s.get("min").and_then(Json::as_i64), s.get("max").and_then(Json::as_i64));
        match key {
            "sofa.start_on_pad" | "sofa.motion" => assert_eq!((ty, def.as_bool()), ("bool", Some(true)), "{key}"),
            "sofa.sort" => assert_eq!((choices(), def.as_str()), (Some(vec!["zuletzt", "az", "groesse"]), Some("zuletzt"))),
            "sofa.home_chord" => assert_eq!((choices(), def.as_str()), (Some(vec!["select_start", "aus"]), Some("select_start"))),
            "sofa.start_volume" => assert_eq!((range(), def.as_i64()), ((Some(0), Some(100)), Some(20))),
            "sofa.overscan" => assert_eq!((range(), def.as_i64()), ((Some(90), Some(100)), Some(100))),
            "sofa.pad.map" => assert_eq!((ty, def.as_str()), ("text", Some(""))),
            o => panic!("key {o}"),
        }
        let de = d.get("text").and_then(|t| t.get("de")).and_then(Json::as_str).unwrap();
        assert!(!de.is_empty());
        assert_eq!(d.get("safety").and_then(Json::as_str), Some("normal"));
    }
    let line = crate::settings_write::request_line(&crate::settings_write::Req::Defs { keys: defs.clone() });
    let j = Json::parse(&line).unwrap();
    assert_eq!(j.get("op").and_then(Json::as_str), Some("register"));
    assert_eq!(j.get("owner").and_then(Json::as_str), Some("bitscomp"));
    assert_eq!(j.get("keys").and_then(Json::as_array).map(<[Json]>::len), Some(7));
}

fn new_app() -> (Display<App>, App) {
    let d = Display::<App>::new().expect("display");
    let app = App::new(d.handle(), "wy-sofa-test".into(), (2560, 1440));
    (d, app)
}

#[test]
fn switching_makes_the_sofa_workspace_once_and_comes_back_to_the_one_before() {
    let (_d, mut app) = new_app();
    let first = app.desk.names[0].clone();
    app.sofa_switch(Way::CtlOn);
    assert!(app.sofa.active);
    assert_eq!(app.desk.names, vec![first.clone(), WORKSPACE.to_string()], "Sofa is made after the others");
    assert_eq!(app.desk.active, 1, "Sofa is shown");
    assert_eq!(app.sofa.show(), Show::Shell, "no program: the start screen");
    assert_eq!(app.sofa.focus.as_deref(), Some("hero"), "the start screen's focus starts on the hero (2.1)");
    app.sofa_switch(Way::WinEsc);
    assert!(!app.sofa.active);
    assert_eq!(app.desk.active, 0, "the workspace shown before");
    assert_eq!(app.desk.names.len(), 2, "Sofa stays in the list");
    // From another workspace: back to that one; Sofa is not made twice.
    app.ws_new();
    assert_eq!(app.desk.active, 2);
    app.sofa_switch(Way::Address);
    assert_eq!((app.desk.active, app.desk.names.iter().filter(|n| *n == WORKSPACE).count()), (1, 1));
    assert!(!app.ws_remove(1), "Sofa is not removed while the mode is on");
    app.sofa_switch(Way::Workspace);
    assert_eq!(app.desk.active, 2);
}

#[test]
fn home_over_a_program_opens_the_quick_menu_and_the_pad_is_the_programs() {
    let (_d, mut app) = new_app();
    app.sofa_switch(Way::Home);
    // A program in front (sofa_window_new sets this when its window maps on Sofa).
    app.sofa.front = Front::Program;
    assert_eq!(app.sofa.show(), Show::None, "the program alone");
    assert!(!app.sofa.listens());
    app.sofa_switch(Way::Home);
    assert_eq!(app.sofa.show(), Show::Over, "Home: the quick menu over it");
    app.sofa_pad(Press::Btn(Btn::B));
    assert_eq!(app.sofa.show(), Show::None, "B closes it, the program goes on");
    app.sofa_switch(Way::Home);
    app.sofa_switch(Way::Home);
    assert_eq!(app.sofa.show(), Show::None, "Home again closes it");
    assert!(app.sofa.active, "nothing left the mode");
    app.sofa_switch(Way::WinEsc);
    assert!(!app.sofa.active);
    app.sofa_switch(Way::WinEsc);
    assert_eq!(app.sofa.show(), Show::Shell, "back in without a real window here: the start screen");
}

#[test]
fn a_pad_plugged_in_starts_the_sofa_mode_only_with_the_setting_on() {
    use crate::input::Origin;
    let byte = crate::pad::Axis { min: 0, max: 255, flat: 15 };
    let keys: Vec<u16> = (0x120..=0x12b).collect();
    let pad = || crate::pad_app::PadDev::new("test", "DragonRise", (0x0079, 0x0006), &keys, (Some(byte), Some(byte)), Origin::Physical);
    // Before settingsd's values: it waits for them, then the setting (default on) decides.
    let (_d, mut app) = new_app();
    app.pad_plug(1, pad());
    assert!(!app.sofa.active && app.sofa.pending.is_some(), "waits for the settings");
    app.sofa_settings_came();
    assert!(app.sofa.active && app.sofa.pending.is_none(), "at the start with a pad: the sofa mode");
    // The settings never come: the defaults decide at the deadline.
    let (_d, mut app) = new_app();
    app.pad_plug(1, pad());
    let due = app.sofa_due().unwrap();
    app.sofa_tick(due);
    assert!(app.sofa.active);
    // Hot plug with the setting off: the desktop stays; on: the sofa mode.
    let (_d, mut app) = new_app();
    app.sofa_settings_came();
    app.settings.set("sofa.start_on_pad", "false");
    app.sofa_apply();
    app.pad_plug(1, pad());
    assert!(!app.sofa.active, "setting off");
    app.pad_gone(1);
    app.settings.set("sofa.start_on_pad", "true");
    app.sofa_apply();
    app.pad_plug(2, pad());
    assert!(app.sofa.active, "setting on");
    // sofa.home_chord aus: Select+Start held is no Home any more.
    app.settings.set("sofa.home_chord", "aus");
    app.sofa_apply();
    assert!(app.pads.list.iter().all(|(_, d)| d.pad.chord_off));
}

#[test]
fn the_librarys_tile_and_the_address_enter() {
    let (_d, mut app) = new_app();
    assert_eq!(app.library.by_address("sofa/").map(|e| e.id.as_str()), Some("sofa-modus"), "the built-in tile has the address");
    app.library_open("test");
    app.library_start("sofa-modus", crate::address::Where::Tab);
    assert!(app.sofa.active && !app.lib.open, "the tile enters and the library closes");
    app.sofa_switch(Way::CtlOff);
    for a in ["sofa/", "Sofa", "sofa-modus"] {
        app.open_address(a, crate::address::Where::Tab, None);
        assert!(app.sofa.active, "{a}");
        app.sofa_switch(Way::CtlOff);
    }
    assert_eq!(crate::address::resolve("sofa/"), crate::address::Open::Sofa);
}

fn dragonrise() -> crate::pad_app::PadDev {
    let byte = crate::pad::Axis { min: 0, max: 255, flat: 15 };
    let keys: Vec<u16> = (0x120..=0x12b).collect();
    crate::pad_app::PadDev::new("test", "DragonRise", (0x0079, 0x0006), &keys, (Some(byte), Some(byte)), crate::input::Origin::Physical)
}

use crate::backend::evdev_map::{EV_ABS, EV_KEY, EV_SYN, SYN_REPORT};

fn key(app: &mut App, code: u16, v: i32) {
    app.pad_feed(1, &[(EV_KEY, code, v), (EV_SYN, SYN_REPORT, 0)]);
}

fn hat(app: &mut App, x: i32) {
    app.pad_feed(1, &[(EV_ABS, crate::pad::ABS_HAT0X, x), (EV_SYN, SYN_REPORT, 0)]);
}

#[test]
fn the_saved_session_never_shows_the_sofa_workspace() {
    let (_d, mut app) = new_app();
    app.ws_new();
    app.sofa_switch(Way::CtlOn);
    app.sofa_switch(Way::CtlOff);
    // A workspace made after Sofa: the indices of the saved list skip Sofa.
    app.ws_new();
    assert_eq!(app.desk.active, 3);
    app.sofa_switch(Way::CtlOn);
    assert_eq!(app.desk.names[app.desk.active], WORKSPACE);
    let s = app.session_now();
    let names: Vec<&str> = s.spaces.iter().map(|sp| sp.name.as_str()).collect();
    assert!(!names.contains(&WORKSPACE), "Sofa is not saved: {names:?}");
    assert_eq!(names.len(), 3);
    assert_eq!(names[s.active], app.desk.names[3], "the workspace shown before the mode is the saved one");
    app.sofa_switch(Way::CtlOff);
    let s = app.session_now();
    assert_eq!(s.spaces[s.active].name, app.desk.names[3]);
}

#[test]
fn a_program_ending_under_the_quick_menu_closes_it() {
    let (_d, mut app) = new_app();
    app.sofa_switch(Way::CtlOn);
    app.sofa.front = Front::Program;
    app.sofa_switch(Way::Home);
    assert_eq!(app.sofa.show(), Show::Over);
    // The program's last window goes (none is mapped here: the workspace is empty).
    app.sofa_window_gone();
    assert_eq!((app.sofa.front, app.sofa.nav.quick, app.sofa.show()), (Front::Shell, false, Show::Shell));
    assert!(app.sofa.focus.is_some(), "the start screen has its focus again");
}

#[test]
fn the_zum_desktop_hint_leaves_and_unplugging_the_last_pad_keeps_the_mode() {
    let (_d, mut app) = new_app();
    app.sofa_settings_came();
    app.pad_plug(1, dragonrise());
    assert!(app.sofa.active);
    app.pad_gone(1);
    assert!(app.sofa.active && app.sofa.show() == Show::Shell, "the mode stays without a pad (the keyboard and mouse work)");
    assert_eq!(app.pad_due(), None);
    app.pad_tick(crate::pacer::now() + std::time::Duration::from_secs(1));
    app.sofa_activate("hint:home");
    assert!(!app.sofa.active, "Zum Desktop leaves");
    assert_eq!(app.desk.active, 0);
}

#[test]
fn the_pad_repeats_on_a_sofa_screen_and_the_chord_works_behind_a_program() {
    use std::time::Duration;
    let (_d, mut app) = new_app();
    app.sofa_settings_came();
    app.pad_plug(1, dragonrise());
    assert_eq!(app.sofa.focus.as_deref(), Some("hero"));
    app.sofa.focus = Some("pill:start".into());
    // The hat held right: one move now, the repeat after REPEAT_FIRST_MS through pad_due/pad_tick (not silenced).
    hat(&mut app, 1);
    assert_eq!(app.sofa.focus.as_deref(), Some("pill:bibliothek"));
    assert!(app.pad_due().is_some(), "the repeat wakes the loop on a sofa screen");
    app.pad_tick(crate::pacer::now() + Duration::from_millis(crate::pad::REPEAT_FIRST_MS + 50));
    assert_eq!(app.sofa.focus.as_deref(), Some("pill:medien"), "the held hat repeated");
    hat(&mut app, 0);
    // Start alone opens the quick menu when let go (Select+Start is this pad's Home); B closes it.
    key(&mut app, 0x129, 1);
    assert!(!app.sofa.nav.quick);
    key(&mut app, 0x129, 0);
    assert!(app.sofa.nav.quick, "Start alone came out on release");
    key(&mut app, 0x121, 1);
    key(&mut app, 0x121, 0);
    assert!(!app.sofa.nav.quick);
    // Behind a program in front: A and the hat are the program's, the chord still opens the quick menu.
    app.sofa.front = Front::Program;
    let focus = app.sofa.focus.clone();
    key(&mut app, 0x122, 1);
    key(&mut app, 0x122, 0);
    hat(&mut app, -1);
    hat(&mut app, 0);
    assert_eq!((app.sofa.focus.clone(), app.sofa.show()), (focus, Show::None), "the pad is the program's");
    key(&mut app, 0x128, 1);
    key(&mut app, 0x129, 1);
    assert!(app.pad_due().is_some(), "the chord's hold wakes the loop behind a program");
    app.pad_tick(crate::pacer::now() + Duration::from_millis(crate::pad::CHORD_MS + 50));
    assert_eq!(app.sofa.show(), Show::Over, "the chord is Home: the quick menu over the program");
    key(&mut app, 0x128, 0);
    key(&mut app, 0x129, 0);
    assert_eq!(app.sofa.show(), Show::Over, "letting the chord go gives no Select or Start");
}

#[test]
fn the_quick_menu_keeps_its_screen_and_the_typed_letters_are_bounded() {
    let (_d, mut app) = new_app();
    app.sofa_switch(Way::CtlOn);
    app.sofa_pad(Press::Btn(Btn::Start));
    assert!(app.sofa.nav.quick);
    for b in [Btn::Y, Btn::LB, Btn::RB] {
        app.sofa_pad(Press::Btn(b));
    }
    app.sofa_activate("search");
    assert_eq!(app.sofa.nav.screen, crate::sofa_focus::Screen::Start, "no screen change under the quick menu");
    app.sofa_pad(Press::Btn(Btn::B));
    assert!(!app.sofa.nav.quick);
    for _ in 0..TYPED_MAX + 50 {
        app.sofa_type('a');
    }
    assert_eq!(app.sofa.search.field.len(), TYPED_MAX);
    assert!(app.sofa.search.field.full);
}

#[test]
fn a_settings_change_reaches_the_sofa_frame() {
    let (_d, mut app) = new_app();
    assert!((app.sofa_frame().k - 4.0 / 3.0).abs() < 1e-9);
    app.settings_msg(crate::settings::Msg::Changed("sofa.overscan".into(), Some("90".into())));
    let f = app.sofa_frame();
    assert_eq!((f.x, f.y, f.w, f.h), (128, 72, 2304, 1296));
    assert!((f.k - 1.2).abs() < 1e-9);
    app.settings_msg(crate::settings::Msg::Changed("sofa.home_chord".into(), Some("aus".into())));
    assert!(!app.sofa.prefs.chord);
}

#[test]
fn the_header_keeps_a_place_for_the_volume() {
    let (_d, app) = new_app();
    let (words, _) = app.sofa_status();
    assert_eq!(words.len(), 2, "network, volume");
    assert_eq!(words[1], crate::sofa_app::VOLUME_UNKNOWN);
}

/// GM-6b: the library's collections (LB/RB), the sort button (sofa.sort), the selection and the grid's scroll,
/// on a fake index with Steam's games; nothing is started here (the scenario 62 starts a fake steam).
#[test]
fn the_library_screen_switches_collections_sorts_selects_and_scrolls() {
    use crate::pad::Dir;
    let (_d, mut app) = new_app();
    app.library = std::sync::Arc::new(crate::sofa_model::tests::index("app-lib"));
    app.sofa_switch(Way::CtlOn);
    assert_eq!(app.sofa.focus.as_deref(), Some("hero"));
    assert!(app.sofa_state().contains(" hero=steam-1044720 "), "the newest by Steam's LastPlayed: {}", app.sofa_state());
    app.sofa_activate("alle");
    assert_eq!(app.sofa.nav.screen, crate::sofa_focus::Screen::Bibliothek);
    assert_eq!(app.sofa.focus.as_deref(), Some("tile:0"));
    assert!(app.sofa_state().contains(" coll=spiele sort=zuletzt sel=steam-1044720 first=0 tiles=6 "), "{}", app.sofa_state());
    app.sofa_pad(Press::Btn(Btn::RB));
    assert!(app.sofa_state().contains(" coll=windows ") && app.sofa_state().contains(" tiles=3 "), "{}", app.sofa_state());
    app.sofa_pad(Press::Btn(Btn::LB));
    app.sofa_pad(Press::Btn(Btn::LB));
    assert!(app.sofa_state().contains(" coll=spiele "), "LB at the first collection stays: {}", app.sofa_state());
    app.sofa_pad(Press::Move(Dir::Right));
    assert_eq!(app.sofa.focus.as_deref(), Some("tile:1"));
    assert!(app.sofa_state().contains(" sel=steam-32360 "), "the selection follows the focus: {}", app.sofa_state());
    // The sort button cycles sofa.sort (kept locally without settingsd).
    app.sofa_activate("sort");
    assert_eq!((app.sofa.prefs.sort.as_str(), app.settings.text("sofa.sort")), ("az", Some("az")));
    assert_eq!(app.sofa_tiles()[0].key, "steam-900001");
    app.sofa_activate("sort");
    assert_eq!(app.sofa.prefs.sort, "groesse");
    app.sofa_activate("sort");
    assert_eq!(app.sofa.prefs.sort, "zuletzt");
    // Six games: the second row is the last one; Down from it goes to the bar, B back to Start.
    app.sofa.focus = Some("tile:5".into());
    app.sofa_pad(Press::Move(Dir::Down));
    assert_eq!(app.sofa.focus.as_deref(), Some("play"));
    assert_eq!(app.sofa.body.first, 0);
    app.sofa_pad(Press::Btn(Btn::B));
    assert_eq!(app.sofa.nav.screen, crate::sofa_focus::Screen::Start);
    // Medien: YouTube, Brave and Musik only, opened on YouTube.
    app.sofa_activate("pill:medien");
    assert!(app.sofa_state().contains(" coll=youtube "), "{}", app.sofa_state());
    assert_eq!(app.sofa_colls().len(), 3);
}

/// GM-6b: a library with more games than its rows scrolls by the D-pad (R1: no fixed count of tiles).
#[test]
fn the_grid_scrolls_with_more_games_than_fit() {
    use crate::pad::Dir;
    let (_d, mut app) = new_app();
    let mut ix = crate::sofa_model::tests::index("app-scroll");
    let base = ix.entries.iter().find(|e| e.id == "steam-32360").unwrap().clone();
    for n in 0..20 {
        ix.entries.push(crate::library::Entry { id: format!("steam-{}", 100 + n), name: format!("Spiel {n:02}"), ..base.clone() });
    }
    app.library = std::sync::Arc::new(ix);
    app.sofa_switch(Way::CtlOn);
    app.sofa_activate("alle");
    app.sofa.focus = Some("tile:7".into());
    app.sofa_pad(Press::Move(Dir::Down));
    assert_eq!((app.sofa.focus.as_deref(), app.sofa.body.first), (Some("tile:12"), 1), "the grid scrolled a row");
    assert!(app.sofa_items().iter().any(|i| i.id == "tile:12") && !app.sofa_items().iter().any(|i| i.id == "tile:0"));
    for _ in 0..10 {
        app.sofa_pad(Press::Move(Dir::Down));
    }
    assert_eq!(app.sofa.focus.as_deref(), Some("play"), "past the last row: the bar");
    app.sofa.focus = Some(format!("tile:{}", app.sofa.body.first * 5));
    for _ in 0..10 {
        app.sofa_pad(Press::Move(Dir::Up));
    }
    assert_eq!(app.sofa.body.first, 0);
    assert!(app.sofa.focus.as_deref().is_some_and(|f| f.starts_with("pill:")), "{:?}", app.sofa.focus);
}

#[test]
fn the_search_takes_the_pad_and_the_real_keyboard_into_one_field() {
    use crate::sofa_focus::Screen;
    use smithay::input::keyboard::{Keysym, ModifiersState};
    let (_d, mut app) = new_app();
    app.sofa_switch(Way::CtlOn);
    app.sofa_pad(Press::Btn(Btn::RB));
    assert_eq!(app.sofa.nav.screen, Screen::Bibliothek);
    // A letter on any sofa screen opens the search with it.
    let none = ModifiersState::default();
    assert!(app.sofa_key(Keysym::g, &none));
    assert_eq!((app.sofa.nav.screen, app.sofa.search.field.text.as_str()), (Screen::Suche, "g"));
    assert_eq!(app.sofa.focus.as_deref(), Some("key:1"));
    // The pad: A on ö types it, Y is space, LB moves the cursor, X deletes before it.
    app.sofa.focus = Some("key:ö".into());
    app.sofa_pad(Press::Btn(Btn::A));
    app.sofa_pad(Press::Btn(Btn::Y));
    assert_eq!(app.sofa.search.field.text, "gö ");
    app.sofa_pad(Press::Btn(Btn::LB));
    app.sofa_pad(Press::Btn(Btn::X));
    assert_eq!((app.sofa.search.field.text.as_str(), app.sofa.search.field.cursor), ("g ", 1));
    app.sofa_pad(Press::Btn(Btn::RB));
    // ⇧ Groß for the next letter alone; &123 shows the symbols, its key keeps the focus as abc.
    app.sofa_activate("key:⇧ Groß");
    app.sofa_activate("key:ä");
    app.sofa_activate("key:ä");
    assert_eq!(app.sofa.search.field.text, "g Ää");
    app.sofa_activate("key:&123");
    assert_eq!(app.sofa.focus.as_deref(), Some("key:abc"));
    app.sofa_activate("key:€");
    assert!(app.sofa_key(Keysym::BackSpace, &none));
    assert!(app.sofa_key(Keysym::BackSpace, &none));
    assert_eq!(app.sofa.search.field.text, "g Ä");
    // Start (Suchen) goes to the best hit; B goes back to the screen the search came from.
    app.sofa_pad(Press::Btn(Btn::Start));
    assert_eq!(app.sofa.focus.as_deref(), Some("hit:0"));
    assert!(!app.sofa.nav.quick, "Start on the search is Suchen, not the quick menu");
    app.sofa_pad(Press::Btn(Btn::B));
    assert_eq!(app.sofa.nav.screen, Screen::Bibliothek);
    // Y opens it again empty.
    app.sofa_pad(Press::Btn(Btn::Y));
    assert_eq!((app.sofa.nav.screen, app.sofa.search.field.text.as_str()), (Screen::Suche, ""));
    assert!(app.sofa_state().contains(" search= cursor=0 target=search "), "{}", app.sofa_state());
}

#[test]
fn a_held_a_repeats_on_the_keyboard_while_the_pad_holds_it() {
    use std::time::Duration;
    let (_d, mut app) = new_app();
    app.sofa_settings_came();
    app.pad_plug(1, dragonrise());
    app.sofa_pad(Press::Btn(Btn::Y));
    app.sofa.focus = Some("key:a".into());
    key(&mut app, 0x122, 1);
    assert_eq!(app.sofa.search.field.text, "a");
    let t = crate::pacer::now();
    assert!(app.sofa_due().is_some_and(|d| d <= t + crate::sofa_search::REPEAT_FIRST), "the repeat wakes the loop");
    app.sofa_tick(t + Duration::from_millis(400));
    assert_eq!(app.sofa.search.field.text, "a", "not before 500 ms");
    app.sofa_tick(t + Duration::from_millis(520));
    app.sofa_tick(t + Duration::from_millis(560));
    assert_eq!(app.sofa.search.field.text, "aaa");
    key(&mut app, 0x122, 0);
    app.sofa_tick(t + Duration::from_millis(700));
    assert_eq!(app.sofa.search.field.text, "aaa", "let go: no more");
    assert_eq!(app.sofa.search.pad, None);
    // Suchen does not repeat.
    app.sofa.focus = Some("key:Suchen".into());
    key(&mut app, 0x122, 1);
    assert_eq!(app.sofa.search.pad, None);
    key(&mut app, 0x122, 0);
}

#[test]
fn the_wifi_rows_open_the_keyboard_and_the_password_is_never_kept_without_settingsd() {
    use crate::sofa_focus::Screen;
    let (_d, mut app) = new_app();
    app.sofa_switch(Way::CtlOn);
    app.settings.set("net.wifi.ssid", "Flur");
    app.sofa_search_edit("net.wifi.ssid");
    assert_eq!((app.sofa.nav.screen, app.sofa.search.field.text.as_str()), (Screen::Suche, "Flur"), "the name to change");
    assert!(app.sofa_search_items().iter().all(|i| !i.id.starts_with("hit:")), "no hits on a settings field");
    assert!(app.sofa_search_items().iter().any(|i| i.id == "key:Fertig"));
    app.sofa_pad(Press::Btn(Btn::B));
    app.sofa_search_edit("net.wifi.psk");
    assert_eq!(app.sofa.search.field.text, "", "a secret is never read back");
    for c in "geheim".chars() {
        app.sofa_type(c);
    }
    assert!(app.sofa_state().contains(" search=(6_hidden) "), "{}", app.sofa_state());
    app.sofa_activate("key:Fertig");
    assert_eq!(app.settings.text("net.wifi.psk"), None, "not stored in bitscomp without settingsd");
    assert_eq!(app.sofa.set.note.as_deref(), Some("Das WLAN-Passwort geht nur mit dem Einstellungsdienst."));
    assert_ne!(app.sofa.nav.screen, Screen::Suche, "Fertig goes back");
}

#[test]
fn the_wifi_password_goes_only_to_settingsd_and_an_empty_field_writes_nothing() {
    use crate::settings_write::{Req, Writer};
    use crate::sofa_focus::Screen;
    let (_d, mut app) = new_app();
    app.sofa_switch(Way::CtlOn);
    let (w, rx) = Writer::for_test();
    app.settings_writer = Some(w);
    app.settings_live = true;
    let wifi = |rx: &std::sync::mpsc::Receiver<(u32, Req, std::time::Instant)>| -> Vec<(u32, Req)> {
        rx.try_iter().filter(|(_, r, _)| matches!(r, Req::Set { key, .. } if key.starts_with("net.wifi."))).map(|(p, r, _)| (p, r)).collect()
    };
    // Fertig on the empty password field (and Start, a stray press) is Abbrechen: the stored one stays.
    app.sofa_search_edit("net.wifi.psk");
    app.sofa_pad(Press::Btn(Btn::Start));
    assert_ne!(app.sofa.nav.screen, Screen::Suche, "back to the row");
    assert_eq!(app.sofa.set.note.as_deref(), Some("Nicht geändert: das Feld war leer."));
    assert!(wifi(&rx).is_empty(), "nothing sent for an empty field");
    // An emptied WLAN-Name is not written either.
    app.settings.set("net.wifi.ssid", "Flur");
    app.sofa_search_edit("net.wifi.ssid");
    for _ in 0..4 {
        app.sofa_pad(Press::Btn(Btn::X));
    }
    assert_eq!(app.sofa.search.field.text, "");
    app.sofa_activate("key:Fertig");
    assert!(wifi(&rx).is_empty());
    assert_eq!(app.settings.text("net.wifi.ssid"), Some("Flur"));
    // A typed password: one set to settingsd, nothing kept in bitscomp, the field dropped once back.
    app.sofa_search_edit("net.wifi.psk");
    for c in "geheim".chars() {
        app.sofa_type(c);
    }
    app.sofa_activate("key:Fertig");
    assert_eq!(wifi(&rx), vec![(crate::sofa_settings::SOFA_PID, Req::set("net.wifi.psk", Json::str("geheim")))]);
    assert_eq!(app.settings.text("net.wifi.psk"), None, "never stored in bitscomp's own values");
    assert_eq!((app.sofa.search.field.text.as_str(), app.sofa.search.target.secret()), ("", false), "the typed password is dropped");
    assert!(!app.sofa_state().contains("geheim"));
    // B (and leaving the mode) drops a half-typed one too.
    app.sofa_search_edit("net.wifi.psk");
    app.sofa_type('x');
    app.sofa_pad(Press::Btn(Btn::B));
    assert_eq!(app.sofa.search.field.text, "");
    app.sofa_search_edit("net.wifi.psk");
    app.sofa_type('y');
    app.sofa_switch(Way::WinEsc);
    assert_eq!(app.sofa.search.field.text, "");
    assert!(wifi(&rx).is_empty());
}

#[test]
fn a_held_real_key_repeats_on_the_search_screen_until_it_is_let_go() {
    use smithay::input::keyboard::{Keycode, Keysym, ModifiersState};
    use std::time::Duration;
    let (_d, mut app) = new_app();
    app.sofa_switch(Way::CtlOn);
    let m = ModifiersState::default();
    let t0 = crate::pacer::now();
    // A letter: typed once on the press (input.rs calls sofa_key, then sofa_hold).
    assert!(app.sofa_key(Keysym::x, &m));
    app.sofa_hold(Keycode::new(53), Keysym::x, m);
    assert_eq!(app.sofa.search.field.text, "x");
    app.sofa_tick(t0 + Duration::from_millis(400));
    assert_eq!(app.sofa.search.field.text, "x", "no repeat before 500 ms");
    app.sofa_tick(t0 + Duration::from_millis(600));
    app.sofa_tick(t0 + Duration::from_millis(650));
    assert_eq!(app.sofa.search.field.text, "xxx", "then every 33 ms");
    app.sofa_release(Keycode::new(53));
    app.sofa_tick(t0 + Duration::from_millis(900));
    assert_eq!(app.sofa.search.field.text, "xxx", "let go: no more");
    // Rücktaste held deletes the same way.
    let t1 = crate::pacer::now();
    app.sofa_key(Keysym::BackSpace, &m);
    app.sofa_hold(Keycode::new(22), Keysym::BackSpace, m);
    assert_eq!(app.sofa.search.field.text, "xx");
    app.sofa_tick(t1 + Duration::from_millis(400));
    assert_eq!(app.sofa.search.field.text, "xx");
    app.sofa_tick(t1 + Duration::from_millis(600));
    assert_eq!(app.sofa.search.field.text, "x");
    app.sofa_release(Keycode::new(22));
    app.sofa_tick(t1 + Duration::from_millis(900));
    assert_eq!(app.sofa.search.field.text, "x");
    assert_eq!(app.sofa.search.due(), None, "nothing held: no wake-up");
}

#[test]
fn a_program_in_front_a_question_and_the_learn_screen_keep_the_keys_from_the_search() {
    use crate::sofa_focus::Screen;
    use smithay::input::keyboard::{Keycode, Keysym, ModifiersState};
    let (_d, mut app) = new_app();
    app.sofa_settings_came();
    app.pad_plug(1, dragonrise());
    let m = ModifiersState::default();
    // A program in front: the layer does not listen (input.rs passes keys to it only then), the pad's Y is the game's.
    app.sofa.front = Front::Program;
    assert!(!app.sofa.listens());
    key(&mut app, 0x120, 1);
    key(&mut app, 0x120, 0);
    app.sofa_hold(Keycode::new(42), Keysym::g, m);
    assert_eq!((app.sofa.nav.screen, app.sofa.search.held), (Screen::Start, None));
    // A question: it takes the keys first.
    app.sofa.front = Front::Shell;
    app.sofa_ask(crate::sofa_quick::Confirm::Off);
    app.sofa_key(Keysym::g, &m);
    app.sofa_pad(Press::Btn(Btn::Y));
    assert_eq!((app.sofa.nav.screen, app.sofa.search.field.text.as_str()), (Screen::Start, ""));
    assert!(app.sofa.confirm.is_some());
    app.sofa.confirm = None;
    // The learn screen: a letter does not drop the half-done learning for the search.
    app.sofa.set.learn = Some(crate::sofa_settings::Learn { pad: 1, got: Vec::new() });
    app.sofa_key(Keysym::g, &m);
    assert_eq!(app.sofa.nav.screen, Screen::Start);
    assert!(app.sofa.set.learn.is_some());
    app.sofa.set.learn = None;
    app.sofa_key(Keysym::g, &m);
    assert_eq!((app.sofa.nav.screen, app.sofa.search.field.text.as_str()), (Screen::Suche, "g"), "otherwise a letter opens it");
}
