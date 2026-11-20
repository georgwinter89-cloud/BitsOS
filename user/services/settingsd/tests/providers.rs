//! CL-5s: settingsd makes the provider keys itself from the list `ai.providers`, a name taken off the list
//! removes the provider with its key in one confirmed step, the role keys are typed, and `claude.name` moves
//! to `ai.assistant.name` once.

mod common;
use common::*;
use settingsd::change::Who;
use settingsd::json::{Json, parse};
use settingsd::key::KeyDef;
use settingsd::peer::Peer;
use settingsd::proto::{self, Reply};
use settingsd::registry::Registry;
use settingsd::server::Shared;
use std::sync::{Arc, Mutex};

/// A value no answer to a stranger, no event and no journal may hold.
const KEY: &str = "sk-cl5s-NEVER-SHOWN-9d2e";
const HOME: &str = "https://api.example.com";
const FIELDS: [&str; 5] = ["kind", "address", "model", "key", "place"];

fn stranger() -> Peer {
    Peer::program(1000, "/data/home/1000/dl/game")
}

fn yes() -> Who<'static> {
    Who { source: "test", confirmed: true, asker: None }
}

fn ask(shared: &Arc<Shared>, peer: &Peer, line: &str) -> Json {
    match proto::handle(shared, peer, line) {
        Reply::Line(l) => parse(&l).unwrap(),
        Reply::Watch(_) => panic!("watch"),
    }
}

fn clear(r: &Registry, key: &str, p: &Peer) -> Result<Json, &'static str> {
    r.get(key, Some("assist"), p).map(|j| j.get("value").cloned().unwrap()).map_err(|e| e.code)
}

/// Provider `id` on the list, set up at [`HOME`], its key stored.
fn add(r: &mut Registry, id: &str) {
    let mut ids = settingsd::providers::ids(value(r, "ai.providers").as_str().unwrap());
    ids.push(id.to_string());
    r.set("ai.providers", &s(&ids.join(", ")), &WHO).unwrap();
    for (f, v) in [("kind", "anthropic"), ("address", HOME), ("place", "cloud"), ("model", "m1"), ("key", KEY)] {
        r.set(&format!("ai.provider.{id}.{f}"), &s(v), &WHO).unwrap();
    }
}

fn with_claude(env: &Env) -> (Registry, Arc<Mutex<Vec<String>>>) {
    let (mut r, ev) = env.open();
    add(&mut r, "claude");
    (r, ev)
}

#[test]
fn a_name_on_the_list_makes_the_five_keys() {
    let env = Env::new("prov_make", true);
    let (mut r, ev) = env.open();
    assert_eq!(r.get("ai.provider.claude.kind", None, &stranger()).unwrap_err().code, "unknown_key");
    let (rx, _) = r.watch("ai.provider");
    r.set("ai.providers", &s("claude"), &who_is(&stranger())).unwrap();
    // The page hears that rows came and reads them again.
    let told = parse(&rx.try_recv().unwrap()).unwrap();
    assert_eq!((told.str("event"), told.str("owner")), (Some("registered"), Some("assist")));
    for f in FIELDS {
        let d = r.describe(&format!("ai.provider.claude.{f}")).unwrap().get("key").cloned().unwrap();
        assert_eq!(d.str("owner"), Some("assist"), "{f}");
        assert_eq!(d.str("safety"), Some(if f == "key" { "secret" } else { "normal" }), "{f}");
        let text = d.get("text").unwrap();
        assert!(text.str("de").unwrap().starts_with("Anbieter claude: ") && text.str("en").unwrap().starts_with("Provider claude: "), "{f}");
    }
    assert_eq!(value(&r, "ai.provider.claude.kind"), s("unset"));
    assert_eq!(value(&r, "ai.provider.claude.place"), s("unset"));
    assert_eq!(r.set("ai.provider.claude.place", &s("moon"), &WHO).unwrap_err().code, "bad_value");
    // The secret is the system's assist's without any registration: it reads, nobody else does.
    r.set("ai.provider.claude.key", &s(KEY), &WHO).unwrap();
    assert_eq!(clear(&r, "ai.provider.claude.key", &prog("assist")), Ok(s(KEY)));
    assert_eq!(clear(&r, "ai.provider.claude.key", &Peer::program(0, &format!("{BIN}/assist"))), Ok(s(KEY)));
    for p in [stranger(), prog("settings"), Peer::program(1000, "/data/home/1000/assist")] {
        assert_eq!(clear(&r, "ai.provider.claude.key", &p), Err("not_owner"), "{p:?}");
    }
    assert_eq!(r.get("ai.provider.claude.key", Some("settingsd"), &prog("assist")).unwrap_err().code, "not_owner");
    assert_eq!(value(&r, "ai.provider.claude.key"), s("***"));
    assert_eq!(r.describe("ai.provider.claude.key").unwrap().get("key").unwrap().str("owner_program"), Some("/bitsos/bin/assist"));
    assert!(env.stick_file("settings/owners.list").contains("ai.provider.claude.key"));
    // A second name: the first one's keys and values stay as they are.
    r.set("ai.providers", &s("claude, strata"), &WHO).unwrap();
    assert_eq!(value(&r, "ai.provider.strata.address"), s(""));
    assert_eq!(clear(&r, "ai.provider.claude.key", &prog("assist")), Ok(s(KEY)));
    let log = ev.lock().unwrap().join("\n");
    assert!(log.contains("settingsd providers listed=2 keys_made=5") && !log.contains(KEY), "{log}");
}

fn who_is(p: &Peer) -> Who<'_> {
    Who { source: "test", confirmed: false, asker: Some(p) }
}

#[test]
fn the_keys_are_there_from_the_start() {
    let env = Env::new("prov_boot", true);
    let (r, _) = with_claude(&env);
    drop(r);
    // A restart: nobody registers anything, the keys and their values are there.
    let (r, _) = env.open();
    assert_eq!(value(&r, "ai.provider.claude.address"), s(HOME));
    assert_eq!(value(&r, "ai.provider.claude.kind"), s("anthropic"));
    assert_eq!(clear(&r, "ai.provider.claude.key", &prog("assist")), Ok(s(KEY)));
    assert_eq!(clear(&r, "ai.provider.claude.key", &stranger()), Err("not_owner"));
    drop(r);
    // The files of a machine set up before CL-5s (assist registered the keys then), the record lost: the same.
    std::fs::remove_file(env.paths.stick.join("settings/owners.list")).unwrap();
    let (mut r, _) = env.open();
    assert_eq!(clear(&r, "ai.provider.claude.key", &prog("assist")), Ok(s(KEY)));
    assert_eq!(clear(&r, "ai.provider.claude.key", &stranger()), Err("not_owner"));
    // Nobody else gets the keys by registering them, and the stored key still binds the address.
    let j = r#"{"key":"ai.provider.claude.key","schema":{"type":"text","max_bytes":200},"default":"","text":{"de":"S","en":"S"}}"#;
    for (owner, p) in [("game", stranger()), ("assist", stranger()), ("settings", prog("settings"))] {
        let d = KeyDef::from_json(&parse(j).unwrap(), owner).unwrap();
        assert_eq!(r.register(owner, vec![d], &p).unwrap_err().code, "not_owner", "{owner}");
    }
    assert_eq!(r.set("ai.provider.claude.address", &s("https://evil.example.net"), &yes()).unwrap_err().code, "key_bound");
}

#[test]
fn taking_a_name_off_removes_the_provider_in_one_confirmed_step() {
    let env = Env::new("prov_remove", true);
    let (mut r, ev) = with_claude(&env);
    add(&mut r, "strata");
    let (rx, _) = r.watch("ai.provider.claude");
    // Without a yes nothing happens, and the refusal says what would go. Whose yes it must be: tests/vouch.rs.
    for (w, code) in [(WHO, "needs_confirm"), (who_is(&prog("bitscomp")), "needs_confirm"), (who_is(&prog("assist")), "not_confirmer"), (who_is(&stranger()), "not_confirmer")] {
        let e = r.set("ai.providers", &s("strata"), &w).unwrap_err();
        assert_eq!(e.code, code);
        assert!(e.msg.contains("„claude“") && e.msg.contains("Schlüssel"), "{}", e.msg);
    }
    assert_eq!(r.reset("ai.providers", &WHO).unwrap_err().code, "needs_confirm");
    assert_eq!(value(&r, "ai.providers"), s("claude, strata"));
    assert_eq!(clear(&r, "ai.provider.claude.key", &prog("assist")), Ok(s(KEY)));
    assert!(rx.try_recv().is_err());

    let a = r.set("ai.providers", &s("strata"), &yes()).unwrap();
    assert_eq!(a.str("stored"), Some("stick"));
    for f in FIELDS {
        assert_eq!(r.get(&format!("ai.provider.claude.{f}"), None, &prog("assist")).unwrap_err().code, "unknown_key", "{f}");
    }
    let told = parse(&rx.try_recv().unwrap()).unwrap();
    assert_eq!(told.str("event"), Some("unregistered"));
    // Nothing of it is left anywhere: the file, the record, the history.
    let file = env.stick_file("settings/ai.toml");
    assert!(!file.contains("provider.claude") && file.contains("provider.strata.address") && file.contains(KEY), "{file}");
    assert_eq!(file.matches(KEY).count(), 1, "strata's key stays, claude's is gone");
    let owners = env.stick_file("settings/owners.list");
    assert!(!owners.contains("ai.provider.claude") && owners.contains("ai.provider.strata.key"), "{owners}");
    assert!(!env.stick_file("settings/journal.log").contains("ai.provider.claude"));
    assert_eq!(r.history(100, Some("ai.provider.claude")).get("entries"), Some(&Json::Arr(Vec::new())));
    let log = ev.lock().unwrap().join("\n");
    assert!(log.contains("settingsd provider.removed provider=claude key_stored=true source=test") && !log.contains(KEY), "{log}");
    // The other provider is untouched.
    assert_eq!(value(&r, "ai.provider.strata.address"), s(HOME));
    assert_eq!(clear(&r, "ai.provider.strata.key", &prog("assist")), Ok(s(KEY)));

    // An undo brings the name back, not what was removed: a new, empty provider.
    r.undo(1, &WHO).unwrap();
    assert_eq!(value(&r, "ai.providers"), s("claude, strata"));
    assert_eq!(value(&r, "ai.provider.claude.address"), s(""));
    assert_eq!(clear(&r, "ai.provider.claude.key", &prog("assist")), Ok(s("")));
    // A provider nothing is stored for goes without a question.
    r.set("ai.providers", &s("strata"), &WHO).unwrap();
    // A restart finds no trace either.
    drop(r);
    let (mut r, _) = env.open();
    r.set("ai.providers", &s("strata, claude"), &WHO).unwrap();
    assert_eq!(value(&r, "ai.provider.claude.kind"), s("unset"));
    assert_eq!(clear(&r, "ai.provider.claude.key", &prog("assist")), Ok(s("")));
}

/// The page's two requests as they travel: add by the list, remove with `"confirmed": true`. A program that
/// is not assist's writes a key only confirmed, and reads none.
#[test]
fn over_the_socket_protocol() {
    let env = Env::new("prov_proto", true);
    let (r, _) = env.open();
    let shared = Arc::new(Shared::new(r));
    let page = prog("bitscomp");
    assert_eq!(ask(&shared, &page, r#"{"op":"set","key":"ai.providers","value":"lokal","source":"page"}"#).get("ok"), Some(&Json::Bool(true)));
    let rows = ask(&shared, &page, r#"{"op":"list","prefix":"ai.provider.lokal"}"#);
    assert!(matches!(rows.get("keys"), Some(Json::Arr(k)) if k.len() == 5), "{}", rows.to_text());
    let set_key = |confirmed: bool| format!(r#"{{"op":"set","key":"ai.provider.lokal.key","value":"{KEY}","source":"page","confirmed":{confirmed}}}"#);
    assert_eq!(ask(&shared, &page, &set_key(false)).str("error"), Some("not_owner"));
    assert_eq!(ask(&shared, &page, &set_key(true)).get("ok"), Some(&Json::Bool(true)));
    let read = r#"{"op":"get","key":"ai.provider.lokal.key","owner":"assist"}"#;
    assert_eq!(ask(&shared, &page, read).str("error"), Some("not_owner"));
    assert_eq!(ask(&shared, &prog("assist"), read).str("value"), Some(KEY));
    assert_eq!(ask(&shared, &page, r#"{"op":"get","key":"ai.provider.lokal.key"}"#).str("value"), Some("***"));
    // Bad names never become keys.
    for bad in ["Lokal", "a.b", "unset", "x, y z!"] {
        let a = ask(&shared, &page, &format!(r#"{{"op":"set","key":"ai.providers","value":"lokal, {bad}"}}"#));
        assert_eq!(a.str("error"), Some("bad_value"), "{bad}");
        assert!(a.str("message").unwrap().contains("geht nicht"), "{bad}");
    }
    let off = |confirmed: bool| format!(r#"{{"op":"set","key":"ai.providers","value":"","source":"page","confirmed":{confirmed}}}"#);
    let no = ask(&shared, &page, &off(false));
    assert_eq!(no.str("error"), Some("needs_confirm"));
    assert!(!no.to_text().contains(KEY));
    assert_eq!(ask(&shared, &page, &off(true)).get("ok"), Some(&Json::Bool(true)));
    assert_eq!(ask(&shared, &prog("assist"), read).str("error"), Some("unknown_key"));
    assert!(!env.stick_file("settings/ai.toml").contains(KEY));
}

/// Removed while the stick was away: the stick loses the provider and its key when it is back.
#[test]
fn a_removal_on_the_fallback_reaches_the_stick() {
    let env = Env::new("prov_fallback", true);
    let (mut r, _) = with_claude(&env);
    // The stick turns read-only in the middle: the write of the removal itself fails.
    let dir = env.paths.stick.join("settings");
    let mode = |m: u32| std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(m)).unwrap();
    mode(0o555);
    let a = r.set("ai.providers", &s(""), &yes()).unwrap();
    mode(0o700);
    assert_eq!(a.str("stored"), Some("fallback"), "this test needs a user whose writes a read-only folder stops (not root)");
    assert!(!env.fb_file("ai.toml").contains(KEY));
    assert!(env.stick_file("settings/ai.toml").contains(KEY), "the stick did not take the write and still holds it");
    assert!(r.tick_now());
    assert_eq!(r.status().str("place"), Some("stick"));
    let file = env.stick_file("settings/ai.toml");
    assert!(!file.contains(KEY) && !file.contains("provider.claude"), "{file}");
    assert!(!env.stick_file("settings/owners.list").contains("ai.provider.claude"));
    assert_eq!(r.get("ai.provider.claude.key", None, &prog("assist")).unwrap_err().code, "unknown_key");
    // And a list that only a file brings (settingsd started without the stick) deletes nothing.
    let env = Env::new("prov_fallback2", true);
    let (r, _) = with_claude(&env);
    drop(r);
    let away = env.root.join("stick.away");
    std::fs::rename(&env.paths.stick, &away).unwrap();
    let (mut r, _) = env.open();
    assert_eq!(r.get("ai.provider.claude.key", None, &prog("assist")).unwrap_err().code, "unknown_key");
    std::fs::rename(&away, &env.paths.stick).unwrap();
    assert!(r.tick_now());
    assert_eq!(value(&r, "ai.provider.claude.address"), s(HOME));
    assert_eq!(clear(&r, "ai.provider.claude.key", &prog("assist")), Ok(s(KEY)));
}

#[test]
fn the_roles_and_the_name_are_typed_keys() {
    let env = Env::new("prov_roles", true);
    let (mut r, _) = env.open();
    for (key, ty, default) in [
        ("ai.role.chat", "text", s("")),
        ("ai.role.chat_fallback", "text", s("")),
        ("ai.local_only", "bool", Json::Bool(false)),
        ("ai.cloud_notice", "bool", Json::Bool(true)),
        ("ai.assistant.name", "text", s("Claude")),
    ] {
        let d = r.describe(key).unwrap().get("key").cloned().unwrap();
        assert_eq!(d.get("schema").unwrap().str("type"), Some(ty), "{key}");
        assert_eq!(d.get("default"), Some(&default), "{key}");
        assert_eq!(d.str("owner"), Some("settingsd"), "{key}");
        let de = d.get("text").unwrap().str("de").unwrap();
        assert!(de.len() > 10 && !de.contains("provider") && !de.contains("fallback"), "{key}: {de}");
    }
    // EINS: only the chat's role is left; the others are no settings any more.
    for gone in ["ai.role.search", "ai.role.search_fallback", "ai.role.jobs", "ai.role.jobs_fallback", "ai.role.explain"] {
        assert_eq!(r.describe(gone).unwrap_err().code, "unknown_key", "{gone}");
    }
    // A role names one provider, its fallback a list of them, in order.
    r.set("ai.role.chat", &s("strata"), &WHO).unwrap();
    r.set("ai.role.chat_fallback", &s("lokal, claude"), &WHO).unwrap();
    assert_eq!(r.set("ai.role.chat", &s("strata, claude"), &WHO).unwrap_err().code, "bad_value");
    assert_eq!(r.set("ai.role.chat_fallback", &s("Strata"), &WHO).unwrap_err().code, "bad_value");
    r.set("ai.role.chat", &s(""), &WHO).unwrap();
    r.set("ai.role.chat_fallback", &s(""), &WHO).unwrap();
    assert_eq!(r.set("ai.cloud_notice", &s("vielleicht"), &WHO).unwrap_err().code, "bad_value");
    r.set("ai.assistant.name", &s("Fable"), &WHO).unwrap();
    assert_eq!(value(&r, "ai.assistant.name"), s("Fable"));
}

/// No role is left pointing at a provider that was removed; the list's change stays the one an undo by its
/// number names.
#[test]
fn a_removed_provider_leaves_the_roles() {
    let env = Env::new("prov_roles_gone", true);
    let (mut r, _) = with_claude(&env);
    add(&mut r, "strata");
    r.set("ai.role.chat", &s("claude"), &WHO).unwrap();
    r.set("ai.role.chat_fallback", &s("strata, claude"), &WHO).unwrap();
    let (rx, _) = r.watch("ai.role.chat");

    // Refused: nothing moves, the roles neither.
    assert_eq!(r.set("ai.providers", &s("strata"), &WHO).unwrap_err().code, "needs_confirm");
    assert_eq!(value(&r, "ai.role.chat"), s("claude"));
    assert!(rx.try_recv().is_err());

    let a = r.set("ai.providers", &s("strata"), &yes()).unwrap();
    assert_eq!(value(&r, "ai.role.chat"), s(""));
    assert_eq!(value(&r, "ai.role.chat_fallback"), s("strata"));
    let told = parse(&rx.try_recv().unwrap()).unwrap();
    assert_eq!((told.str("event"), told.str("key"), told.str("source")), (Some("changed"), Some("ai.role.chat"), Some("test")));
    let file = env.stick_file("settings/ai.toml");
    assert!(!file.contains("claude") && file.contains("role.chat_fallback"), "{file}");

    // The list's change is the newest: the page's undo names it and gets the name back, as an empty provider.
    let seq = a.get("seq").and_then(Json::as_i64).unwrap();
    let Some(Json::Arr(newest)) = r.history(1, None).get("entries").cloned() else { panic!("history") };
    assert_eq!((newest[0].str("key"), newest[0].get("seq").and_then(Json::as_i64)), (Some("ai.providers"), Some(seq)));
    r.undo_expect(1, u64::try_from(seq).ok(), &WHO).unwrap();
    assert_eq!(value(&r, "ai.providers"), s("claude, strata"));
    assert_eq!(value(&r, "ai.role.chat"), s(""), "the role is set again when the provider is");
    // The roles' changes are in the history like any other and can be undone after it.
    r.undo(2, &WHO).unwrap();
    assert_eq!(value(&r, "ai.role.chat"), s("claude"));
    assert_eq!(value(&r, "ai.role.chat_fallback"), s("strata, claude"));

    // An empty provider goes without a question, and still leaves its roles. After a restart too.
    r.set("ai.providers", &s("strata"), &WHO).unwrap();
    assert_eq!(value(&r, "ai.role.chat"), s(""));
    drop(r);
    let (mut r, _) = env.open();
    assert_eq!(value(&r, "ai.role.chat"), s(""));
    assert_eq!(value(&r, "ai.role.chat_fallback"), s("strata"));
    // A role may name a provider that is not on the list (the page may set the role first).
    r.set("ai.role.chat", &s("spaeter"), &WHO).unwrap();
}

/// The files of a machine from before CL-5s: `claude.name` in claude.toml, the providers in ai.toml with no
/// key definitions anywhere. Old file in, new keys out; the second start changes nothing.
#[test]
fn old_names_move_once() {
    let env = Env::new("prov_migrate", true);
    let dir = env.paths.stick.join("settings");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("claude.toml"), "claude.name = \"Fable\"\n").unwrap();
    let ai = format!("ai.providers = \"claude\"\nai.provider.claude.kind = \"anthropic\"\nai.provider.claude.address = \"{HOME}\"\nai.provider.claude.key = \"{KEY}\"\nai.role.chat = \"claude\"\n");
    std::fs::write(dir.join("ai.toml"), &ai).unwrap();
    let old_entry = r#"{"seq":1,"t_ms":5,"key":"claude.name","old":null,"new":"Fable","source":"page","op":"set"}"#;
    std::fs::write(dir.join("journal.log"), format!("{old_entry}\n")).unwrap();

    let (r, ev) = env.open();
    assert_eq!(value(&r, "ai.assistant.name"), s("Fable"));
    assert_eq!(r.get("claude.name", None, &prog("bitscomp")).unwrap_err().code, "unknown_key");
    assert!(!dir.join("claude.toml").exists(), "the old name left the files");
    assert!(env.stick_file("settings/ai.toml").contains("ai.assistant.name = \"Fable\""));
    let journal = env.stick_file("settings/journal.log");
    assert!(!journal.contains("claude.name") && journal.contains(r#""key":"ai.assistant.name""#) && journal.contains(r#""op":"migrate""#), "{journal}");
    let log = ev.lock().unwrap().join("\n");
    assert!(log.contains("settingsd migrated key=claude.name to=ai.assistant.name new_key_was_set=false"), "{log}");
    // The providers of the old file have their keys, values and the secret's reader without a registration.
    assert_eq!(value(&r, "ai.provider.claude.kind"), s("anthropic"));
    assert_eq!(value(&r, "ai.provider.claude.model"), s(""));
    assert_eq!(clear(&r, "ai.provider.claude.key", &prog("assist")), Ok(s(KEY)));
    assert_eq!(clear(&r, "ai.provider.claude.key", &stranger()), Err("not_owner"));
    assert!(!log.contains(KEY), "{log}");
    drop(r);

    let files = |env: &Env| ["ai.toml", "journal.log", "claude.toml", "MIGRATED"].map(|f| env.stick_file(&format!("settings/{f}")));
    let before = files(&env);
    let (r, ev) = env.open();
    assert_eq!(before, files(&env), "the second start writes nothing");
    assert!(!ev.lock().unwrap().iter().any(|l| l.contains("migrated key=") || l.contains("migrate.failed")), "{:?}", ev.lock().unwrap());
    assert_eq!(value(&r, "ai.assistant.name"), s("Fable"));
    drop(r);

    // A name Georg already gave the new key stays; the old one still leaves.
    let env = Env::new("prov_migrate2", true);
    let dir = env.paths.stick.join("settings");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("claude.toml"), "claude.name = \"Alt\"\n").unwrap();
    std::fs::write(dir.join("ai.toml"), "ai.assistant.name = \"Neu\"\n").unwrap();
    let (r, ev) = env.open();
    assert_eq!(value(&r, "ai.assistant.name"), s("Neu"));
    assert!(!dir.join("claude.toml").exists());
    assert!(ev.lock().unwrap().iter().any(|l| l.contains("migrated key=claude.name to=ai.assistant.name new_key_was_set=true")));
    // No old name stored: nothing is logged and nothing written.
    let env = Env::new("prov_migrate3", true);
    let (_r, ev) = env.open();
    assert!(!ev.lock().unwrap().iter().any(|l| l.contains("migrated key=")), "{:?}", ev.lock().unwrap());
}
