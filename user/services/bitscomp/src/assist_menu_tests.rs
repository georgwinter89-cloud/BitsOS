//! The sidebar's model list through bitscomp's real input path: which provider it is for, what it offers by
//! kind, what a pick sends to settingsd (a writer whose queue the test reads), its keys, a click outside.

use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use mcpreg::Json;
use smithay::reexports::wayland_server::Display;

use super::*;
use crate::assist_client::Msg;
use crate::input::InputEvent;
use crate::input::Origin::{Injected, Physical};
use crate::pointer::BTN_LEFT;
use crate::settings::Msg as Setting;
use crate::settings_write::Writer;

const KEY_ESC: u32 = 1;
const KEY_TAB: u32 = 15;
const KEY_ENTER: u32 = 28;
const KEY_DOWN: u32 = 108;

type Queue = Receiver<(u32, Req, Instant)>;

struct Case {
    _d: Display<App>,
    app: App,
    rx: Queue,
    dir: std::path::PathBuf,
}

impl Drop for Case {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn set(app: &mut App, pairs: &[(&str, &str)]) {
    for (k, v) in pairs {
        app.settings_msg(Setting::Changed(k.to_string(), Some(v.to_string())));
    }
}

fn status(app: &mut App, provider: &str, ready: bool, busy: bool) {
    let l = format!(r#"{{"ok":true,"name":"Claude","provider":"{provider}","place":"cloud","ready":{ready},"why":"{}","busy":{busy},"turns":0}}"#, if ready { "" } else { "Kein Schlüssel." });
    app.assist_msg(Msg::Line(Json::parse(&l).unwrap()));
}

/// bitscomp at 2560x1440 with settingsd's writer, an assistant whose status names `provider`, and three
/// providers: a helper `abo` that announces its models, `anth` (Anthropic) and `oai` (OpenAI).
fn case(provider: &str) -> Case {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("bitscomp-menu-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let program = dir.join("claude-abo");
    std::fs::write(&program, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let p = program.display().to_string();
    std::fs::write(dir.join("abo.helper"), format!("bitsos_helper = 2\nname = Abo\npath = {p}\nmodels = fable: Fable, opus: Opus, sonnet: Sonnet, haiku: Haiku\n")).unwrap();
    std::fs::set_permissions(dir.join("abo.helper"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let d = Display::<App>::new().expect("display");
    let mut app = App::new(d.handle(), "wy-menu-test".into(), (2560, 1440));
    let (w, rx) = Writer::for_test();
    (app.settings_writer, app.settings_live) = (Some(w), true);
    app.assist_menu.dir = Some(dir.clone());
    let (client, _lines) = crate::assist_client::Client::detached();
    app.assist_client = Some(client);
    app.assist_msg(Msg::Up);
    app.assist_msg(Msg::Line(Json::parse(r#"{"ok":true,"turns":[]}"#).unwrap()));
    set(
        &mut app,
        &[
            ("ai.providers", "abo, anth, oai"),
            ("ai.provider.abo.kind", "program"),
            ("ai.provider.abo.address", &p),
            ("ai.provider.anth.kind", "anthropic"),
            ("ai.provider.anth.model", "Fable 5.2"),
            ("ai.provider.oai.kind", "openai"),
            ("ai.provider.oai.model", "gpt-x"),
            ("ai.role.chat", provider),
        ],
    );
    status(&mut app, provider, true, false);
    assert!(app.assist_place().is_some(), "the conversation has a place on this screen");
    Case { _d: d, app, rx, dir }
}

/// The `ctl hot` line starting with `head`: its point.
fn point(app: &App, head: &str) -> (f64, f64) {
    let hot = app.assist_hot();
    let l = hot.lines().find(|l| l.starts_with(head)).unwrap_or_else(|| panic!("no {head:?} in {hot}"));
    let at = l.split(" at=").nth(1).unwrap().split(' ').next().unwrap();
    let (x, y) = at.split_once(',').unwrap();
    (x.parse().unwrap(), y.parse().unwrap())
}

fn click(app: &mut App, at: (f64, f64), o: crate::input::Origin) {
    app.handle_input_as(InputEvent::Motion { x: at.0, y: at.1 }, o);
    app.handle_input_as(InputEvent::Button { code: BTN_LEFT, pressed: true }, o);
    app.handle_input_as(InputEvent::Button { code: BTN_LEFT, pressed: false }, o);
}

fn key(app: &mut App, code: u32, o: crate::input::Origin) {
    app.handle_input_as(InputEvent::Key { code, pressed: true }, o);
    app.handle_input_as(InputEvent::Key { code, pressed: false }, o);
}

fn sent(rx: &Queue) -> Vec<(u32, Req)> {
    rx.try_iter().map(|(p, r, _)| (p, r)).collect()
}

fn rows(app: &App) -> Vec<String> {
    app.assist_menu_hot().lines().filter(|l| l.contains(" row ")).map(|l| l.split(" label=").nth(1).unwrap().to_string()).collect()
}

#[test]
fn a_helper_offers_its_announced_models_and_a_click_sets_the_answering_providers_model() {
    let mut c = case("abo");
    let app = &mut c.app;
    let hot = app.assist_menu_hot();
    assert!(hot.starts_with("assist-model at=") && hot.contains(r#"label="Standard" open=false provider=abo"#), "{hot}");
    let (mx, my) = point(app, "assist-model at=");
    let input = point(app, "assist-input");
    assert!(my < input.1, "the control is in the head, above the conversation");
    click(app, (mx, my), Physical);
    assert!(app.assist_menu.open, "a click opens the list");
    assert_eq!(rows(app), [r#""Standard" current=true"#, r#""Fable" current=false"#, r#""Opus" current=false"#, r#""Sonnet" current=false"#, r#""Haiku" current=false"#]);
    let opus = point(app, "assist-model row 2 ");
    click(app, opus, Physical);
    assert!(!app.assist_menu.open, "a pick closes the list");
    assert_eq!(sent(&c.rx), [(PID, Req::set("ai.provider.abo.model", Json::str("opus")))]);
    // settingsd took it (its answer comes before the setting's echo): the line above the input says when it
    // counts, the label follows the setting.
    c.app.page_done(Done { pid: PID, ok: true, key: Some("ai.provider.abo.model".into()), text: "Geändert".into(), ask: None });
    set(&mut c.app, &[("ai.provider.abo.model", "opus")]);
    assert!(c.app.assist_menu_hot().contains(r#"label="Opus""#));
    assert_eq!(c.app.assist.note, Some(("Modell: Opus. Gilt ab der nächsten Frage.".into(), false)));
    // The model set now again: nothing is sent.
    c.app.assist_menu_toggle("test");
    c.app.assist_menu_pick(2, Physical);
    assert!(sent(&c.rx).is_empty());
    // A refusal says so.
    c.app.page_done(Done { pid: PID, ok: false, key: Some("ai.provider.abo.model".into()), text: "zu lang".into(), ask: None });
    assert_eq!(c.app.assist.note, Some(("Modell nicht geändert: zu lang".into(), true)));
}

#[test]
fn the_kinds_offer_their_own_lists_and_a_hand_typed_model_stays_visible() {
    let mut c = case("anth");
    c.app.assist_menu_toggle("test");
    assert_eq!(rows(&c.app)[..2], [r#""Standard" current=false"#.to_string(), r#""Fable 5.2" current=false"#.to_string()]);
    assert_eq!(rows(&c.app).last().unwrap(), r#""Fable 5.2" current=true"#, "the hand-typed value, as it stands");
    assert!(c.app.assist_menu_hot().contains(r#"value="claude-opus-5-5" label="Opus 5.5""#));
    // The fallback that answers (the status says which): its list.
    status(&mut c.app, "oai", true, false);
    assert!(c.app.assist_menu_hot().contains("provider=oai"));
    assert_eq!(rows(&c.app), [r#""Standard" current=false"#, r#""gpt-x" current=true"#, &format!("{:?} current=false", crate::assist_menu::MORE)]);
    assert!(c.app.assist_menu_hot().contains("value=more"));
}

#[test]
fn keys_while_open_injected_input_may_pick_and_a_click_outside_closes() {
    let mut c = case("abo");
    let app = &mut c.app;
    // `ctl assist model open` and the keys of `ctl key` are injected input: allowed, as on the page "KI".
    app.ctl_origin = Injected;
    assert!(app.assist_menu_ctl(&["open"]).unwrap().contains("open=true provider=abo"));
    key(app, KEY_DOWN, Injected);
    key(app, KEY_DOWN, Injected);
    key(app, KEY_ENTER, Injected);
    assert_eq!(sent(&c.rx), [(PID, Req::set("ai.provider.abo.model", Json::str("opus")))]);
    assert!(!c.app.assist_menu.open);
    c.app.assist_menu_toggle("test");
    key(&mut c.app, KEY_ESC, Physical);
    assert!(!c.app.assist_menu.open, "Esc closes");
    c.app.assist_menu_toggle("test");
    key(&mut c.app, KEY_TAB, Physical);
    assert!(!c.app.assist_menu.open, "Tab closes");
    c.app.assist_menu_toggle("test");
    click(&mut c.app, (300.0, 1300.0), Physical);
    assert!(!c.app.assist_menu.open, "a click outside closes");
    assert!(sent(&c.rx).is_empty(), "nothing was sent by closing");
    assert!(c.app.assist_menu_ctl(&["pick", "9"]).is_err());
    c.app.assist_menu_ctl(&["pick", "4"]).unwrap();
    assert_eq!(sent(&c.rx), [(PID, Req::set("ai.provider.abo.model", Json::str("haiku")))]);
}

#[test]
fn while_an_answer_runs_a_pick_goes_to_the_settings_for_the_next_question() {
    let mut c = case("abo");
    let asked = c.app.assist.ask("Erzähl.", crate::pacer::now(), false);
    c.app.assist_sync(asked);
    let id = c.app.assist.ask_id();
    assert!(id.is_some() && c.app.assist_menu_hot().starts_with("assist-model"), "the list is there while it answers");
    c.app.assist_menu_toggle("test");
    c.app.assist_menu_pick(1, Physical);
    assert_eq!(sent(&c.rx), [(PID, Req::set("ai.provider.abo.model", Json::str("fable")))]);
    assert_eq!(c.app.assist.ask_id(), id, "the running answer goes on");
}

#[test]
fn not_shown_without_a_ready_provider_or_the_settings_service() {
    let mut c = case("abo");
    c.app.assist_menu_toggle("test");
    status(&mut c.app, "abo", false, false);
    assert!(c.app.assist_menu_hot().is_empty() && !c.app.assist_menu.open, "not ready: no list, an open one closes");
    status(&mut c.app, "abo", true, false);
    assert!(!c.app.assist_menu_hot().is_empty());
    status(&mut c.app, "fremd", true, false);
    assert!(c.app.assist_menu_hot().is_empty(), "a provider the settings do not list");
    status(&mut c.app, "abo", true, false);
    c.app.settings_msg(Setting::Lost);
    assert!(c.app.assist_menu_hot().is_empty(), "no settings service: nothing could be changed");
}
