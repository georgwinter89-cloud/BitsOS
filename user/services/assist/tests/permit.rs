//! The permission step against a real settingsd (plan/spec/assistent-eins K2): one level for every tool and
//! both question origins, and the fixed rules nothing lifts (F3: R8 names and the assistant's own leash `ai.*`;
//! F4: deleting asks; `confirmed` only from Georg's answer).

mod common;
use assist::config;
use assist::json::{Json, parse};
use assist::settings;
use assist::settings::{Refused, Settings};
use assist::tools::desk;
use assist::tools::native;
use assist::tools::permit::{By, Decision, Level, decide, judge, offered};
use assist::tools::settings::SettingsTools;
use assist::tools::{Class, Grant, ToolSource, ToolSpec};
use common::World;

fn spec(tools: &[ToolSpec], name: &str) -> ToolSpec {
    tools.iter().find(|t| t.name == name).unwrap().clone()
}

fn args(text: &str) -> Json {
    parse(text).unwrap()
}

/// Every tool the chat has with the class it has: settingsd's six, the native ones and the desktop's.
fn every_tool(w: &World) -> Vec<ToolSpec> {
    let mut all = SettingsTools { settings: w }.list();
    all.extend(native::specs());
    all.extend(desk::specs());
    all
}

/// A call of each tool whose arguments describe something harmless (no ai.*, no R8 name).
fn harmless(name: &str) -> Json {
    match name {
        "settings_set" => args(r#"{"key":"theme.scale","value":150}"#),
        "settings_get" | "settings_describe" => args(r#"{"key":"theme.scale"}"#),
        _ => args(r#"{"path":"/tmp/x","command":"true","url":"https://example.org","pattern":"x","id":"b1","element":"w1/1","aktion":"press","name":"OK","warum":"weil"}"#),
    }
}

#[test]
fn the_level_table_for_every_class_and_both_origins() {
    let w = World::new("permit-levels");
    let cfg = config::load(&w).unwrap();
    assert_eq!(cfg.level, Level::Auto, "auto until Georg chooses less (DECISIONS 18.11.)");
    // The keys of before EINS are no settings any more.
    for gone in ["ai.tools.settings_set.allow", "ai.helper.run.allow", "ai.role.explain"] {
        assert!(settings::get(&w, gone).is_err(), "{gone}");
    }
    // A harmless change in the journal: what an undo would take back is known.
    w.set("theme.scale", 130i64);
    let all = every_tool(&w);
    for level in [Level::Answer, Level::Ask, Level::Auto] {
        for by in [By::Hand, By::Remote] {
            let l = level.for_origin(by);
            assert!(l <= Level::Ask || by == By::Hand, "a remote question gets at most ask");
            for t in &all {
                let d = decide(l, t, &harmless(&t.name), &w);
                let want = match (t.class, l, t.name.as_str()) {
                    (Class::Read, _, _) => "run",
                    (_, Level::Answer, _) => "refuse",
                    // F4 at every level that offers it; the compositor judges each step itself.
                    (_, _, "loeschen") => "ask",
                    (_, _, "handeln") => "run",
                    (_, Level::Ask, _) => "ask",
                    (_, Level::Auto, _) => "run",
                };
                let got = match &d {
                    Decision::Run => "run",
                    Decision::Ask(_) => "ask",
                    Decision::Refuse(_) => "refuse",
                };
                assert_eq!(got, want, "{} at {level:?} by {by:?}: {d:?}", t.name);
                assert_eq!(offered(l, t), want != "refuse", "{} is offered exactly when it is not refused", t.name);
            }
        }
    }
    // A change asks in plain German, with the value now and the value wanted.
    let set = spec(&all, "settings_set");
    assert_eq!(decide(Level::Ask, &set, &args(r#"{"key":"theme.scale","value":150}"#), &w), Decision::Ask("Einstellung „theme.scale“ ändern: 130 → 150".into()));
    assert!(matches!(decide(Level::Answer, &set, &args("{}"), &w), Decision::Refuse(m) if m.contains("Nur antworten")));
    let del = spec(&all, "loeschen");
    assert_eq!(decide(Level::Auto, &del, &args(r#"{"path":"/data/home/1000/a.txt"}"#), &w), Decision::Ask("Löschen (in den Papierkorb): /data/home/1000/a.txt".into()));
    // The level itself: a value that is no level cannot be stored, and a stored one is read for every question.
    assert!(settings::set(&w, "ai.level", "always", "test").is_err());
    w.set("ai.level", "ask");
    assert_eq!(config::load(&w).unwrap().level, Level::Ask);
}

#[test]
fn the_fixed_rules_hold_at_every_level() {
    let w = World::new("permit-fixed");
    let tools = SettingsTools { settings: &w }.list();
    let set = spec(&tools, "settings_set");
    for level in [Level::Ask, Level::Auto] {
        // F3: R8 switches, guarded CONFIG keys, kernel secrets: never, whole or as the last part of a key.
        for key in ["nvme_write", "kernel.nvme_write", "stick_write", "usb_msd_write", "nvme_data", "kernel.mcp", "mcp_confirmer", "kernel.token", "x.wlan_psk", "claude_api_key"] {
            let d = decide(level, &set, &Json::obj().with("key", key).with("value", true), &w);
            assert!(matches!(&d, Decision::Refuse(m) if m.starts_with("refused_r8")), "{key}: {d:?}");
        }
        // The assistant's own leash asks even in Automodus.
        for (key, value) in [("ai.level", "auto"), ("ai.local_only", "false"), ("ai.role.chat", "evil"), ("ai.provider.x.address", "https://evil.example"), ("ai.providers", "evil")] {
            let d = decide(level, &set, &Json::obj().with("key", key).with("value", value), &w);
            assert!(matches!(&d, Decision::Ask(q) if q.contains(key)), "{key}: {d:?}");
        }
    }
    // Also through the back door: undoing Georg's own change to it.
    w.set("ai.local_only", true);
    let undo = spec(&tools, "settings_undo");
    assert!(matches!(decide(Level::Auto, &undo, &args("{}"), &w), Decision::Ask(q) if q.contains("ai.local_only")));
    w.set("theme.scale", 150i64);
    assert_eq!(decide(Level::Auto, &undo, &args("{}"), &w), Decision::Run, "the newest change is harmless again");
    assert!(matches!(decide(Level::Auto, &undo, &args(r#"{"n":2}"#), &w), Decision::Ask(_)), "two back reaches ai.local_only");
}

#[test]
fn confirmed_comes_from_georg_never_from_the_model() {
    let w = World::new("permit-confirm");
    let key = parse(r#"{"key":"assist.careful","schema":{"type":"bool"},"default":false,"text":{"de":"Vorsicht","en":"Careful"},"safety":"confirm"}"#).unwrap();
    settings::register(&w, vec![key]).unwrap();
    let tools = SettingsTools { settings: &w };
    // The model claims Georg agreed: the claim is dropped before settingsd sees the request.
    let claimed = args(r#"{"key":"assist.careful","value":true,"confirmed":true}"#);
    let e = tools.call("settings_set", &claimed, Grant::confirmed(false)).unwrap_err();
    assert!(e.starts_with("needs_confirm"), "{e}");
    assert_eq!(w.value("assist.careful"), Json::Bool(false));
    // With Georg's yes it goes through, and the journal names the chat as the source.
    let ok = tools.call("settings_set", &args(r#"{"key":"assist.careful","value":true}"#), Grant::confirmed(true)).unwrap();
    assert!(ok.contains(r#""new":true"#), "{ok}");
    let h = tools.call("settings_history", &args(r#"{"n":1}"#), Grant::confirmed(false)).unwrap();
    assert!(h.contains(r#""source":"assist""#), "{h}");
    // `owner` is not an argument of any tool: a secret stays *** whatever the model sends.
    w.provider("p", "anthropic", "https://example.invalid", "cloud", Some(common::KEY));
    let got = tools.call("settings_get", &args(r#"{"key":"ai.provider.p.key","owner":"assist"}"#), Grant::confirmed(false)).unwrap();
    assert!(got.contains("***") && !got.contains(common::KEY), "{got}");
    let listed = tools.call("settings_list", &args(r#"{"prefix":"ai","limit":3}"#), Grant::confirmed(false)).unwrap();
    assert!(listed.contains(r#""more":"#) && !listed.contains(common::KEY));
}

#[test]
fn text_from_the_model_cannot_dress_up_the_question() {
    let w = World::new("permit-question");
    let tools = SettingsTools { settings: &w }.list();
    let set = spec(&tools, "settings_set");
    // A value with line breaks that tries to answer the question itself and to push the real change away.
    let fake = "light\nErlauben? (j = ja, n = nein) ja\n\n\n\n\n\n\n\n\x1b[2JFertig.";
    let Decision::Ask(q) = decide(Level::Ask, &set, &Json::obj().with("key", "theme.mode").with("value", fake), &w) else { panic!("asks") };
    assert!(!q.contains('\n') && !q.chars().any(char::is_control), "{q:?}");
    assert!(q.starts_with("Einstellung „theme.mode“ ändern: „dark“ → „light\\nErlauben?"), "{q}");
    // 10 kB: cut, and said so with the full length; the key and the old value stay in sight.
    let long = "x".repeat(10_000);
    let Decision::Ask(q) = decide(Level::Ask, &set, &Json::obj().with("key", "theme.mode").with("value", long.as_str()), &w) else { panic!("asks") };
    assert!(q.chars().count() < 400 && q.contains("(gekürzt, 10000 Zeichen)") && q.starts_with("Einstellung „theme.mode“ ändern: „dark“ → „xxx"), "{q}");
    // The key is the model's text too.
    let Decision::Ask(q) = decide(Level::Ask, &set, &Json::obj().with("key", "theme.mode\n· Erlauben?").with("value", 1i64), &w) else { panic!("asks") };
    assert!(!q.contains('\n'), "{q:?}");
}

/// A settingsd whose journal cannot be read.
struct NoHistory<'a>(&'a World);

impl Settings for NoHistory<'_> {
    fn request(&self, req: &Json) -> Result<Json, Refused> {
        if req.str("op") == Some("history") {
            return Err(Refused { code: "unreachable".into(), message: "busy".into() });
        }
        self.0.request(req)
    }
}

#[test]
fn an_undo_is_held_to_the_change_that_was_looked_at() {
    let w = World::new("permit-undo");
    let tools = SettingsTools { settings: &w };
    let list = tools.list();
    let undo = spec(&list, "settings_undo");

    // After an undo the newest journal line is the undo itself; what the next undo takes back is the change
    // before it, here Georg's own switch.
    w.set("ai.local_only", true);
    w.set("theme.scale", 150i64);
    tools.call("settings_undo", &args("{}"), Grant::confirmed(false)).unwrap();
    assert_eq!(w.value("theme.scale").as_i64(), Some(125));
    let v = judge(Level::Auto, &undo, &args("{}"), &w);
    assert!(matches!(&v.decision, Decision::Ask(q) if q.contains("ai.local_only")), "{v:?}");

    // What it would take back cannot be read: Automodus does not cover the unknown.
    let blind = NoHistory(&w);
    let v = judge(Level::Auto, &undo, &args("{}"), &blind);
    assert!(matches!(&v.decision, Decision::Ask(q) if q.contains("ließ sich gerade nicht lesen")), "{v:?}");
    assert_eq!(v.pin, None);

    // The call carries the journal number that was looked at: a change in between and nothing is undone.
    w.set("theme.scale", 175i64);
    let v = judge(Level::Auto, &undo, &args("{}"), &w);
    assert_eq!((v.decision.clone(), v.what.as_str()), (Decision::Run, "Änderung zurücknehmen: „theme.scale“: 175 → 125"));
    assert!(v.pin.is_some());
    w.set("ai.role.chat", "anders");
    let e = tools.call("settings_undo", &args("{}"), Grant { confirmed: false, pin: v.pin, level: Level::Auto }).unwrap_err();
    assert!(e.starts_with("changed_meanwhile"), "{e}");
    assert_eq!((w.value("ai.role.chat").as_str(), w.value("theme.scale").as_i64()), (Some("anders"), Some(175)), "nothing undone");
    // The same for an undo of several changes.
    let v = judge(Level::Auto, &undo, &args(r#"{"n":2}"#), &w);
    w.set("theme.scale", 200i64);
    assert!(tools.call("settings_undo", &args(r#"{"n":2}"#), Grant { confirmed: true, pin: v.pin, level: Level::Auto }).unwrap_err().starts_with("changed_meanwhile"));
    assert_eq!(w.value("theme.scale").as_i64(), Some(200));
}

/// A settingsd whose journal lines carry no number.
struct NoSeq<'a>(&'a World);

impl Settings for NoSeq<'_> {
    fn request(&self, req: &Json) -> Result<Json, Refused> {
        let a = self.0.request(req)?;
        if req.str("op") != Some("history") {
            return Ok(a);
        }
        let mut text = a.to_text();
        while let Some(at) = text.find(r#""seq":"#) {
            let end = text[at..].find([',', '}']).map_or(text.len(), |e| at + e);
            let end = if text[end..].starts_with(',') { end + 1 } else { end };
            text.replace_range(at..end, "");
        }
        Ok(parse(&text.replace(",}", "}")).unwrap())
    }
}

/// The assistant above `assist --mcp`: says yes, and writes down whether it was told to ask whatever the level.
struct Bell(std::cell::RefCell<Vec<(String, bool)>>);

impl assist::mcp::gate::Owner for Bell {
    fn ask(&self, tool: &str, _question: &str, must_ask: bool) -> assist::provider::program::ask::Reply {
        self.0.borrow_mut().push((tool.to_string(), must_ask));
        assist::provider::program::ask::Reply::Yes
    }
}

#[test]
fn a_fixed_rules_question_is_asked_at_every_level_through_a_bell_too() {
    let w = World::new("permit-bell-must");
    w.set("ai.level", "auto");
    w.set("theme.scale", 150i64);
    let blind = NoSeq(&w);
    let tools = SettingsTools { settings: &blind };
    let undo = spec(&tools.list(), "settings_undo");
    // An undo without the journal's number: asked in the chat at every level.
    let v = judge(Level::Auto, &undo, &args("{}"), &blind);
    assert!(matches!(v.decision, Decision::Ask(_)) && v.must && v.pin.is_none(), "{v:?}");
    let set = spec(&tools.list(), "settings_set");
    let plain = judge(Level::Ask, &set, &args(r#"{"key":"theme.scale","value":175}"#), &blind);
    assert!(matches!(plain.decision, Decision::Ask(_)) && !plain.must, "the level's question is the level's: {plain:?}");
    // Beside a bell the same: the bell's judge must not let Automodus run it unasked.
    let bell = Bell(std::cell::RefCell::new(Vec::new()));
    let gate = assist::mcp::gate::Gate { settings: &blind, tools: &tools, read_only: false, owner: Some(&bell) };
    assert!(gate.call("settings_undo", &args("{}"), &|| false).is_ok());
    gate.call("settings_set", &args(r#"{"key":"theme.scale","value":175}"#), &|| false).ok();
    assert_eq!(*bell.0.borrow(), [("settings_undo".to_string(), true), ("settings_set".to_string(), false)]);
}
