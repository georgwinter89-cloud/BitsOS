//! CX-3 (DESIGN §11.4), offline with the fake embedder on host/cx-eval: (a) documents opened together climb
//! for a query that finds one of them; (b) a dismissed document leaves its workspace for good and sinks for
//! similar queries; (c) a document that matches nothing never appears however often it was opened (the
//! gate); (d) forgetting the usage gives the cold order back exactly; (e) after 180 days an edge is a quarter.
//! The sessions are a replay file (host/tests/cx/sessions/steuer.json), run by `ctxd replay`.

mod common;
use common::*;
use ctxd::api;
use ctxd::config::{Config, RootCfg};
use ctxd::engine::{Caller, Engine, EngineOpts};
use serde_json::{Value, json};
use std::process::Command;

fn rank(top: &Value, name: &str) -> Option<usize> {
    top.as_array()?.iter().position(|p| p == name).map(|p| p + 1)
}

#[test]
fn replayed_sessions_move_the_ranks_and_forgetting_moves_them_back() {
    let d = tmp("replay");
    let out = Command::new(bin())
        .args(["--data", d.join("data").to_str().unwrap(), "--root", eval_docs().to_str().unwrap(), "--embedder", "fake", "replay"])
        .arg(repo().join("host/tests/cx/sessions/steuer.json"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let r = |label: &str| v["reports"].as_array().unwrap().iter().find(|r| r["label"] == label).unwrap()["top"].clone();
    let (cold, learned, recipe, later, forgot) =
        (r("kalt"), r("nach drei Sitzungen"), r("Rezept oft geöffnet"), r("nach 180 Tagen"), r("vergessen"));
    let b = |t: &Value| rank(t, "steuer-07.txt").unwrap_or(99);
    // (a) Opened three times with the first hit, after the same query: at least five places up.
    assert!(b(&learned) + 5 <= b(&cold), "steuer-07 {} -> {}", b(&cold), b(&learned));
    assert_eq!(rank(&learned, "steuer-12.md"), Some(1), "the anchor itself stays first");
    // (c) The gate: ten opens of a recipe after the search do not make it a tax document.
    assert_eq!(rank(&recipe, "rezept-01.md"), None, "{recipe}");
    // (e) decay: half a year later the climb is smaller, but not gone.
    assert!(b(&learned) < b(&later) && b(&later) < b(&cold), "{} {} {}", b(&learned), b(&later), b(&cold));
    // (d) Forgotten: the cold order, exactly.
    assert_eq!(forgot, cold);
    assert_eq!(v["events"], 0, "nothing learned is left");
}

fn engine(name: &str) -> (Engine, std::path::PathBuf) {
    let d = tmp(name);
    let mut cfg = Config::default();
    cfg.context.roots = vec![RootCfg { path: eval_docs(), private: false }];
    cfg.context.embedder.kind = "fake".into();
    let e = Engine::open(cfg.context, &d.join("data"), EngineOpts { worker_exe: Some(bin()) }).unwrap();
    e.index_now().unwrap();
    (e, d)
}

fn doc(e: &Engine, file: &str) -> String {
    let g = e.generation().unwrap();
    g.cat.docs.iter().find(|d| d.paths[0].path.ends_with(file)).unwrap().id.clone()
}

fn search(e: &Engine, q: &str) -> Vec<Value> {
    api::call(e, "context_search", &json!({"query": q, "limit": 50}), Caller::Desktop).unwrap()["items"].as_array().unwrap().clone()
}

fn pos(items: &[Value], id: &str) -> Option<usize> {
    items.iter().position(|i| i["doc"] == id)
}

#[test]
fn edges_decay_dismissals_count_and_the_why_says_it() {
    let (e, d) = engine("learn");
    let (a, b) = (doc(&e, "steuer-12.md"), doc(&e, "steuer-07.txt"));
    search(&e, "Steuer");
    for id in [&a, &b] {
        api::opened(&e, &json!({"doc": id})).unwrap();
        e.learn.advance(30);
    }
    let (fresh, n) = e.learn.edge(&a, &b);
    assert!(fresh > 0.8 && n == 1, "{fresh} {n}");
    // The why names the document they were opened with, the explain the numbers.
    let items = search(&e, "Steuer");
    let it = &items[pos(&items, &b).unwrap()];
    let why = it["why"].to_string();
    assert!(why.contains("Oft zusammen geöffnet mit") && why.contains("Nach einer ähnlichen Suche"), "{why}");
    let x = api::call(&e, "context_explain", &json!({"doc": b, "query": "Steuer"}), Caller::Desktop).unwrap();
    assert!(x["parts"]["C"]["A"].as_f64().unwrap() > 0.0 && x["parts"]["C"]["best"]["doc"] == a, "{x}");
    // (e) After 180 days the edge is a quarter (half-life 90 days).
    e.learn.advance(180 * 86_400);
    let ratio = e.learn.edge(&a, &b).0 / fresh;
    assert!((ratio - 0.25).abs() < 0.01, "{ratio}");

    // (b) Dismissed in a workspace: gone from it for good, and lower for the same query elsewhere.
    let w = api::call(&e, "workspace_open", &json!({"query": "Steuer"}), Caller::Desktop).unwrap();
    let ws = w["workspace"]["id"].as_str().unwrap().to_string();
    let c = doc(&e, "steuer-11.txt");
    let before = pos(&search(&e, "Steuer"), &c).unwrap();
    api::call(&e, "dismiss", &json!({"workspace": ws, "doc": c, "scope": "everywhere"}), Caller::Desktop).unwrap();
    let again = api::call(&e, "workspace_open", &json!({"workspace": ws}), Caller::Desktop).unwrap();
    let items: Vec<&Value> = again["workspace"]["groups"].as_array().unwrap().iter().flat_map(|g| g["items"].as_array().unwrap()).collect();
    assert!(!items.is_empty() && !items.iter().any(|i| i["doc"] == c), "not among the items");
    assert!(again["workspace"]["hidden"].to_string().contains(&c), "listed as hidden, with an undo");
    let after = pos(&search(&e, "Steuer"), &c);
    assert!(after.is_none_or(|p| p > before + 2), "{before} -> {after:?}");
    api::call(&e, "dismiss", &json!({"workspace": ws, "doc": c, "undo": true}), Caller::Desktop).unwrap();
    assert!(pos(&search(&e, "Steuer"), &c).is_some_and(|p| p <= before + 1), "undo takes the dismissal back");

    // The usage log survives a restart and holds no query text.
    drop(e);
    let mut cfg = Config::default();
    cfg.context.roots = vec![RootCfg { path: eval_docs(), private: false }];
    cfg.context.embedder.kind = "fake".into();
    let e2 = Engine::open(cfg.context, &d.join("data"), EngineOpts { worker_exe: Some(bin()) }).unwrap();
    assert!(e2.learn.events() >= 6);
    for f in std::fs::read_dir(d.join("data/usage")).unwrap().flatten() {
        let t = String::from_utf8_lossy(&std::fs::read(f.path()).unwrap()).to_string();
        assert!(!t.contains("Steuer"), "{}: {t}", f.path().display());
    }
    let n = api::call(&e2, "context_forget", &json!({"scope": "usage"}), Caller::Desktop).unwrap();
    assert!(n["events"].as_u64().unwrap() >= 6);
    assert_eq!(e2.learn.events(), 0);
}

/// Checker 12.11.: co-use with a private document must not reach Claude, neither through a plain search's
/// anchors (documents opened after similar queries) nor through a workspace Georg filled on the desktop.
#[test]
fn co_use_with_a_private_document_stays_off_the_mcp_answer() {
    let d = tmp("learn-private");
    let private = d.join("privat");
    std::fs::create_dir_all(&private).unwrap();
    std::fs::write(private.join("zebrafink-steuer.txt"), "Zebrafink Geheimakte\n\nSteuer Steuer Steuerbescheid privat.\n").unwrap();
    let mut cfg = Config::default();
    cfg.context.roots = vec![RootCfg { path: eval_docs(), private: false }, RootCfg { path: private.clone(), private: true }];
    cfg.context.embedder.kind = "fake".into();
    let e = Engine::open(cfg.context, &d.join("data"), EngineOpts { worker_exe: Some(bin()) }).unwrap();
    e.index_now().unwrap();
    let (a, p) = (doc(&e, "steuer-07.txt"), doc(&e, "zebrafink-steuer.txt"));
    search(&e, "Steuer");
    for id in [&p, &a] {
        api::opened(&e, &json!({"doc": id})).unwrap();
        e.learn.advance(30);
    }
    let clean = |v: &Value| {
        let s = v.to_string();
        assert!(!s.contains(&p) && !s.contains("Zebrafink") && !s.contains("zebrafink"), "private leaked: {s}");
    };
    // The desktop sees the tie (so the test exercises the path) ...
    let x = api::call(&e, "context_explain", &json!({"doc": a, "query": "Steuer"}), Caller::Desktop).unwrap();
    assert_eq!(x["parts"]["C"]["best"]["doc"], p, "{x}");
    // ... Claude does not, and its ranking is not steered by it.
    let x = api::call(&e, "context_explain", &json!({"doc": a, "query": "Steuer"}), Caller::Mcp).unwrap();
    clean(&x);
    assert!(x["parts"]["C"]["best"].is_null(), "{x}");
    clean(&api::call(&e, "context_search", &json!({"query": "Steuer", "limit": 50}), Caller::Mcp).unwrap());

    let w = api::call(&e, "workspace_open", &json!({"query": "Steuer"}), Caller::Desktop).unwrap();
    let ws = w["workspace"]["id"].as_str().unwrap().to_string();
    api::call(&e, "workspace_add", &json!({"workspace": ws, "docs": [p]}), Caller::Desktop).unwrap();
    let x = api::call(&e, "context_explain", &json!({"doc": a, "query": "Steuer", "workspace": ws}), Caller::Mcp).unwrap();
    clean(&x);
    clean(&api::call(&e, "workspace_open", &json!({"workspace": ws}), Caller::Mcp).unwrap());
}
