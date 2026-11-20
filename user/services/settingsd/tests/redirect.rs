//! INT-CL3T (CL-S change request 5): a stored provider key goes only where it was entered for. While a key is
//! stored, the provider's address, kind and place do not change, whoever asks and whatever the request claims;
//! not by set, reset or undo, and not by a change that comes back from the fallback.

mod common;
use common::*;
use settingsd::change::Who;
use settingsd::json::{Json, parse};
use settingsd::key::KeyDef;
use settingsd::peer::{self, Peer};
use settingsd::proto::{self, Reply};
use settingsd::registry::Registry;
use settingsd::server::Shared;
use std::sync::Arc;

/// A value no answer and no event may hold.
const KEY: &str = "sk-redirect-NEVER-SHOWN-77c1";
const HOME: &str = "https://api.example.com";
const EVIL: &str = "https://evil.example.net";

/// The five keys of provider `id`, as assist registers them.
fn provider(id: &str) -> Vec<KeyDef> {
    let k = |name: &str, schema: &str, default: &str, more: &str| {
        let j = format!(r#"{{"key":"ai.provider.{id}.{name}","schema":{schema},"default":"{default}","text":{{"de":"A","en":"A"}}{more}}}"#);
        KeyDef::from_json(&parse(&j).unwrap(), "assist").unwrap()
    };
    let text = r#"{"type":"text","max_bytes":2048}"#;
    vec![
        k("kind", r#"{"type":"choice","choices":["unset","anthropic","openai"],"choices_de":["-","A","O"]}"#, "unset", ""),
        k("address", text, "", ""),
        k("model", text, "", ""),
        k("key", text, "", r#","safety":"secret""#),
        k("place", r#"{"type":"choice","choices":["unset","cloud","local"],"choices_de":["-","C","L"]}"#, "unset", ""),
    ]
}

fn stranger() -> Peer {
    Peer::program(1000, "/data/home/1000/dl/game")
}

fn who<'a>(p: &'a Peer, confirmed: bool) -> Who<'a> {
    Who { source: "test", confirmed, asker: Some(p) }
}

/// Provider `p` set up at [`HOME`], its key stored by assist.
fn with_key(env: &Env) -> (Registry, Arc<std::sync::Mutex<Vec<String>>>) {
    let (mut r, ev) = env.open();
    r.register("assist", provider("p"), &prog("assist")).unwrap();
    for (f, v) in [("kind", "anthropic"), ("address", HOME), ("place", "cloud"), ("model", "m1")] {
        r.set(&format!("ai.provider.p.{f}"), &s(v), &WHO).unwrap();
    }
    r.set("ai.provider.p.key", &s(KEY), &who(&prog("assist"), false)).unwrap();
    (r, ev)
}

#[test]
fn a_stored_key_does_not_move() {
    let env = Env::new("redirect", true);
    let (mut r, ev) = with_key(&env);
    let (assist, game) = (prog("assist"), stranger());
    // Nobody: not another program, not with "confirmed", not the owner's own program, not the MCP bridge.
    for (peer, confirmed) in [(&game, false), (&game, true), (&assist, true), (&peer::INSIDE, true)] {
        for (f, v) in [("address", EVIL), ("kind", "openai"), ("place", "local")] {
            let e = r.set(&format!("ai.provider.p.{f}"), &s(v), &who(peer, confirmed)).unwrap_err();
            assert_eq!(e.code, "key_bound", "{f} by {peer:?} confirmed {confirmed}: {}", e.msg);
            assert!(e.msg.contains("settings reset --confirm ai.provider.p.key") && e.msg.contains("assist --key p"), "{}", e.msg);
        }
        assert_eq!(r.reset("ai.provider.p.address", &who(peer, confirmed)).unwrap_err().code, "key_bound");
    }
    assert_eq!(value(&r, "ai.provider.p.address"), s(HOME));
    assert_eq!(value(&r, "ai.provider.p.kind"), s("anthropic"));
    let log = ev.lock().unwrap().join("\n");
    assert!(log.contains("settingsd redirect.refused key=ai.provider.p.address provider=p op=set source=test pid=0 uid=1000 exe=/data/home/1000/dl/game"), "{log}");
    assert!(!log.contains(KEY) && !log.contains(EVIL), "{log}");
    // What does not move the key stays free: the model, the same value again, another provider without a key.
    r.set("ai.provider.p.model", &s("m2"), &who(&game, false)).unwrap();
    let same = r.set("ai.provider.p.address", &s(HOME), &who(&game, false)).unwrap();
    assert_eq!(same.get("changed").and_then(Json::as_bool), Some(false));
    r.register("assist", provider("q"), &assist).unwrap();
    r.set("ai.provider.q.address", &s(EVIL), &who(&game, false)).unwrap();
    // An ordinary undo goes on working.
    r.set("theme.radius", &Json::Int(7), &WHO).unwrap();
    assert_eq!(r.undo(1, &WHO).unwrap().get("stopped"), Some(&Json::Null));
    // Georg's way: remove the key (a secret is removed by its owner, or by his own yes on the page: INT-CL-4,
    // another program's "confirmed" is not that), change, enter it again.
    assert_eq!(r.check_secret_write("ai.provider.p.key", &game, false).unwrap_err().code, "not_owner");
    assert_eq!(r.check_secret_write("ai.provider.p.key", &game, true).unwrap_err().code, "not_confirmer");
    let page = prog("bitscomp");
    r.check_secret_write("ai.provider.p.key", &page, true).unwrap();
    r.reset("ai.provider.p.key", &who(&page, true)).unwrap();
    r.set("ai.provider.p.address", &s(EVIL), &who(&game, false)).unwrap();
    // Whoever moved it got no key with it: the secret is gone, and only its owner's program reads a new one.
    assert_eq!(r.get("ai.provider.p.key", Some("assist"), &assist).unwrap().get("value"), Some(&s("")));
    r.set("ai.provider.p.key", &s("another"), &who(&assist, false)).unwrap();
    assert_eq!(r.set("ai.provider.p.address", &s(HOME), &who(&assist, true)).unwrap_err().code, "key_bound");
}

/// The journal's order protects the address too: a key can only be entered after the address it is for, and
/// an undo stops at the key's entry (a secret's old value is not kept), so it never reaches a change of the
/// address behind it. `change` holds the rule for an undo all the same.
#[test]
fn an_undo_does_not_move_a_stored_key() {
    let env = Env::new("redirect_undo", true);
    let (mut r, _) = env.open();
    r.register("assist", provider("p"), &prog("assist")).unwrap();
    for (f, v) in [("kind", "anthropic"), ("place", "cloud"), ("address", EVIL)] {
        r.set(&format!("ai.provider.p.{f}"), &s(v), &WHO).unwrap();
    }
    // The newest change of the journal: the address moves from EVIL to HOME; then the key is entered for HOME.
    r.set("ai.provider.p.address", &s(HOME), &WHO).unwrap();
    r.set("ai.provider.p.key", &s(KEY), &who(&prog("assist"), false)).unwrap();
    // Undoing "the last change" would skip the secret and put EVIL back under the stored key.
    let e = r.undo(2, &Who { source: "test", confirmed: true, asker: None }).unwrap_err();
    assert_eq!(e.code, "not_undoable", "{}", e.msg);
    assert_eq!(value(&r, "ai.provider.p.address"), s(HOME));
}

#[test]
fn the_rule_holds_before_assist_is_back_after_a_restart() {
    let env = Env::new("redirect_restart", true);
    let (r, _) = with_key(&env);
    drop(r);
    // settingsd restarted: the values are in its files, nobody registered the keys yet.
    let (mut r, _) = env.open();
    assert_eq!(r.set("ai.provider.p.address", &s(EVIL), &who(&stranger(), true)).unwrap_err().code, "unknown_key");
    assert_eq!(r.register("game", provider("p").into_iter().map(|mut d| { d.owner = "game".into(); d.applier = settingsd::key::Applier::Service("game".into()); d }).collect(), &stranger()).unwrap_err().code, "not_owner");
    r.register("assist", provider("p"), &prog("assist")).unwrap();
    assert_eq!(r.set("ai.provider.p.address", &s(EVIL), &who(&stranger(), true)).unwrap_err().code, "key_bound");
    assert_eq!(r.get("ai.provider.p.key", Some("assist"), &prog("assist")).unwrap().get("value"), Some(&s(KEY)));
}

/// settingsd starts on the fallback for about a second at every boot, and then knows nothing of the stick's
/// key. A change of the address made there does not reach the provider when the stick's values arrive.
#[test]
fn a_change_from_the_fallback_does_not_move_the_sticks_key() {
    let env = Env::new("redirect_resync", true);
    let (r, _) = with_key(&env);
    drop(r);
    let away = env.root.join("stick.away");
    std::fs::rename(&env.paths.stick, &away).unwrap();
    let (mut r, ev) = env.open();
    assert_eq!(r.status().str("place"), Some("fallback"));
    r.register("assist", provider("p"), &prog("assist")).unwrap();
    r.set("ai.provider.p.address", &s(EVIL), &who(&stranger(), false)).unwrap();
    r.set("ai.provider.p.model", &s("m9"), &who(&stranger(), false)).unwrap();
    std::fs::rename(&away, &env.paths.stick).unwrap();
    assert!(r.tick_now());
    assert_eq!(r.status().str("place"), Some("stick"), "{:?}", ev.lock().unwrap());
    assert_eq!(value(&r, "ai.provider.p.address"), s(HOME), "the fallback's address reached the stick's key");
    assert_eq!(value(&r, "ai.provider.p.model"), s("m9"), "an ordinary change made on the fallback is carried over");
    assert!(ev.lock().unwrap().iter().any(|l| l.contains("redirect.dropped key=ai.provider.p.address when=resync")), "{:?}", ev.lock().unwrap());
    let file = env.stick_file("settings/ai.toml");
    assert!(file.contains(HOME) && !file.contains(EVIL) && file.contains(KEY), "{file}");
    assert_eq!(r.get("ai.provider.p.key", Some("assist"), &prog("assist")).unwrap().get("value"), Some(&s(KEY)));

    // The same when settingsd restarts in between (the fallback's files are read at the start).
    let env = Env::new("redirect_resync2", true);
    let (r, _) = with_key(&env);
    drop(r);
    let away = env.root.join("stick.away");
    std::fs::rename(&env.paths.stick, &away).unwrap();
    let (mut r, _) = env.open();
    r.register("assist", provider("p"), &prog("assist")).unwrap();
    r.set("ai.provider.p.address", &s(EVIL), &who(&stranger(), false)).unwrap();
    drop(r);
    std::fs::rename(&away, &env.paths.stick).unwrap();
    let (mut r, ev) = env.open();
    assert_eq!(r.status().str("place"), Some("stick"));
    assert!(ev.lock().unwrap().iter().any(|l| l.contains("redirect.dropped key=ai.provider.p.address")), "{:?}", ev.lock().unwrap());
    r.register("assist", provider("p"), &prog("assist")).unwrap();
    assert_eq!(value(&r, "ai.provider.p.address"), s(HOME));

    // A key entered on the fallback comes with the address it was entered for: both are the fallback's.
    let env = Env::new("redirect_resync3", true);
    let (r, _) = with_key(&env);
    drop(r);
    let away = env.root.join("stick.away");
    std::fs::rename(&env.paths.stick, &away).unwrap();
    let (mut r, _) = env.open();
    r.register("assist", provider("p"), &prog("assist")).unwrap();
    r.set("ai.provider.p.address", &s(EVIL), &who(&stranger(), false)).unwrap();
    r.set("ai.provider.p.key", &s("the-movers-own"), &who(&stranger(), true)).unwrap();
    std::fs::rename(&away, &env.paths.stick).unwrap();
    assert!(r.tick_now());
    assert_eq!(r.status().str("place"), Some("stick"));
    assert_eq!(value(&r, "ai.provider.p.address"), s(EVIL));
    let file = env.stick_file("settings/ai.toml");
    assert!(!file.contains(KEY) && file.contains("the-movers-own"), "the stick's key went along to the new address: {file}");
}

fn ask(shared: &Arc<Shared>, peer: &Peer, line: &str) -> Json {
    match proto::handle(shared, peer, line) {
        Reply::Line(l) => parse(&l).unwrap(),
        Reply::Watch(_) => panic!("watch"),
    }
}

/// `set` with `while`: the key is stored only if the provider is still what the one who typed it was shown;
/// check and store are one step. And through the socket's protocol the refusal names its code.
#[test]
fn a_key_is_entered_for_what_was_shown() {
    let env = Env::new("redirect_while", true);
    let (mut r, _) = env.open();
    r.register("assist", provider("p"), &prog("assist")).unwrap();
    for (f, v) in [("kind", "anthropic"), ("address", HOME), ("place", "cloud")] {
        r.set(&format!("ai.provider.p.{f}"), &s(v), &WHO).unwrap();
    }
    let shared = Arc::new(Shared::new(r));
    let assist = prog("assist");
    let set_key = |shown: &str| {
        let line = format!(
            r#"{{"op":"set","key":"ai.provider.p.key","value":"{KEY}","while":{{"ai.provider.p.address":"{shown}","ai.provider.p.kind":"anthropic","ai.provider.p.place":"cloud"}}}}"#
        );
        ask(&shared, &assist, &line)
    };
    // Somebody moved the address while the key was typed.
    ask(&shared, &stranger(), &format!(r#"{{"op":"set","key":"ai.provider.p.address","value":"{EVIL}"}}"#));
    let a = set_key(HOME);
    assert_eq!((a.get("ok").and_then(Json::as_bool), a.str("error")), (Some(false), Some("changed_meanwhile")), "{}", a.to_text());
    assert_eq!(ask(&shared, &assist, r#"{"op":"get","key":"ai.provider.p.key","owner":"assist"}"#).str("value"), Some(""));
    // Shown and unchanged: stored, and from then on the address stays.
    ask(&shared, &stranger(), &format!(r#"{{"op":"set","key":"ai.provider.p.address","value":"{HOME}"}}"#));
    assert_eq!(set_key(HOME).get("ok").and_then(Json::as_bool), Some(true));
    let a = ask(&shared, &stranger(), &format!(r#"{{"op":"set","key":"ai.provider.p.address","value":"{EVIL}","confirmed":true}}"#));
    assert_eq!(a.str("error"), Some("key_bound"), "{}", a.to_text());
    assert!(!a.to_text().contains(KEY));
    // A key nobody registered, a value of another type, a list instead of an object: nothing is stored.
    for cond in [r#"{"ai.provider.zz.address":"x"}"#, r#"{"ai.provider.p.address":7}"#, r#"["ai.provider.p.address"]"#] {
        let a = ask(&shared, &assist, &format!(r#"{{"op":"set","key":"ai.provider.p.model","value":"mx","while":{cond}}}"#));
        assert_eq!(a.get("ok").and_then(Json::as_bool), Some(false), "{cond}: {}", a.to_text());
    }
    assert_eq!(ask(&shared, &assist, r#"{"op":"get","key":"ai.provider.p.model"}"#).str("value"), Some(""));
    // `status` tells the asker what settingsd takes it for.
    let st = ask(&shared, &assist, r#"{"op":"status"}"#);
    assert_eq!(st.get("asker").and_then(|a| a.str("exe")), Some("/bitsos/bin/assist"), "{}", st.to_text());
}

/// MC-L change request 5b: a settings change that came through the kernel's local tools socket is journaled
/// as `local`, one from the network side as `mcp`; neither moves a stored key.
#[test]
fn a_call_through_the_local_tools_socket_is_journaled_as_local() {
    use settingsd::mcp_tools;
    let env = Env::new("redirect_local", true);
    let (r, _) = with_key(&env);
    let shared = Shared::new(r);
    let args = |t: &str| mcpreg::Json::parse(t).unwrap();
    for (local, radius) in [(true, 3), (false, 5)] {
        let a = mcp_tools::call(&shared, "settings_set", &args(&format!(r#"{{"key":"theme.radius","value":{radius}}}"#)), local);
        assert!(matches!(a, mcpreg::Reply::Ok(_)), "local {local}");
    }
    let h = shared.view().history(2, Some("theme")).to_text();
    assert!(h.contains(r#""source":"local""#) && h.contains(r#""source":"mcp""#), "{h}");
    for local in [true, false] {
        let a = mcp_tools::call(&shared, "settings_set", &args(&format!(r#"{{"key":"ai.provider.p.address","value":"{EVIL}","confirmed":true}}"#)), local);
        let mcpreg::Reply::Error(text) = a else { panic!("the address moved (local {local})") };
        assert!(text.starts_with("key_bound: ") && !text.contains(KEY), "{text}");
    }
}
