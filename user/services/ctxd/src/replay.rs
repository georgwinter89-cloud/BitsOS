//! CX-3: `ctxd replay FILE` (DESIGN §7.3, §11.4): recorded sessions run against the index without a person.
//! The file is `{"steps": [...]}` with steps `{"search": Q}`, `{"open": PATH, "workspace"?: NAME}`,
//! `{"add": [PATH...], "workspace": NAME}`, `{"dismiss": PATH, "workspace": NAME, "scope"?: S}`,
//! `{"wait_s": N}` / `{"wait_days": N}` (the learning clock moves forward), `{"forget": "usage"}` and
//! `{"report": Q, "label"?: L, "n"?: N}`, which records the top N (10; paths) without counting as Georg's search.
//! Paths are relative to the first root. The answer lists every report in order.

use crate::api;
use crate::engine::{Caller, Engine, SearchReq};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

fn ws_id(e: &Engine, name: &str) -> Result<String, String> {
    if let Some(w) = e.ws.find_by_name(name) {
        return Ok(w.id);
    }
    let v = api::call(e, "workspace_open", &json!({"name": name}), Caller::Desktop).map_err(|x| x.message)?;
    v["workspace"]["id"].as_str().map(str::to_string).ok_or_else(|| "workspace without id".into())
}

fn doc_of(e: &Engine, path: &str) -> Result<String, String> {
    let g = e.generation().ok_or("index not built")?;
    g.cat.docs.iter().find(|d| d.paths.iter().any(|p| p.path == path)).map(|d| d.id.clone()).ok_or_else(|| format!("{path}: not in the index"))
}

/// The top `n` (paths) for `q`, unrecorded.
pub fn ranked(e: &Engine, q: &str, n: usize) -> Vec<String> {
    let req = SearchReq {
        query: q.to_string(),
        limit: n,
        kinds: None,
        roots: None,
        since: None,
        caller: Caller::Desktop,
        deadline: Duration::from_secs(30),
        hidden: vec![],
        force_doc: None,
        anchors: vec![],
    };
    let Some(g) = e.generation() else { return Vec::new() };
    e.search(&req).hits.iter().map(|h| g.cat.docs[h.doc as usize].paths[0].path.clone()).collect()
}

pub fn run(e: &Engine, file: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(file).map_err(|x| format!("{}: {x}", file.display()))?;
    let v: Value = serde_json::from_str(&text).map_err(|x| format!("{}: {x}", file.display()))?;
    let base = e.cfg.roots.first().map(|r| std::fs::canonicalize(&r.path).unwrap_or_else(|_| r.path.clone())).ok_or("no root")?;
    let path = |p: &Value| p.as_str().map(|p| base.join(p).display().to_string()).ok_or("a path is a string");
    let mut reports = Vec::new();
    for (i, s) in v["steps"].as_array().ok_or("steps: want a list")?.iter().enumerate() {
        let at = |x: String| format!("step {}: {x}", i + 1);
        let ws = |s: &Value| s["workspace"].as_str().map(|n| ws_id(e, n)).transpose();
        if let Some(q) = s["search"].as_str() {
            api::call(e, "context_search", &json!({"query": q}), Caller::Desktop).map_err(|x| at(x.message))?;
        } else if s.get("open").is_some() {
            let doc = doc_of(e, &path(&s["open"])?).map_err(at)?;
            api::opened(e, &json!({"doc": doc, "workspace": ws(s).map_err(at)?})).map_err(|x| at(x.message))?;
        } else if let Some(list) = s["add"].as_array() {
            let id = ws(s).map_err(at)?.ok_or_else(|| at("add needs a workspace".into()))?;
            let paths: Vec<String> = list.iter().map(path).collect::<Result<_, _>>().map_err(|x| at(x.into()))?;
            api::call(e, "workspace_add", &json!({"workspace": id, "paths": paths}), Caller::Desktop).map_err(|x| at(x.message))?;
        } else if s.get("dismiss").is_some() {
            let id = ws(s).map_err(at)?.ok_or_else(|| at("dismiss needs a workspace".into()))?;
            let doc = doc_of(e, &path(&s["dismiss"])?).map_err(at)?;
            let scope = s["scope"].as_str().unwrap_or("workspace");
            api::call(e, "dismiss", &json!({"workspace": id, "doc": doc, "scope": scope}), Caller::Desktop).map_err(|x| at(x.message))?;
        } else if let Some(n) = s["wait_s"].as_i64() {
            e.learn.advance(n);
        } else if let Some(n) = s["wait_days"].as_f64() {
            e.learn.advance((n * 86_400.0) as i64);
        } else if s["forget"].as_str() == Some("usage") {
            e.learn.forget();
        } else if let Some(q) = s["report"].as_str() {
            let n = s["n"].as_u64().unwrap_or(10) as usize;
            let top: Vec<String> = ranked(e, q, n).into_iter().map(|p| p.strip_prefix(&format!("{}/", base.display())).unwrap_or(&p).to_string()).collect();
            reports.push(json!({"label": s["label"].as_str().unwrap_or(q), "query": q, "top": top}));
        } else {
            return Err(at(format!("unknown step {s}")));
        }
    }
    Ok(json!({"reports": reports, "events": e.learn.events()}))
}
