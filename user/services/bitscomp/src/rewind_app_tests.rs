//! REWIND through the real input path (keys and clicks as Georg's or as injected), with bitssnap's answers fed in.

use mcpreg::Json;
use smithay::reexports::wayland_server::Display;

use super::*;
use crate::input::InputEvent;
use crate::input::Origin::{Injected, Physical};
use crate::rewind::Ask;

const KEY_LEFTMETA: u32 = 125;
/// bitscomp's keymap is German: Z sits where the US layout has Y (evdev KEY_Y).
const KEY_Z: u32 = 21;
const KEY_ENTER: u32 = 28;
const KEY_TAB: u32 = 15;
const KEY_ESC: u32 = 1;
const KEY_DOWN: u32 = 108;
const KEY_LEFT: u32 = 105;
const NOW: u64 = 1_794_493_800;

fn new_app() -> (Display<App>, App) {
    let d = Display::<App>::new().expect("display");
    let mut app = App::new(d.handle(), "wy-rewind-test".into(), (2560, 1440));
    app.rewind.sent = Some(Vec::new());
    (d, app)
}

fn key(app: &mut App, code: u32, o: crate::input::Origin) {
    for pressed in [true, false] {
        app.handle_input_as(InputEvent::Key { code, pressed }, o);
    }
}

fn win_z(app: &mut App) {
    for (code, pressed) in [(KEY_LEFTMETA, true), (KEY_Z, true), (KEY_Z, false), (KEY_LEFTMETA, false)] {
        app.handle_input_as(InputEvent::Key { code, pressed }, Physical);
    }
}

/// A click at the middle of the first part `h` of the view, by `o` in every part of it.
fn click(app: &mut App, h: Hot, o: crate::input::Origin) {
    let (_, r) = *app.rewind_parts().iter().find(|(p, _)| *p == h).unwrap_or_else(|| panic!("no {h:?} in {:?}", app.rewind_parts()));
    let s = app.theme.scale;
    let (x, y) = ((r.x as f64 + r.w as f64 / 2.0) * s, (r.y as f64 + r.h as f64 / 2.0) * s);
    app.handle_input_as(InputEvent::Motion { x, y }, o);
    for pressed in [true, false] {
        app.handle_input_as(InputEvent::Button { code: crate::pointer::BTN_LEFT, pressed }, o);
    }
}

/// The requests sent since the last look, as JSON.
fn sent(app: &mut App) -> Vec<(u64, Json)> {
    std::mem::take(app.rewind.sent.as_mut().unwrap()).into_iter().map(|(id, l)| (id, Json::parse(&l).unwrap())).collect()
}

fn op(j: &Json) -> &str {
    j.get("op").and_then(Json::as_str).unwrap_or("")
}

fn answer(app: &mut App, id: u64, text: &str) {
    app.rewind_msg(Msg::Reply(id, Json::parse(text).unwrap()));
}

/// Answers the one open list with three snapshots (newest 30, an auto one; 20 by hand; 10 before a restore).
fn listed(app: &mut App) {
    let l = sent(app);
    let (id, j) = l.iter().find(|(_, j)| op(j) == "list").expect("a list was asked");
    assert_eq!(op(j), "list");
    let id = *id;
    let snaps = r#"[{"id":30,"created_s":1794493200,"kind":"auto","label":"auto"},{"id":20,"created_s":1794400000,"kind":"manual","label":"vor Urlaub"},{"id":10,"created_s":1794300000,"kind":"before-restore","label":"before-restore"}]"#;
    answer(app, id, &format!(r#"{{"ok":true,"data":"ready","snapshots":{snaps},"utc_offset_min":60,"now_s":{NOW},"rollback_pending":null}}"#));
}

/// The view open on the three snapshots.
fn opened() -> (Display<App>, App) {
    let (d, mut app) = new_app();
    app.rewind_probe("test");
    listed(&mut app);
    win_z(&mut app);
    assert!(app.rewind.open);
    listed(&mut app);
    (d, app)
}

fn armed(app: &mut App) {
    app.rewind.since = app.rewind.since.saturating_sub(crate::install_dialog::ARM);
}

#[test]
fn grey_until_bitssnap_lists_snapshots_then_win_z_opens_and_closes() {
    let (_d, mut app) = new_app();
    win_z(&mut app);
    assert!(!app.rewind.open, "nothing asked yet: grey");
    assert_eq!(app.desk.notice.as_ref().map(|n| n.0.as_str()), app.rewind.avail.reason());
    let l = sent(&mut app);
    assert_eq!(l.len(), 1, "Win+Z on the grey button asks again");
    answer(&mut app, l[0].0, r#"{"ok":true,"data":"ready","snapshots":[]}"#);
    assert_eq!(app.rewind.avail, Avail::Empty);
    win_z(&mut app);
    assert!(!app.rewind.open && app.desk.notice.as_ref().unwrap().0.contains("noch kein Stand"));
    let l = sent(&mut app);
    app.rewind_msg(Msg::Failed(l[0].0, "absent".into()));
    assert_eq!(app.rewind.avail, Avail::Absent);
    assert!(app.rewind_hot().starts_with("rewind at="), "{}", app.rewind_hot());
    app.rewind_probe("test");
    listed(&mut app);
    assert_eq!(app.rewind.avail, Avail::Ready(3));
    win_z(&mut app);
    assert!(app.rewind.open, "with snapshots Win+Z opens it");
    assert!(!app.lib.open, "Win+Z is no Win alone");
    win_z(&mut app);
    assert!(!app.rewind.open, "Win+Z again closes it");
}

#[test]
fn the_time_line_is_newest_first_and_the_keys_walk_it() {
    let (_d, mut app) = opened();
    let hot = app.rewind_hot();
    assert!(hot.contains("rewind chip 0 id=30 pick=true") && hot.contains("rewind chip 2 id=10 pick=false"), "{hot}");
    for w in ["way path", "way program", "way all", "back", "now", "older", "newer"] {
        assert!(hot.contains(&format!("rewind {w}")), "{w} missing: {hot}");
    }
    key(&mut app, KEY_LEFT, Physical);
    assert_eq!(app.rewind.chosen().map(|s| s.id), Some(20), "Left goes back in time");
    click(&mut app, Hot::Chip(2), Injected);
    assert_eq!(app.rewind.chosen().map(|s| s.id), Some(10), "injected input may choose");
    click(&mut app, Hot::Newer, Physical);
    assert_eq!(app.rewind.chosen().map(|s| s.id), Some(20));
    key(&mut app, KEY_ESC, Injected);
    assert!(!app.rewind.open, "Esc closes, also injected");
}

#[test]
fn a_file_comes_back_only_with_georgs_own_yes_after_the_preview() {
    let (_d, mut app) = opened();
    click(&mut app, Hot::Way(Tab::Path), Physical);
    let l = sent(&mut app);
    assert_eq!((op(&l[0].1), l[0].1.get("path").and_then(Json::as_str), l[0].1.get("snapshot").and_then(Json::as_u64)), ("browse", Some(rewind::home().as_str()), Some(30)));
    let home = rewind::home();
    answer(&mut app, l[0].0, r#"{"ok":true,"entries":[{"name":"Dokumente","kind":"folder","open":true},{"name":"brief.txt","kind":"file","bytes":1200,"open":false}]}"#);
    assert!(app.rewind_hot().contains("rewind row 1 name=\"brief.txt\" pick=false"));
    // Down, Down: the file; Enter on a file asks (the preview first, nothing changed).
    key(&mut app, KEY_DOWN, Physical);
    key(&mut app, KEY_DOWN, Physical);
    assert_eq!(app.rewind.browse.pick, Some(1));
    key(&mut app, KEY_ENTER, Injected);
    let l = sent(&mut app);
    assert_eq!(l.len(), 1);
    let (id, j) = &l[0];
    assert_eq!((op(j), j.get("dry_run").and_then(Json::as_bool), j.get("confirmed")), ("restore-path", Some(true), None), "injected input may ask for the preview");
    assert_eq!(j.get("path").and_then(Json::as_str), Some(format!("{home}/brief.txt").as_str()));
    answer(&mut app, *id, &format!(r#"{{"ok":true,"path":"{home}/brief.txt","then":{{"kind":"file","files":1,"bytes":1200}},"now":{{"kind":"file","files":1,"bytes":1500}}}}"#));
    let (d, ask) = app.rewind.dialog.clone().expect("the dialog shows the preview");
    assert_eq!((d.title.as_str(), d.ok.as_str(), d.on_ok), ("Wirklich zurückholen?", "Zurückholen", false));
    assert!(d.lines.iter().any(|l| l == "Damals: eine Datei, 1,2 kB") && matches!(ask, Ask::Path { snap: 30, .. }));
    // Injected input: Tab cannot move the focus to the yes, Enter cannot give it, a click on it does not count.
    armed(&mut app);
    key(&mut app, KEY_TAB, Injected);
    assert!(!app.rewind.dialog.as_ref().unwrap().0.on_ok, "an injected Tab never prepares the yes");
    armed(&mut app);
    click(&mut app, Hot::DialogOk, Injected);
    assert!(app.rewind.dialog.is_some() && sent(&mut app).is_empty(), "an injected click on the yes sends nothing");
    assert_eq!(app.rewind.msg.as_ref().map(|m| m.0.as_str()), Some(REAL_INPUT));
    // Georg's own yes too soon after the question (or after injected input touched it) does not count either.
    key(&mut app, KEY_TAB, Physical);
    key(&mut app, KEY_ENTER, Physical);
    assert!(app.rewind.dialog.is_some() && sent(&mut app).is_empty(), "a yes before ARM");
    armed(&mut app);
    key(&mut app, KEY_ENTER, Physical);
    assert!(app.rewind.dialog.is_none());
    let l = sent(&mut app);
    let (id, j) = &l[0];
    assert_eq!((op(j), j.get("confirmed").and_then(Json::as_bool), j.get("dry_run")), ("restore-path", Some(true), None));
    assert!(app.rewind.busy.is_some());
    answer(&mut app, *id, &format!(r#"{{"ok":true,"restored":"{home}/brief.txt","before":31}}"#));
    assert_eq!(app.rewind.msg.as_ref().map(|m| m.0.as_str()), Some(format!("Zurückgeholt: {home}/brief.txt. Der Stand davor ist als Nr. 31 gesichert.").as_str()));
    let ops: Vec<String> = sent(&mut app).iter().map(|(_, j)| op(j).to_string()).collect();
    assert!(ops.contains(&"list".to_string()) && ops.contains(&"browse".to_string()), "the list and the folder are read again: {ops:?}");
}

#[test]
fn folders_open_by_a_second_click_and_enter_and_never_above_data() {
    let (_d, mut app) = opened();
    app.rewind_tab(Tab::Path);
    let l = sent(&mut app);
    answer(&mut app, l[0].0, r#"{"ok":true,"entries":[{"name":"Dokumente","kind":"folder","open":true},{"name":"geheim","kind":"folder","open":false}]}"#);
    click(&mut app, Hot::Row(0), Physical);
    assert!(sent(&mut app).is_empty() && app.rewind.browse.pick == Some(0), "the first click chooses");
    click(&mut app, Hot::Row(0), Physical);
    let l = sent(&mut app);
    assert!(l[0].1.get("path").and_then(Json::as_str).unwrap().ends_with("/Dokumente"), "the second opens it");
    answer(&mut app, l[0].0, r#"{"ok":true,"entries":[]}"#);
    assert_eq!(app.rewind_pane(&app.rewind_geo()).rows.len() > 0, true);
    assert_eq!(crate::rewind_app::act_texts(&app.rewind).0, "Diesen Ordner zurückholen");
    click(&mut app, Hot::Up, Physical);
    let l = sent(&mut app);
    assert_eq!(l[0].1.get("path").and_then(Json::as_str), Some(rewind::home().as_str()));
    answer(&mut app, l[0].0, r#"{"ok":true,"entries":[{"name":"geheim","kind":"folder","open":false}]}"#);
    click(&mut app, Hot::Row(0), Physical);
    click(&mut app, Hot::Row(0), Physical);
    assert!(sent(&mut app).is_empty(), "a folder that is not his to look into does not open");
    // At the home folder no "up" (bitssnap restores nothing of his above it), the folder itself still.
    app.rewind.browse.pick = None;
    assert!(!app.rewind_parts().iter().any(|(h, _)| *h == Hot::Up) && crate::rewind_app::act_texts(&app.rewind).0 == "Diesen Ordner zurückholen");
    // At /data there is no "up" and no restore of the folder itself (the whole /data is "Alles").
    app.rewind.browse.path = "/data".into();
    app.rewind.browse.pick = None;
    assert!(!app.rewind_parts().iter().any(|(h, _)| *h == Hot::Up) && crate::rewind_app::act_texts(&app.rewind).0.is_empty());
}

#[test]
fn a_program_and_everything_ask_the_same_way_and_a_refusal_is_said() {
    let (_d, mut app) = opened();
    key(&mut app, KEY_TAB, Physical);
    key(&mut app, KEY_TAB, Physical);
    assert_eq!(app.rewind.tab, Tab::Program, "Tab walks the ways");
    let l = sent(&mut app);
    let (id, j) = l.iter().find(|(_, j)| j.get("programs").is_some()).unwrap();
    assert_eq!((op(j), j.get("programs").and_then(Json::as_bool)), ("browse", Some(true)));
    answer(&mut app, *id, r#"{"ok":true,"programs":[{"name":"brave","version":"1.2","now":"1.3","data":true}]}"#);
    key(&mut app, KEY_DOWN, Physical);
    click(&mut app, Hot::Do, Physical);
    let l = sent(&mut app);
    assert_eq!((op(&l[0].1), l[0].1.get("program").and_then(Json::as_str), l[0].1.get("dry_run").and_then(Json::as_bool)), ("restore-program", Some("brave"), Some(true)));
    answer(&mut app, l[0].0, r#"{"ok":true,"program":"brave","version":"1.2","now":"1.3","data_then":null,"data_now":null}"#);
    assert_eq!(app.rewind.dialog.as_ref().unwrap().0.ok, "Zurücksetzen");
    armed(&mut app);
    click(&mut app, Hot::DialogOk, Physical);
    let l = sent(&mut app);
    assert_eq!(l[0].1.get("confirmed").and_then(Json::as_bool), Some(true));
    // bitssnap refuses (as it would for a confirmation that is not the compositor's): said, nothing claimed.
    answer(&mut app, l[0].0, r#"{"ok":false,"error":"not_confirmer","detail":"x"}"#);
    assert_eq!(app.rewind.msg, Some((rewind::failed("not_confirmer").into(), true)));
    // Everything: the preview names the generation and the date; after the yes the restart and the undo.
    key(&mut app, KEY_TAB, Physical);
    assert_eq!(app.rewind.tab, Tab::All);
    key(&mut app, KEY_ENTER, Physical);
    let l = sent(&mut app);
    let (id, j) = l.iter().find(|(_, j)| op(j) == "restore-system").unwrap();
    assert_eq!((j.get("at").and_then(Json::as_u64), j.get("dry_run").and_then(Json::as_bool)), (Some(1_794_493_200), Some(true)));
    answer(&mut app, *id, r#"{"ok":true,"dry_run":true,"snapshot":30,"snapshot_s":1794493200,"pin":1,"running":2,"next":2}"#);
    let d = app.rewind.dialog.clone().unwrap().0;
    assert!(d.lines.contains(&"System: Generation 1 (läuft jetzt: Generation 2)".to_string()) && d.lines[0].contains("Do 12.11.2026 um 15:20"), "{:?}", d.lines);
    key(&mut app, KEY_ESC, Physical);
    assert!(app.rewind.dialog.is_none() && app.rewind.open && sent(&mut app).is_empty(), "Esc cancels the question, the view stays");
    key(&mut app, KEY_ENTER, Physical);
    let l = sent(&mut app);
    answer(&mut app, l[0].0, r#"{"ok":true,"dry_run":true,"snapshot":30,"snapshot_s":1794493200,"pin":1,"running":2,"next":2}"#);
    armed(&mut app);
    key(&mut app, KEY_TAB, Physical);
    key(&mut app, KEY_ENTER, Physical);
    let l = sent(&mut app);
    assert_eq!((op(&l[0].1), l[0].1.get("confirmed").and_then(Json::as_bool)), ("restore-system", Some(true)));
    answer(&mut app, l[0].0, r#"{"ok":true,"dry_run":false,"snapshot":30,"snapshot_s":1794493200,"pin":1,"running":2,"next":2}"#);
    assert!(app.rewind.set_for_start);
    assert_eq!(crate::rewind_app::act_texts(&app.rewind), ("Jetzt neu starten".into(), "Rückgängig machen".into()));
    // No power socket here: the restart says where to do it instead.
    click(&mut app, Hot::Restart, Physical);
    assert!(app.rewind.dialog.is_none() && app.rewind.msg.as_ref().unwrap().1);
    click(&mut app, Hot::Undo, Physical);
    assert_eq!(app.rewind.dialog.as_ref().map(|(_, a)| a.clone()), Some(Ask::Undo), "the undo asks too (no preview)");
    armed(&mut app);
    key(&mut app, KEY_TAB, Physical);
    key(&mut app, KEY_ENTER, Physical);
    let l = sent(&mut app);
    assert_eq!((op(&l[0].1), l[0].1.get("confirmed").and_then(Json::as_bool)), ("undo", Some(true)));
    answer(&mut app, l[0].0, r#"{"ok":true,"undone":true,"text":"x"}"#);
    assert!(!app.rewind.set_for_start && app.rewind.msg.as_ref().unwrap().0.starts_with("Zurückgenommen"));
}

#[test]
fn a_restart_with_init_there_asks_and_goes_only_by_georgs_hand() {
    let (_d, mut app) = opened();
    let dir = std::env::temp_dir().join(format!("wy-rewind-power-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("power.sock");
    std::fs::write(&sock, b"").unwrap();
    app.sofa.quick.power = Some(sock);
    app.rewind_tab(Tab::All);
    app.rewind.set_for_start = true;
    click(&mut app, Hot::Restart, Injected);
    assert_eq!(app.rewind.dialog.as_ref().map(|(d, a)| (d.title.clone(), a.clone())), Some(("Jetzt neu starten?".into(), Ask::Restart)));
    armed(&mut app);
    click(&mut app, Hot::DialogOk, Injected);
    assert!(app.rewind.dialog.is_some() && app.rewind.busy.is_none(), "an injected yes does not restart");
    click(&mut app, Hot::DialogCancel, Injected);
    assert!(app.rewind.dialog.is_none() && app.rewind.waiting.is_empty(), "injected input may cancel");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn not_over_the_sofa_or_a_dialog_and_the_library_and_the_feld_take_turns() {
    let (_d, mut app) = opened();
    app.library_open("test");
    assert!(app.lib.open && !app.rewind.open, "the library closes the view");
    app.rewind_open("test");
    assert!(app.rewind.open && !app.lib.open, "and the other way round");
    app.feld_open("test");
    assert!(app.feld.open && !app.rewind.open, "the Feld closes it");
    app.rewind_open("test");
    assert!(app.rewind.open && !app.feld.open);
    app.ws_new();
    app.ws_switch(0);
    app.ws_cycle(1);
    assert!(!app.rewind.open, "Win+Tab closes it before the switch");
    app.sofa_switch(crate::sofa::Way::CtlOn);
    win_z(&mut app);
    assert!(!app.rewind.open, "not over the sofa mode's screen");
}

#[test]
fn ctl_says_why_it_is_grey_and_lists_the_view() {
    let (_d, mut app) = new_app();
    assert!(ctl(&mut app, &["rewind", "open"]).unwrap().unwrap_err().contains("unknown"));
    app.rewind_probe("test");
    listed(&mut app);
    let s = ctl(&mut app, &["rewind", "open"]).unwrap().unwrap();
    assert!(s.starts_with("rewind open=true avail=ready:3 pick=0 snap=30 tab=choose"), "{s}");
    assert!(s.contains("snap 1 id=20 time=") && s.contains("kind=manual"), "{s}");
    assert!(ctl(&mut app, &["rewind", "close"]).unwrap().unwrap().starts_with("rewind open=false"));
    assert!(ctl(&mut app, &["rewind", "bogus"]).unwrap().is_err());
    assert!(ctl(&mut app, &["feld"]).is_none());
}

#[test]
fn the_question_names_the_snapshot_asked_about_even_if_the_choice_moved() {
    let (_d, mut app) = opened();
    app.rewind_tab(Tab::Path);
    let l = sent(&mut app);
    answer(&mut app, l[0].0, r#"{"ok":true,"entries":[{"name":"brief.txt","kind":"file","bytes":1200,"open":false}]}"#);
    key(&mut app, KEY_DOWN, Physical);
    key(&mut app, KEY_ENTER, Physical);
    let l = sent(&mut app);
    assert_eq!((op(&l[0].1), l[0].1.get("snapshot").and_then(Json::as_u64)), ("restore-path", Some(30)));
    let preview = format!(r#"{{"ok":true,"path":"{}/brief.txt","then":{{"kind":"file","files":1,"bytes":1200}},"now":null}}"#, rewind::home());
    // While bitssnap is asked, chips, arrows, ways and the list wait (only leaving the view goes).
    click(&mut app, Hot::Chip(2), Injected);
    key(&mut app, KEY_LEFT, Physical);
    key(&mut app, KEY_TAB, Physical);
    assert_eq!((app.rewind.chosen().map(|s| s.id), app.rewind.tab), (Some(30), Tab::Path), "nothing moved while busy");
    assert!(sent(&mut app).is_empty());
    // A new list can still move the choice: the answer then asks nothing.
    app.rewind.pick = 1;
    answer(&mut app, l[0].0, &preview);
    assert!(app.rewind.dialog.is_none() && app.rewind.busy.is_none(), "a preview for another choice asks nothing");
    assert!(app.rewind.msg.as_ref().unwrap().0.starts_with("Die Auswahl hat sich geändert"));
    // Back on 30: the dialog's lines are 30's.
    app.rewind.pick = 0;
    key(&mut app, KEY_ENTER, Physical);
    let l = sent(&mut app);
    answer(&mut app, l[0].0, &preview);
    let d = app.rewind.dialog.clone().unwrap().0;
    assert!(d.lines.iter().any(|x| x.starts_with("Stand von heute um ") && x.ends_with("(automatisch)")), "{:?}", d.lines);
}

#[test]
fn a_rollback_set_elsewhere_is_named_and_its_undo_said_honestly() {
    let (_d, mut app) = opened();
    let dir = std::env::temp_dir().join(format!("wy-rewind-pending-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("power.sock");
    std::fs::write(&sock, b"").unwrap();
    app.sofa.quick.power = Some(sock);
    // `bitssnap rollback 20` (or MCP) set it, not this view.
    app.rewind_probe("test");
    let l = sent(&mut app);
    let snaps = r#"[{"id":30,"created_s":1794493200,"kind":"auto"},{"id":20,"created_s":1794400000,"kind":"manual"}]"#;
    answer(&mut app, l[0].0, &format!(r#"{{"ok":true,"data":"ready","snapshots":{snaps},"utc_offset_min":60,"now_s":{NOW},"rollback_pending":20}}"#));
    app.rewind_tab(Tab::All);
    assert_eq!(crate::rewind_app::act_texts(&app.rewind).0, "Jetzt neu starten");
    click(&mut app, Hot::Restart, Physical);
    let d = app.rewind.dialog.clone().unwrap().0;
    assert!(d.lines.contains(&"Danach sind deine Daten wie in Stand Nr. 20".to_string()) && d.lines.iter().any(|x| x.starts_with("vom ") && x.contains("2026 um ")), "{:?}", d.lines);
    assert!(d.lines.iter().any(|x| x.starts_with("Nicht hier eingestellt")), "the chosen time is not claimed: {:?}", d.lines);
    key(&mut app, KEY_ESC, Physical);
    click(&mut app, Hot::Undo, Physical);
    let d = app.rewind.dialog.clone().unwrap().0;
    assert!(d.lines[0] == "Eingestellt ist Stand Nr. 20" && d.lines.iter().any(|x| x.contains("nur, was hier eingestellt wurde")), "{:?}", d.lines);
    armed(&mut app);
    key(&mut app, KEY_TAB, Physical);
    key(&mut app, KEY_ENTER, Physical);
    let l = sent(&mut app);
    answer(&mut app, l[0].0, r#"{"ok":true,"undone":false,"text":"no restore-at of this start"}"#);
    assert!(app.rewind.msg.as_ref().unwrap().0.starts_with("Das ließ sich hier nicht zurücknehmen"), "{:?}", app.rewind.msg);
    assert_eq!(app.rewind.pending, Some(20), "still set for the next start");
    // This view's own "Alles" names its generation in the restart's question.
    app.rewind.ours = Some((20, Some(1)));
    click(&mut app, Hot::Restart, Physical);
    let d = app.rewind.dialog.clone().unwrap().0;
    assert!(d.lines.contains(&"Das System startet als Generation 1.".to_string()), "{:?}", d.lines);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_way_into_the_sofa_mode_closes_the_view() {
    let (_d, mut app) = opened();
    app.rewind_tab(Tab::All);
    key(&mut app, KEY_ENTER, Physical);
    let l = sent(&mut app);
    answer(&mut app, l.iter().find(|(_, j)| op(j) == "restore-system").unwrap().0, r#"{"ok":true,"dry_run":true,"snapshot":30,"snapshot_s":1794493200,"pin":1,"running":2,"next":2}"#);
    assert!(app.rewind.dialog.is_some());
    app.sofa_switch(crate::sofa::Way::CtlOn);
    assert!(!app.rewind.open && app.rewind.dialog.is_none(), "ctl sofa on: no question left under the sofa screens");
}

#[path = "rewind_paint_tests.rs"]
mod paint;
