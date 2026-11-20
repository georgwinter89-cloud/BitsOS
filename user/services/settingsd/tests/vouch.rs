//! INT-CL-4: whose `"confirmed": true` is Georg's own yes. A stored secret is cleared or replaced, and a
//! provider with its key removed, only by a request of the session's compositor or of root; every other
//! program of his user is refused with a sentence that says where to do it, and nothing is stored, removed
//! or journaled by a refused request. With the kernel's answers of BitsOS given: the record of who connected
//! and the holder of the screen's grant.

mod common;
use common::*;
use settingsd::json::{Json, parse};
use settingsd::peer::{self, Peer, Source};
use settingsd::proto::{self, Reply};
use settingsd::registry::Registry;
use settingsd::server::Shared;
use std::sync::{Arc, Mutex};

/// A value no answer, no event and no journal may hold.
const KEY: &str = "sk-int-cl4-NEVER-SHOWN-71c0";
const LIST: &str = "claude, strata";

fn told(pid: u32, uid: u32, exe: &str) -> Peer {
    Peer { pid, uid: Some(uid), exe: Some(exe.to_string()), by: Source::Kernel }
}

/// The session's compositor: the kernel names pid 412 as the holder of the screen.
fn compositor() -> Peer {
    told(412, 1000, "/boot/BITSOS2/bin/bitscomp")
}

fn root() -> Peer {
    told(77, 0, "/bin/settings")
}

/// Everybody whose yes is not Georg's, with why.
fn others() -> Vec<(Peer, &'static str)> {
    vec![
        (told(500, 1000, "/bin/settings"), "the command line of his user"),
        (told(501, 1000, "/data/home/1000/dl/game"), "a program he downloaded"),
        (told(502, 1000, "/bitsos/boot/BITSOS2/bin/bitscomp"), "a compositor that is not at its place"),
        (told(413, 1000, "/boot/BITSOS2/bin/bitscomp"), "a second bitscomp that does not hold the screen"),
        (Peer { by: Source::Proc, ..compositor() }, "a pid that only /proc names as the compositor"),
        (Peer { exe: None, ..compositor() }, "a program in a namespace of its own"),
        (peer::INSIDE.clone(), "settingsd's own MCP tools"),
    ]
}

/// Two providers with stored keys, on a machine whose kernel names the screen's holder.
fn machine(env: &Env) -> (Arc<Shared>, Arc<Mutex<Vec<String>>>) {
    let (mut r, ev) = env.open();
    r.set_holder(Arc::new(|| Ok(Some("holder=412\n".to_string()))));
    r.set("ai.providers", &s(LIST), &WHO).unwrap();
    for id in ["claude", "strata"] {
        for (f, v) in [("kind", "anthropic"), ("address", "https://api.example.com"), ("place", "cloud"), ("key", KEY)] {
            r.set(&format!("ai.provider.{id}.{f}"), &s(v), &WHO).unwrap();
        }
    }
    (Arc::new(Shared::new(r)), ev)
}

fn ask(shared: &Arc<Shared>, peer: &Peer, line: &str) -> Json {
    match proto::handle(shared, peer, line) {
        Reply::Line(l) => parse(&l).unwrap(),
        Reply::Watch(_) => panic!("watch"),
    }
}

fn stored(shared: &Arc<Shared>, id: &str) -> Option<Json> {
    let assist = Peer { pid: 600, ..prog("assist") };
    shared.view().get(&format!("ai.provider.{id}.key"), Some("assist"), &assist).ok().and_then(|j| j.get("value").cloned())
}

fn seq(r: &Registry) -> Json {
    r.history(1, None).get("entries").and_then(|e| if let Json::Arr(a) = e { a.first().and_then(|x| x.get("seq").cloned()) } else { None }).unwrap_or(Json::Null)
}

const REQUESTS: [&str; 5] = [
    r#"{"op":"set","key":"ai.provider.claude.key","value":"sk-theirs","confirmed":true}"#,
    r#"{"op":"set","key":"ai.provider.claude.key","value":"","confirmed":true}"#,
    r#"{"op":"reset","key":"ai.provider.claude.key","confirmed":true}"#,
    r#"{"op":"set","key":"ai.providers","value":"strata","confirmed":true}"#,
    r#"{"op":"reset","key":"ai.providers","confirmed":true}"#,
];

#[test]
fn another_programs_confirmed_removes_and_stores_nothing() {
    let env = Env::new("vouch_refused", true);
    let (shared, ev) = machine(&env);
    let (file, owners, journal) = (env.stick_file("settings/ai.toml"), env.stick_file("settings/owners.list"), env.stick_file("settings/journal.log"));
    let before = seq(&shared.view());
    let mut refusals = 0;
    for (p, who) in others() {
        for line in REQUESTS {
            let a = ask(&shared, &p, line);
            let code = if p.untold() { "peer_unknown" } else { "not_confirmer" };
            assert_eq!(a.str("error"), Some(code), "{who}: {line}: {}", a.to_text());
            let msg = a.str("message").unwrap();
            assert!(msg.contains("Seite „KI“") && msg.contains("als root an der Konsole") && msg.contains("zählt hier nicht"), "{who}: {msg}");
            assert!(!a.to_text().contains(KEY), "{who}");
            refusals += 1;
        }
        // Without the word nothing changes either, and the answer does not send the asker for a yes it cannot give.
        let a = ask(&shared, &p, r#"{"op":"set","key":"ai.providers","value":"strata"}"#);
        assert!(matches!(a.str("error"), Some("not_confirmer" | "peer_unknown")), "{who}: {}", a.to_text());
        assert!(matches!(ask(&shared, &p, r#"{"op":"reset","key":"ai.provider.claude.key"}"#).str("error"), Some("not_owner" | "peer_unknown")), "{who}");
        refusals += 1;
    }
    // Nothing was stored, removed or journaled.
    assert_eq!(stored(&shared, "claude"), Some(s(KEY)));
    assert_eq!(value(&shared.view(), "ai.providers"), s(LIST));
    assert_eq!(seq(&shared.view()), before);
    assert_eq!(env.stick_file("settings/ai.toml"), file);
    assert_eq!(env.stick_file("settings/owners.list"), owners);
    assert_eq!(env.stick_file("settings/journal.log"), journal);
    // Each refusal is an event that names the asker and the reason, never a value.
    let log = ev.lock().unwrap().join("\n");
    // (A program settingsd could not tell gets `peer.unknown` instead, also for its unconfirmed reset.)
    let untold = others().iter().filter(|(p, _)| p.untold()).count();
    assert_eq!(log.matches("confirm.refused what=").count() + log.matches("peer.unknown what=").count(), refusals + untold, "{log}");
    assert!(log.contains("settingsd confirm.refused what=remove key=ai.providers pid=500 uid=1000 exe=/bin/settings by=kernel why="), "{log}");
    assert!(log.contains("settingsd confirm.refused what=set key=ai.provider.claude.key pid=413 uid=1000 exe=/boot/BITSOS2/bin/bitscomp by=kernel why=\"pid 413 runs /boot/BITSOS2/bin/bitscomp, but pid 412 holds the compositor grant\""), "{log}");
    assert!(!log.contains(KEY) && !log.contains("sk-theirs"), "{log}");
}

#[test]
fn the_compositors_request_and_roots_are_georgs_own_yes() {
    for (who, p) in [("the compositor", compositor()), ("root", root())] {
        let env = Env::new(&format!("vouch_ok_{}", p.pid + p.uid.unwrap() + p.exe.as_ref().unwrap().len() as u32), true);
        let (shared, ev) = machine(&env);
        // Its word is still needed: without it the answer asks for it.
        assert_eq!(ask(&shared, &p, r#"{"op":"set","key":"ai.providers","value":"strata"}"#).str("error"), Some("needs_confirm"), "{who}");
        assert_eq!(ask(&shared, &p, r#"{"op":"reset","key":"ai.provider.claude.key"}"#).str("error"), Some("not_owner"), "{who}");
        // The key typed on the page, then deleted there.
        let set = ask(&shared, &p, r#"{"op":"set","key":"ai.provider.claude.key","value":"sk-typed-by-georg","confirmed":true}"#);
        assert_eq!(set.get("ok"), Some(&Json::Bool(true)), "{who}: {}", set.to_text());
        assert_eq!(stored(&shared, "claude"), Some(s("sk-typed-by-georg")), "{who}");
        assert_eq!(ask(&shared, &p, r#"{"op":"reset","key":"ai.provider.claude.key","confirmed":true}"#).get("ok"), Some(&Json::Bool(true)), "{who}");
        assert_eq!(stored(&shared, "claude"), Some(s("")), "{who}");
        // The provider removed with what is stored for it.
        let off = ask(&shared, &p, r#"{"op":"set","key":"ai.providers","value":"claude","confirmed":true}"#);
        assert_eq!(off.get("ok"), Some(&Json::Bool(true)), "{who}: {}", off.to_text());
        assert_eq!(stored(&shared, "strata"), None, "{who}");
        assert!(!env.stick_file("settings/ai.toml").contains(KEY), "{who}");
        let log = ev.lock().unwrap().join("\n");
        assert!(log.contains("provider.removed provider=strata key_stored=true") && !log.contains("confirm.refused"), "{who}: {log}");
    }
}

/// A compositor that lost the screen since it connected is refused from its next request on, and when the
/// kernel's file cannot be read only root is taken.
#[test]
fn the_holder_is_asked_for_every_request() {
    let env = Env::new("vouch_holder", true);
    let (mut r, _) = env.open();
    let says = Arc::new(Mutex::new(Ok(Some("holder=412\n".to_string()))));
    let h = says.clone();
    r.set_holder(Arc::new(move || h.lock().unwrap().clone()));
    r.set("ai.providers", &s("claude"), &WHO).unwrap();
    r.set("ai.provider.claude.key", &s(KEY), &WHO).unwrap();
    let shared = Arc::new(Shared::new(r));
    let clear = r#"{"op":"reset","key":"ai.provider.claude.key","confirmed":true}"#;
    for gone in [Ok(Some("holder=none\n".to_string())), Ok(Some("holder=900\n".to_string())), Err("EIO".to_string())] {
        *says.lock().unwrap() = gone.clone();
        assert_eq!(ask(&shared, &compositor(), clear).str("error"), Some("not_confirmer"), "{gone:?}");
        assert_eq!(stored(&shared, "claude"), Some(s(KEY)));
    }
    assert_eq!(ask(&shared, &root(), clear).get("ok"), Some(&Json::Bool(true)), "root needs no holder file");
    *says.lock().unwrap() = Ok(Some("holder=412\n".to_string()));
    assert_eq!(ask(&shared, &compositor(), r#"{"op":"set","key":"ai.providers","value":"","confirmed":true}"#).get("ok"), Some(&Json::Bool(true)));
}

/// An undo is a change like any other: one that would take a provider away needs the same yes. (Reachable
/// when what is stored for a name is older than the name's place on the list: values a file brought.)
#[test]
fn an_undo_that_removes_a_provider_needs_the_same_yes() {
    let env = Env::new("vouch_undo", true);
    let (mut r, _) = env.open();
    r.set("ai.providers", &s("claude"), &WHO).unwrap();
    r.set("ai.provider.claude.address", &s("https://api.example.com"), &WHO).unwrap();
    r.set("ai.provider.claude.key", &s(KEY), &WHO).unwrap();
    drop(r);
    // The list's line is lost from the file; the provider's values stay there as settings nobody registered.
    let file = env.paths.stick.join("settings/ai.toml");
    let text = std::fs::read_to_string(&file).unwrap();
    let kept: Vec<&str> = text.lines().filter(|l| !l.starts_with("ai.providers") && !l.starts_with("providers")).collect();
    assert!(kept.len() < text.lines().count(), "{text}");
    std::fs::write(&file, kept.join("\n") + "\n").unwrap();
    let (mut r, _) = env.open();
    r.set_holder(Arc::new(|| Ok(Some("holder=412\n".to_string()))));
    assert_eq!(value(&r, "ai.providers"), s(""));
    // The name comes back and its values with it: this change is now the newest, and its undo would remove them.
    r.set("ai.providers", &s("claude"), &WHO).unwrap();
    let shared = Arc::new(Shared::new(r));
    assert_eq!(stored(&shared, "claude"), Some(s(KEY)));
    let undo = r#"{"op":"undo","n":1,"confirmed":true}"#;
    let a = ask(&shared, &told(501, 1000, "/data/home/1000/dl/game"), undo);
    assert_eq!(a.str("error"), Some("not_confirmer"), "{}", a.to_text());
    assert_eq!((stored(&shared, "claude"), value(&shared.view(), "ai.providers")), (Some(s(KEY)), s("claude")));
    assert_eq!(ask(&shared, &compositor(), r#"{"op":"undo","n":1}"#).str("error"), Some("needs_confirm"));
    assert_eq!(ask(&shared, &compositor(), undo).get("ok"), Some(&Json::Bool(true)));
    assert_eq!((stored(&shared, "claude"), value(&shared.view(), "ai.providers")), (None, s("")));
}
