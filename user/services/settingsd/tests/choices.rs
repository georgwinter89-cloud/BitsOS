//! CL-8: every key whose valid values form a fixed set is a choice in the schema, with German names, so the
//! settings page (WY-9) shows pills; values stored before keep working.

mod common;
use common::*;
use settingsd::builtin;
use settingsd::json::{self, Json};
use settingsd::value::{LEVELS, LEVELS_DE, Type};

fn strs(j: Option<&Json>) -> Vec<String> {
    match j {
        Some(Json::Arr(a)) => a.iter().map(|x| x.as_str().unwrap().to_string()).collect(),
        other => panic!("not an array: {other:?}"),
    }
}

/// The keys that stay free text: a font name, a list of programs, a name. Anything else that is text is a set.
const FREE_TEXT: &[&str] = &["theme.font.display", "theme.font.ui", "theme.font.mono", "desktop.favourites", "claude.name"];

#[test]
fn every_fixed_set_key_is_a_choice_with_german_names() {
    for d in builtin::all() {
        let schema = d.describe(None).get("schema").cloned().unwrap();
        let ty = schema.str("type").unwrap().to_string();
        match ty.as_str() {
            "choice" => {
                let (c, de) = (strs(schema.get("choices")), strs(schema.get("choices_de")));
                assert_eq!(c.len(), de.len(), "{}: one German name per choice", d.name);
                assert!(de.iter().all(|n| !n.trim().is_empty()), "{}", d.name);
                assert!(c.contains(&d.default.show()), "{}: default is a choice", d.name);
            }
            "text" => assert!(FREE_TEXT.contains(&d.name.as_str()), "{} is text: is it a fixed set?", d.name),
            "bool" | "int" | "color" => {}
            other => panic!("{}: type {other} (no logspec field on the page any more)", d.name),
        }
    }
    for k in [
        "kernel.loglevel",
        "kernel.console_log",
        "kernel.console_font",
        "kernel.snd_output",
        "settings.language",
        "theme.mode",
        "theme.motion",
        "theme.effects",
        "theme.layout.address_bar",
    ] {
        let d = builtin::all().into_iter().find(|d| d.name == k).unwrap();
        assert_eq!(d.describe(None).get("schema").and_then(|s| s.str("type")), Some("choice"), "{k}");
    }
}

#[test]
fn loglevel_is_a_choice_of_the_five_levels() {
    let env = Env::new("loglevel_schema", true);
    let (r, _) = env.open();
    let k = r.describe("kernel.loglevel").unwrap().get("key").cloned().unwrap();
    let s = k.get("schema").unwrap();
    assert_eq!(s.str("type"), Some("choice"));
    assert_eq!(strs(s.get("choices")), LEVELS);
    assert_eq!(strs(s.get("choices_de")), LEVELS_DE);
    assert_eq!(s.str("grammar"), Some("logspec"), "a client may still send warn,net=debug");
    assert_eq!(k.get("value"), Some(&s_("info")));
    let list = r.list("kernel.loglevel");
    let Some(Json::Arr(keys)) = list.get("keys") else { panic!("{list:?}") };
    assert_eq!(keys[0].str("type"), Some("choice"));
    // console_log names its levels the same way, plus its own off and all.
    let c = r.describe("kernel.console_log").unwrap();
    let de = strs(c.get("key").and_then(|k| k.get("schema")).and_then(|s| s.get("choices_de")));
    assert_eq!(de, ["Aus", "Nur Fehler", "Warnungen", "Normal", "Ausführlich", "Alles"]);
}

fn s_(v: &str) -> Json {
    Json::from(v)
}

#[test]
fn schemas_read_back_and_bad_names_are_refused() {
    for d in builtin::all() {
        let schema = d.describe(None).get("schema").cloned().unwrap();
        assert_eq!(Type::from_json(&schema).unwrap(), d.ty, "{}", d.name);
    }
    let p = |t: &str| Type::from_json(&json::parse(t).unwrap());
    assert!(p(r#"{"type":"choice","choices":["x","y"]}"#).is_ok(), "German names are optional for a service");
    assert!(p(r#"{"type":"logspec"}"#).is_ok(), "the pre-CL-8 name still registers");
    for bad in [
        r#"{"type":"choice","choices":["x","y"],"choices_de":["Iks"]}"#,
        r#"{"type":"choice","choices":["x"],"choices_de":["a\nb"]}"#,
        r#"{"type":"choice","choices":["x"],"choices_de":[" "]}"#,
        r#"{"type":"choice","choices":["x"],"choices_de":[]}"#,
        r#"{"type":"choice","choices":["x"],"choices_de":[1]}"#,
        r#"{"type":"choice","choices":[]}"#,
        r#"{"type":"choice","choices":["x","x"]}"#,
        r#"{"type":"choice","choices":["A","b","a"]}"#,
    ] {
        assert!(p(bad).is_err(), "{bad}");
    }
}

#[test]
fn values_stored_before_read_as_the_choice() {
    let env = Env::new("old_values", true);
    std::fs::create_dir_all(env.paths.stick.join("settings")).unwrap();
    std::fs::write(env.paths.stick.join("CONFIG.TXT"), "loglevel=WARN\nconsole_log=Debug\nsnd_output=HDMI\n").unwrap();
    std::fs::write(env.paths.stick.join("settings/theme.toml"), "theme.mode = \"Light\"\n").unwrap();
    let (r, _) = env.open();
    assert_eq!(value(&r, "kernel.loglevel"), s("warn"));
    assert_eq!(value(&r, "kernel.console_log"), s("debug"));
    assert_eq!(value(&r, "kernel.snd_output"), s("hdmi"));
    assert_eq!(value(&r, "theme.mode"), s("light"));
    drop(r);
    // A spec with subsystem parts (set from the CLI or a test script) stays as it was: nothing a user set is lost.
    std::fs::write(env.paths.stick.join("CONFIG.TXT"), "loglevel=warn,syscall=info\n").unwrap();
    let (mut r, ev) = env.open();
    assert_eq!(value(&r, "kernel.loglevel"), s("warn,syscall=info"));
    assert!(!ev.lock().unwrap().iter().any(|l| l.contains("value.bad")), "{:?}", ev.lock().unwrap());
    // Choosing a pill replaces it; any case is the level; a bad level is still refused.
    std::fs::write(env.paths.sys.join("loglevel"), "warn,syscall=info\n").unwrap();
    assert_eq!(r.set("kernel.loglevel", &s("Trace"), &WHO).unwrap().get("new"), Some(&s("trace")));
    assert!(env.stick_file("CONFIG.TXT").contains("loglevel=trace\n"));
    assert_eq!(r.set("kernel.loglevel", &s("warn, net=DEBUG"), &WHO).unwrap().get("new"), Some(&s("warn,net=debug")));
    assert_eq!(r.set("kernel.loglevel", &s("loud"), &WHO).unwrap_err().code, "bad_value");
}

#[test]
fn hostile_stored_loglevels_fall_back_and_a_pill_drops_the_spec() {
    let env = Env::new("hostile_loglevel", true);
    std::fs::create_dir_all(env.paths.stick.join("settings")).unwrap();
    for bad in ["loglevel=\n", "loglevel=warn,=debug\n", "loglevel=TRACE,net=bogus\n", "loglevel=,\n"] {
        std::fs::write(env.paths.stick.join("CONFIG.TXT"), bad).unwrap();
        let (r, ev) = env.open();
        assert_eq!(value(&r, "kernel.loglevel"), s("info"), "{bad:?}");
        let ev = ev.lock().unwrap();
        assert!(ev.iter().any(|l| l.contains("value.bad") && l.contains("kernel.loglevel")), "{bad:?}: {ev:?}");
    }
    // A stored spec keeps its overrides; choosing a pill stores just the level and the overrides are gone.
    std::fs::write(env.paths.stick.join("CONFIG.TXT"), "loglevel=warn,syscall=info,net=TRACE\n").unwrap();
    let (mut r, ev) = env.open();
    assert_eq!(value(&r, "kernel.loglevel"), s("warn,syscall=info,net=trace"));
    assert!(!ev.lock().unwrap().iter().any(|l| l.contains("value.bad")), "{:?}", ev.lock().unwrap());
    std::fs::write(env.paths.sys.join("loglevel"), "warn,syscall=info,net=trace\n").unwrap();
    assert_eq!(r.set("kernel.loglevel", &s("debug"), &WHO).unwrap().get("new"), Some(&s("debug")));
    let config = env.stick_file("CONFIG.TXT");
    assert!(config.contains("loglevel=debug\n") && !config.contains("syscall="), "{config}");
    assert_eq!(value(&r, "kernel.loglevel"), s("debug"));
}
