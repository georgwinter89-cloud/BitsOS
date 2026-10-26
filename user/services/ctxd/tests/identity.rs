//! DESIGN §4.2/§5/§9: a rename keeps the document id (and so its workspace place); a changed file replaces
//! its text; a deleted file leaves the index in one cycle; an unchanged tree makes no new generation.

mod common;
use common::*;
use ctxd::config::{Config, RootCfg};
use ctxd::engine::{Caller, Engine, EngineOpts};
use serde_json::json;

#[test]
fn rename_change_delete() {
    let d = tmp("identity");
    let root = d.join("docs");
    std::fs::create_dir_all(root.join("alt")).unwrap();
    std::fs::write(root.join("alt/brief.txt"), "Kündigung Stromvertrag Stadtwerke zum 31.12.").unwrap();
    std::fs::write(root.join("rezept.md"), "# Apfelkuchen\nMehl, Zucker, Äpfel.").unwrap();
    std::fs::write(root.join("weg.txt"), "Wird gelöscht: Zebrastreifen").unwrap();
    let mut cfg = Config::default();
    cfg.context.roots = vec![RootCfg { path: root.clone(), private: false }];
    cfg.context.embedder.kind = "fake".into();
    let e = Engine::open(cfg.context, &d.join("data"), EngineOpts { worker_exe: None }).unwrap();
    let g1 = e.index_now().unwrap().generation.unwrap();
    let id_of = |e: &Engine, name: &str| {
        let g = e.generation().unwrap();
        g.cat.docs.iter().find(|d| d.paths.iter().any(|p| p.path.ends_with(name))).map(|d| d.id.clone())
    };
    let brief = id_of(&e, "brief.txt").unwrap();
    let ws = ctxd::api::call(&e, "workspace_open", &json!({"name": "Strom"}), Caller::Desktop).unwrap();
    let ws_id = ws["workspace"]["id"].as_str().unwrap().to_string();
    ctxd::api::call(&e, "workspace_add", &json!({"workspace": ws_id, "docs": [brief]}), Caller::Desktop).unwrap();

    // Nothing changed: no new generation.
    assert_eq!(e.index_now().unwrap().generation, None);

    // Rename and move: same id, workspace item still there.
    std::fs::create_dir_all(root.join("neu")).unwrap();
    std::fs::rename(root.join("alt/brief.txt"), root.join("neu/kuendigung-strom.txt")).unwrap();
    // Change: new text replaces the old.
    std::fs::write(root.join("rezept.md"), "# Zwetschgenkuchen\nHefeteig, Zwetschgen.").unwrap();
    std::fs::remove_file(root.join("weg.txt")).unwrap();
    let g2 = e.index_now().unwrap().generation.unwrap();
    assert!(g2 > g1);
    assert_eq!(id_of(&e, "kuendigung-strom.txt").as_deref(), Some(brief.as_str()), "rename keeps the id");
    assert!(id_of(&e, "brief.txt").is_none());
    assert!(id_of(&e, "weg.txt").is_none(), "deleted file gone in one cycle");
    let r = ctxd::api::call(&e, "context_search", &json!({"query": "Zebrastreifen"}), Caller::Desktop).unwrap();
    assert!(r["items"].as_array().unwrap().iter().all(|i| !i["path"].as_str().unwrap().ends_with("weg.txt")));
    let r = ctxd::api::call(&e, "context_search", &json!({"query": "Zwetschgenkuchen"}), Caller::Desktop).unwrap();
    assert!(r["items"][0]["path"].as_str().unwrap().ends_with("rezept.md"));
    let r = ctxd::api::call(&e, "context_search", &json!({"query": "Apfelkuchen"}), Caller::Desktop).unwrap();
    assert!(r["items"].as_array().unwrap().iter().all(|i| !i["title"].as_str().unwrap().contains("Apfel")));
    let ws = ctxd::api::call(&e, "workspace_open", &json!({"workspace": ws_id}), Caller::Desktop).unwrap();
    assert!(ws.to_string().contains("kuendigung-strom.txt"), "{ws}");
    assert!(ws.to_string().contains("Von Hand hinzugefügt"));

    // Dismiss hides, undo brings it back; rename and pin; list.
    ctxd::api::call(&e, "dismiss", &json!({"workspace": ws_id, "doc": brief}), Caller::Desktop).unwrap();
    let ws = ctxd::api::call(&e, "workspace_open", &json!({"workspace": ws_id}), Caller::Desktop).unwrap();
    assert!(!ws["workspace"]["groups"].to_string().contains(&brief));
    assert!(ws["workspace"]["hidden"].to_string().contains(&brief));
    ctxd::api::call(&e, "dismiss", &json!({"workspace": ws_id, "doc": brief, "undo": true}), Caller::Desktop).unwrap();
    ctxd::api::call(&e, "workspace_rename", &json!({"workspace": ws_id, "name": "Strom 2026"}), Caller::Desktop)
        .unwrap();
    ctxd::api::call(&e, "workspace_pin", &json!({"workspace": ws_id}), Caller::Desktop).unwrap();
    let l = ctxd::api::call(&e, "workspace_list", &json!({"pinned_only": true}), Caller::Desktop).unwrap();
    assert_eq!(l["workspaces"][0]["name"], "Strom 2026");

    // Explain gives the raw numbers; forget index rebuilds.
    let x = ctxd::api::call(
        &e,
        "context_explain",
        &json!({"doc": brief, "query": "Stromvertrag kündigen"}),
        Caller::Desktop,
    )
    .unwrap();
    assert!(x["parts"]["B"]["bm25"].as_f64().unwrap() > 0.0, "{x}");
    assert!(x["weights"]["v"].as_f64().unwrap() > 0.0);
    let f = ctxd::api::call(&e, "context_forget", &json!({"scope": "index"}), Caller::Desktop).unwrap();
    assert!(f["files"].as_u64().unwrap() > 0);
    assert!(e.generation().is_none());
    e.index_now().unwrap();
    assert_eq!(e.generation().unwrap().cat.docs.len(), 2);
}

fn engine(d: &std::path::Path, root: &std::path::Path) -> Engine {
    let mut cfg = Config::default();
    cfg.context.roots = vec![RootCfg { path: root.to_path_buf(), private: false }];
    cfg.context.embedder.kind = "fake".into();
    Engine::open(cfg.context, &d.join("data"), EngineOpts { worker_exe: None }).unwrap()
}

fn top_path(e: &Engine, q: &str) -> Option<String> {
    let r = ctxd::api::call(e, "context_search", &json!({"query": q}), Caller::Desktop).unwrap();
    r["items"][0]["path"].as_str().map(str::to_string)
}

/// An edit in place with the same size and the same mtime second (an editor or script writing twice within
/// one second) is still seen: stamps are compared in nanoseconds, ctime included, and a file hashed right
/// after a write is never trusted by its stamps ("racy").
#[test]
fn same_size_edit_in_the_same_second() {
    let d = tmp("racy");
    let root = d.join("docs");
    std::fs::create_dir_all(&root).unwrap();
    let f = root.join("notiz.txt");
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    // An old file (not racy): only the stamps can tell the change.
    std::fs::write(&f, "Einkauf Apfelmus Haferflocken").unwrap();
    std::fs::File::options().write(true).open(&f).unwrap().set_modified(old).unwrap();
    // A fresh file written right before the scan (racy).
    let g = root.join("frisch.txt");
    std::fs::write(&g, "Termin Zahnarzt Dienstag").unwrap();
    let e = engine(&d, &root);
    e.index_now().unwrap();
    assert!(top_path(&e, "Apfelmus").unwrap().ends_with("notiz.txt"));

    // Same size, same mtime (set back to the old value): only ctime moved.
    std::fs::write(&f, "Einkauf Quittmus Haferflocken").unwrap();
    assert_eq!(std::fs::metadata(&f).unwrap().len(), "Einkauf Apfelmus Haferflocken".len() as u64);
    let f2 = std::fs::File::options().write(true).open(&f).unwrap();
    f2.set_modified(old).unwrap();
    drop(f2);
    // The fresh file changes within the same tick as far as size and mtime go.
    let mt = std::fs::metadata(&g).unwrap().modified().unwrap();
    std::fs::write(&g, "Termin Zahnarzt Mittwoch").unwrap();
    std::fs::File::options().write(true).open(&g).unwrap().set_modified(mt).unwrap();

    assert!(e.index_now().unwrap().generation.is_some(), "the edits make a new generation");
    assert!(top_path(&e, "Quittmus").is_some_and(|p| p.ends_with("notiz.txt")), "edited text is indexed");
    let r = ctxd::api::call(&e, "context_search", &json!({"query": "Apfelmus"}), Caller::Desktop).unwrap();
    assert!(r["items"].as_array().unwrap().iter().all(|i| !i["path"].as_str().unwrap().ends_with("notiz.txt")));
    assert!(top_path(&e, "Mittwoch").is_some_and(|p| p.ends_with("frisch.txt")));
}

/// Workspace edits from many connections at once lose nothing: read, change and save are one step.
#[test]
fn concurrent_workspace_edits_are_not_lost() {
    let d = tmp("ws-race");
    let root = d.join("docs");
    std::fs::create_dir_all(&root).unwrap();
    let n = 16;
    for i in 0..n {
        std::fs::write(root.join(format!("datei-{i}.txt")), format!("Notiz Nummer {i} über Wolkenkratzer {i}"))
            .unwrap();
    }
    let e = engine(&d, &root);
    e.index_now().unwrap();
    let docs: Vec<String> = e.generation().unwrap().cat.docs.iter().map(|d| d.id.clone()).collect();
    assert_eq!(docs.len(), n);
    let ws = ctxd::api::call(&e, "workspace_open", &json!({"name": "Hochhaus"}), Caller::Desktop).unwrap();
    let ws = ws["workspace"]["id"].as_str().unwrap().to_string();
    std::thread::scope(|s| {
        for (i, doc) in docs.iter().enumerate() {
            let (e, ws) = (&e, &ws);
            s.spawn(move || {
                let caller = if i % 2 == 0 { Caller::Desktop } else { Caller::Mcp };
                ctxd::api::call(e, "workspace_add", &json!({"workspace": ws, "docs": [doc]}), caller).unwrap();
                ctxd::api::call(e, "workspace_open", &json!({"workspace": ws}), caller).unwrap();
                ctxd::api::call(e, "workspace_pin", &json!({"workspace": ws, "pinned": i % 3 == 0}), caller).unwrap();
            });
        }
    });
    let w = e.ws.get(&ws).unwrap();
    let manual = w.items.iter().filter(|i| i.how == "manual").count();
    assert_eq!(manual, n, "every hand-added item survives concurrent edits: {:?}", w.items);
    // The journal holds one line per edit (create + 3 per thread), each with the true state before it.
    assert_eq!(e.ws.journal_lines(), 1 + 3 * n);
}
