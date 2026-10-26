//! The operations, written once (DESIGN §2): the socket, the CLI and the MCP tools are thin doors over
//! `call`. Privacy (DESIGN §9.3, Georg 15.10.): for `Caller::Mcp` documents under private roots do not
//! exist (not in results, workspaces, status or explain), and snippets are short.

use crate::engine::{Caller, Engine, Generation, Hit, SearchReq};
use crate::snippet;
use crate::workspace::{Item, Workspace};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub code: &'static str,
    pub message: String,
}

fn err(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError { code, message: message.into() }
}

type R = Result<Value, ApiError>;

pub const OPS: &[&str] = &[
    "context_search",
    "workspace_open",
    "workspace_add",
    "workspace_list",
    "workspace_rename",
    "workspace_pin",
    "dismiss",
    "context_status",
    "context_explain",
    "context_forget",
];

pub fn canonical(op: &str) -> Option<&'static str> {
    let alias = match op {
        "search" => "context_search",
        "status" => "context_status",
        "explain" => "context_explain",
        "forget" => "context_forget",
        o => o,
    };
    OPS.iter().copied().find(|o| *o == alias)
}

pub fn call(e: &Engine, op: &str, args: &Value, caller: Caller) -> R {
    let args = if args.is_null() { &json!({}) } else { args };
    if !args.is_object() {
        return Err(err("bad_request", "arguments must be an object"));
    }
    match canonical(op) {
        Some("context_search") => search(e, args, caller),
        Some("workspace_open") => ws_open(e, args, caller),
        Some("workspace_add") => ws_add(e, args, caller),
        Some("workspace_list") => ws_list(e, args, caller),
        Some("workspace_rename") => ws_rename(e, args, caller),
        Some("workspace_pin") => ws_pin(e, args, caller),
        Some("dismiss") => dismiss(e, args, caller),
        Some("context_status") => status(e, caller),
        Some("context_explain") => explain(e, args, caller),
        Some("context_forget") => forget(e, args, caller),
        _ => Err(err("bad_request", format!("unknown op {:?}", crate::util::cut_bytes(op, 64)))),
    }
}

fn s_arg<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn u_arg(a: &Value, k: &str, default: usize) -> Result<usize, ApiError> {
    match a.get(k) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_u64().map(|n| n as usize).ok_or_else(|| err("bad_request", format!("{k} must be a number"))),
    }
}

fn str_list(a: &Value, k: &str) -> Result<Option<Vec<String>>, ApiError> {
    match a.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(v)) => Ok(Some(v.iter().filter_map(|x| x.as_str().map(String::from)).collect())),
        Some(Value::String(s)) => Ok(Some(vec![s.clone()])),
        _ => Err(err("bad_request", format!("{k} must be a list of strings"))),
    }
}

fn snippet_chars(e: &Engine, a: &Value, caller: Caller) -> usize {
    let snippets = a.get("snippets").and_then(Value::as_bool).unwrap_or(true);
    if !snippets {
        return 0;
    }
    let asked = a.get("snippet_chars").and_then(Value::as_u64).map(|n| n as usize);
    match caller {
        // Claude never gets more than the setting (it can ask for less).
        Caller::Mcp => asked.map_or(e.cfg.claude.snippet_chars, |n| n.min(e.cfg.claude.snippet_chars)),
        Caller::Desktop => asked.unwrap_or(e.cfg.search.desktop_snippet_chars),
    }
}

fn deadline(e: &Engine, a: &Value, caller: Caller) -> Duration {
    let d = match caller {
        Caller::Mcp => e.cfg.search.mcp_deadline_ms,
        Caller::Desktop => e.cfg.search.deadline_ms,
    };
    Duration::from_millis(a.get("deadline_ms").and_then(Value::as_u64).unwrap_or(d).max(1))
}

fn age_text(secs: i64) -> String {
    let days = (crate::util::now_secs() - secs).max(0) / 86_400;
    match days {
        0 => "Zuletzt geändert heute".into(),
        1 => "Zuletzt geändert gestern".into(),
        d => format!("Zuletzt geändert vor {d} Tagen"),
    }
}

/// One result as the outside sees it.
fn item_json(e: &Engine, g: &Generation, h: &Hit, query: &str, chars: usize, explain: bool, caller: Caller) -> Value {
    let d = &g.cat.docs[h.doc as usize];
    let rc = &e.cfg.rank;
    let mut it = json!({
        "doc": d.id, "title": d.title, "path": d.paths[0].path, "kind": d.kind,
        "modified": crate::util::iso_time(d.mtime), "score": (crate::rank::shown(h.s) as f64 * 1000.0).round() / 1000.0,
    });
    if d.paths.len() > 1 {
        it["also_in"] = json!(d.paths[1..].iter().map(|p| p.path.clone()).collect::<Vec<_>>());
    }
    if caller == Caller::Desktop && d.status != "ok" {
        it["status"] = json!(d.status);
    }
    // The snippet comes from the chunk that contributed most.
    let v_part = h.parts.v.unwrap_or(0.0) * rc.w_v;
    let chunk = if h.parts.b * rc.w_b >= v_part { h.b_chunk.or(h.v_chunk) } else { h.v_chunk.or(h.b_chunk) };
    let text = if chars > 0 || explain { e.doc_text(g, h.doc) } else { String::new() };
    let ctext = chunk.map(|c| g.chunk_text(&text, c)).unwrap_or("");
    if chars > 0
        && let Some(s) = snippet::make(ctext, query, chars)
    {
        it["snippet"] = json!(s);
    }
    if explain {
        let mut why = Vec::new();
        if let Some(v) = h.parts.v.filter(|v| *v >= rc.v_min) {
            let (sec, of) = chunk.map(|c| (c - d.chunk_start + 1, d.chunk_count)).unwrap_or((1, 1));
            why.push(json!({"part": "text", "text": format!("Passt inhaltlich zu „{}“ (Abschnitt {sec} von {of}, Ähnlichkeit {:.2})", snippet::cut_chars(query, 60), v)}));
        }
        if h.parts.b >= rc.b_min {
            let mut dw = crate::tokenize::words(&d.title);
            dw.extend(crate::tokenize::words(ctext));
            // Show Georg's own words, matched in folded form (also inside compounds).
            let mut found: Vec<&str> = Vec::new();
            for qw in query.split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 2) {
                let f = crate::tokenize::fold(qw);
                if !crate::tokenize::words(qw).is_empty()
                    && dw.iter().any(|w| w.contains(f.as_str()))
                    && !found.contains(&qw)
                {
                    found.push(qw);
                }
            }
            found.truncate(5);
            let txt = if found.is_empty() {
                "Ähnliche Wortteile im Text".to_string()
            } else {
                format!("Stichwörter: {}", found.join(", "))
            };
            why.push(json!({"part": "keyword", "text": txt}));
        }
        why.push(json!({"part": "recency", "text": age_text(d.mtime)}));
        it["why"] = json!(why);
    }
    it
}

fn search(e: &Engine, a: &Value, caller: Caller) -> R {
    let query = s_arg(a, "query").ok_or_else(|| err("bad_request", "query is required"))?;
    let limit = u_arg(a, "limit", 10)?;
    let since = match s_arg(a, "since") {
        Some(s) => Some(crate::util::parse_iso_date(s).ok_or_else(|| err("bad_request", "since must be YYYY-MM-DD"))?),
        None => None,
    };
    let explain = a.get("explain").and_then(Value::as_bool).unwrap_or(true);
    let req = SearchReq {
        query: query.to_string(),
        limit,
        kinds: str_list(a, "kinds")?,
        roots: str_list(a, "roots")?,
        since,
        caller,
        deadline: deadline(e, a, caller),
        hidden: vec![],
        force_doc: None,
    };
    let out = e.search(&req);
    let g = e.generation();
    let chars = snippet_chars(e, a, caller);
    let items: Vec<Value> = match &g {
        Some(g) => out.hits.iter().map(|h| item_json(e, g, h, query, chars, explain, caller)).collect(),
        None => vec![],
    };
    let mut r = json!({ "items": items, "took_ms": out.took_ms, "embedder": out.embedder });
    if let Some(p) = out.partial {
        r["partial"] = json!(true);
        r["partial_reason"] = json!(p);
    }
    Ok(r)
}

/// Group labels by a small rule table (DESIGN §7.1 "Grouping"): kind first for mails, then words in the
/// title, then the kind.
pub fn group_label(kind: &str, title: &str) -> &'static str {
    let t = crate::tokenize::fold(title);
    let has = |ws: &[&str]| ws.iter().any(|w| t.contains(w));
    if kind == "mail" {
        "Mails"
    } else if has(&["rechnung", "quittung", "beleg", "kassenbon", "spendenbescheinigung", "zuwendungsbestaetigung"]) {
        "Belege"
    } else if has(&["bescheid", "schreiben", "brief", "mitteilung", "kuendigung", "bestaetigung"]) {
        "Briefe"
    } else if has(&["formular", "antrag", "vordruck", "anlage"]) {
        "Formulare"
    } else {
        match kind {
            "pdf" | "docx" | "odt" => "Dokumente",
            "md" | "text" => "Notizen",
            "html" => "Webseiten",
            _ => "Weitere",
        }
    }
}

fn doc_visible(g: &Generation, id: &str, caller: Caller) -> Option<u32> {
    let i = *g.by_id.get(id)?;
    (caller == Caller::Desktop || !g.cat.docs[i as usize].private).then_some(i)
}

fn ws_view(e: &Engine, w: &Workspace, caller: Caller, ranked: &[Hit], query: &str) -> Value {
    let g = e.generation();
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    let mut hidden = Vec::new();
    if let Some(g) = &g {
        let chars = match caller {
            Caller::Mcp => e.cfg.claude.snippet_chars,
            Caller::Desktop => e.cfg.search.desktop_snippet_chars,
        };
        for item in &w.items {
            let Some(i) = doc_visible(g, &item.doc, caller) else { continue };
            let d = &g.cat.docs[i as usize];
            let mut v = match ranked.iter().find(|h| h.doc == i) {
                Some(h) => item_json(e, g, h, query, chars, true, caller),
                None => json!({"doc": d.id, "title": d.title, "path": d.paths[0].path, "kind": d.kind,
                    "modified": crate::util::iso_time(d.mtime)}),
            };
            if item.how == "manual" {
                let when = crate::util::iso_time(item.added);
                let why = json!({"part": "manual", "text": format!("Von Hand hinzugefügt am {}.{}.", &when[8..10], &when[5..7])});
                match v.get_mut("why").and_then(Value::as_array_mut) {
                    Some(a) => a.insert(0, why),
                    None => v["why"] = json!([why]),
                }
            }
            let label = group_label(&d.kind, &d.title).to_string();
            match groups.iter_mut().find(|(l, _)| *l == label) {
                Some((_, items)) => items.push(v),
                None => groups.push((label, vec![v])),
            }
        }
        for id in &w.dismissed {
            if let Some(i) = doc_visible(g, id, caller) {
                hidden.push(json!({"doc": id, "title": g.cat.docs[i as usize].title}));
            }
        }
    }
    json!({
        "id": w.id, "name": w.name, "pinned": w.pinned, "created": crate::util::iso_time(w.created),
        "groups": groups.into_iter().map(|(label, items)| json!({"label": label, "items": items})).collect::<Vec<_>>(),
        "hidden": hidden,
    })
}

fn ws_id(a: &Value) -> Result<&str, ApiError> {
    s_arg(a, "workspace").ok_or_else(|| err("bad_request", "workspace is required"))
}

/// Read, change and save one workspace as a single step (see `WorkspaceStore::transact`). `f` returns the
/// answer and whether anything changed; nothing is written when it did not.
fn ws_edit(e: &Engine, id: &str, op: &str, f: impl FnOnce(&mut Workspace) -> Result<(Value, bool), ApiError>) -> R {
    e.ws.transact(|t| {
        let mut w = t.get(id).ok_or_else(|| err("not_found", "no such workspace"))?;
        let (v, changed) = f(&mut w)?;
        if changed {
            t.save(&w, op).map_err(|x| err("internal", format!("save: {x}")))?;
        }
        Ok(v)
    })
    .map_err(|x| err("internal", format!("workspaces: {x}")))?
}

fn new_workspace(name: &str, query: Option<&str>, now: i64) -> Workspace {
    Workspace {
        id: crate::util::new_id("ws"),
        name: name.to_string(),
        query: Some(query.unwrap_or(name).to_string()),
        pinned: false,
        created: now,
        last_used: now,
        items: vec![],
        dismissed: vec![],
    }
}

fn ws_open(e: &Engine, a: &Value, caller: Caller) -> R {
    let limit = u_arg(a, "limit", 20)?;
    let now = crate::util::now_secs();
    let by_id = s_arg(a, "workspace");
    let name = match by_id {
        Some(_) => String::new(),
        None => snippet::cut_chars(
            s_arg(a, "name")
                .or_else(|| s_arg(a, "query"))
                .ok_or_else(|| err("bad_request", "name, workspace or query is required"))?,
            200,
        ),
    };
    let lookup = |t: &dyn Fn(&str) -> Option<Workspace>, f: &dyn Fn(&str) -> Option<Workspace>| match by_id {
        Some(id) => t(id).ok_or_else(|| err("not_found", "no such workspace")),
        None => Ok(f(&name).unwrap_or_else(|| new_workspace(&name, s_arg(a, "query"), now))),
    };
    // The search runs without the lock (it can take the whole deadline); only the merge below holds it.
    let seen = lookup(&|id| e.ws.get(id), &|n| e.ws.find_by_name(n))?;
    let query = seen.query.clone().unwrap_or_else(|| seen.name.clone());
    let req = SearchReq {
        query: query.clone(),
        limit,
        kinds: None,
        roots: None,
        since: None,
        caller,
        deadline: deadline(e, a, caller),
        hidden: seen.dismissed.clone(),
        force_doc: None,
    };
    let out = e.search(&req);
    let g = e.generation();
    let found: Vec<String> = match &g {
        Some(g) => out.hits.iter().map(|h| g.cat.docs[h.doc as usize].id.clone()).collect(),
        None => Vec::new(),
    };
    let w =
        e.ws.transact(|t| -> Result<Workspace, ApiError> {
            // Re-read under the lock: an edit made while the search ran is kept, not overwritten.
            let mut w = lookup(&|id| t.get(id), &|n| t.find_by_name(n))?;
            if let Some(g) = &g {
                // Refresh the found part with the workspace's query; hand-added items stay.
                let old = std::mem::take(&mut w.items);
                let mut items: Vec<Item> = old.iter().filter(|i| i.how == "manual").cloned().collect();
                // Items Georg sees elsewhere (private, over MCP) were found by a desktop search: keep them.
                items.extend(
                    old.iter()
                        .filter(|i| {
                            i.how != "manual" && caller == Caller::Mcp && doc_visible(g, &i.doc, caller).is_none()
                        })
                        .cloned(),
                );
                for id in &found {
                    if !items.iter().any(|i| i.doc == *id) && !w.dismissed.contains(id) {
                        items.push(Item { doc: id.clone(), how: "search".into(), added: now });
                    }
                }
                w.items = items;
            }
            w.last_used = now;
            t.save(&w, "open").map_err(|x| err("internal", format!("save: {x}")))?;
            Ok(w)
        })
        .map_err(|x| err("internal", format!("workspaces: {x}")))??;
    let ranked = if g.is_some() { out.hits } else { Vec::new() };
    let mut v = ws_view(e, &w, caller, &ranked, &query);
    if let Some(p) = out.partial {
        v["partial"] = json!(true);
        v["partial_reason"] = json!(p);
    }
    Ok(json!({ "workspace": v }))
}

fn ws_add(e: &Engine, a: &Value, caller: Caller) -> R {
    let id = ws_id(a)?;
    if e.ws.get(id).is_none() {
        return Err(err("not_found", "no such workspace"));
    }
    let g = e.generation().ok_or_else(|| err("busy", "index not built yet"))?;
    let now = crate::util::now_secs();
    let mut targets: Vec<(String, Option<u32>)> = Vec::new();
    for id in str_list(a, "docs")?.unwrap_or_default() {
        let i = doc_visible(&g, &id, caller);
        targets.push((id, i));
    }
    for p in str_list(a, "paths")?.unwrap_or_default() {
        let canon = std::fs::canonicalize(&p).ok().map(|c| c.display().to_string());
        let i = g
            .cat
            .docs
            .iter()
            .position(|d| d.paths.iter().any(|x| x.path == p || Some(&x.path) == canon.as_ref()))
            .map(|i| i as u32);
        let i = i.filter(|&i| caller == Caller::Desktop || !g.cat.docs[i as usize].private);
        targets.push((p, i));
    }
    if targets.is_empty() {
        return Err(err("bad_request", "docs or paths is required"));
    }
    ws_edit(e, id, "add", |w| {
        let mut added = Vec::new();
        let mut skipped = Vec::new();
        for (what, i) in targets {
            // Over MCP a private document is "not found", never "private": its existence is not revealed.
            let Some(i) = i else {
                skipped.push(json!({"item": what, "reason": "not_found"}));
                continue;
            };
            let id = g.cat.docs[i as usize].id.clone();
            if w.items.iter().any(|x| x.doc == id && x.how == "manual") {
                skipped.push(json!({"item": what, "reason": "already_in"}));
                continue;
            }
            w.items.retain(|x| x.doc != id);
            w.dismissed.retain(|x| *x != id);
            w.items.push(Item { doc: id.clone(), how: "manual".into(), added: now });
            added.push(id);
        }
        w.last_used = now;
        let changed = !added.is_empty();
        Ok((json!({"added": added, "skipped": skipped}), changed))
    })
}

fn ws_list(e: &Engine, a: &Value, caller: Caller) -> R {
    let pinned_only = a.get("pinned_only").and_then(Value::as_bool).unwrap_or(false);
    let q = s_arg(a, "query").map(crate::tokenize::fold);
    let g = e.generation();
    let list: Vec<Value> = e
        .ws
        .list()
        .into_iter()
        .filter(|w| !pinned_only || w.pinned)
        .filter(|w| q.as_ref().is_none_or(|q| crate::tokenize::fold(&w.name).contains(q.as_str())))
        .map(|w| {
            let n = match &g {
                Some(g) => w.items.iter().filter(|i| doc_visible(g, &i.doc, caller).is_some()).count(),
                None => 0,
            };
            json!({"id": w.id, "name": w.name, "pinned": w.pinned, "items": n, "last_used": crate::util::iso_time(w.last_used)})
        })
        .collect();
    Ok(json!({"workspaces": list}))
}

fn ws_rename(e: &Engine, a: &Value, _caller: Caller) -> R {
    let id = ws_id(a)?;
    let name = snippet::cut_chars(s_arg(a, "name").ok_or_else(|| err("bad_request", "name is required"))?, 200);
    ws_edit(e, id, "rename", |w| {
        w.name = name;
        Ok((json!({"id": w.id, "name": w.name}), true))
    })
}

fn ws_pin(e: &Engine, a: &Value, _caller: Caller) -> R {
    let id = ws_id(a)?;
    let pinned = a.get("pinned").and_then(Value::as_bool).unwrap_or(true);
    ws_edit(e, id, "pin", |w| {
        w.pinned = pinned;
        Ok((json!({"id": w.id, "pinned": w.pinned}), true))
    })
}

fn dismiss(e: &Engine, a: &Value, caller: Caller) -> R {
    let id = ws_id(a)?;
    let doc = s_arg(a, "doc").ok_or_else(|| err("bad_request", "doc is required"))?.to_string();
    let scope = s_arg(a, "scope").unwrap_or("workspace");
    if scope != "workspace" {
        return Err(err("bad_request", "scope query/everywhere comes with the learning step (CX-3)"));
    }
    let undo = a.get("undo").and_then(Value::as_bool).unwrap_or(false);
    let g = e.generation();
    ws_edit(e, id, if undo { "undismiss" } else { "dismiss" }, |w| {
        if let Some(g) = &g
            && doc_visible(g, &doc, caller).is_none()
            && !w.dismissed.contains(&doc)
        {
            return Err(err("not_found", "no such document"));
        }
        if undo {
            w.dismissed.retain(|d| *d != doc);
        } else {
            w.items.retain(|i| i.doc != doc);
            if !w.dismissed.contains(&doc) {
                w.dismissed.push(doc.clone());
            }
        }
        Ok((json!({"ok": true}), true))
    })
}

fn status(e: &Engine, caller: Caller) -> R {
    let g = e.generation();
    let mut by_kind = serde_json::Map::new();
    let mut by_status = serde_json::Map::new();
    let mut failed = Vec::new();
    let mut roots = Vec::new();
    let (mut docs, mut chunks) = (0usize, 0usize);
    if let Some(g) = &g {
        let mut per_root = vec![0u64; g.cat.roots.len()];
        for d in &g.cat.docs {
            for p in &d.paths {
                if let Some(c) = per_root.get_mut(p.root) {
                    *c += 1;
                }
            }
            if caller == Caller::Mcp && d.private {
                continue;
            }
            docs += 1;
            chunks += d.chunk_count as usize;
            *by_kind.entry(d.kind.clone()).or_insert(json!(0)) =
                json!(by_kind.get(&d.kind).and_then(Value::as_u64).unwrap_or(0) + 1);
            let st = d.status.split(':').next().unwrap_or("").to_string();
            *by_status.entry(st.clone()).or_insert(json!(0)) =
                json!(by_status.get(&st).and_then(Value::as_u64).unwrap_or(0) + 1);
            if d.status.starts_with("failed") {
                failed.push(json!({"doc": d.id, "title": d.title, "path": d.paths[0].path, "reason": d.status}));
            }
        }
        for (i, r) in g.cat.roots.iter().enumerate() {
            if r.private && caller == Caller::Mcp {
                // Claude learns that private roots exist (so "why is X missing" has an answer), never where.
                roots.push(json!({"private": true}));
            } else {
                roots.push(json!({"path": r.path, "private": r.private, "files": per_root[i]}));
            }
        }
    }
    let health = e.embedder.as_ref().map(|em| match em.health() {
        crate::embed::Health::Ready => "ready".to_string(),
        crate::embed::Health::Loading => "loading".to_string(),
        crate::embed::Health::Unavailable(r) => format!("unavailable: {r}"),
    });
    let mut v = json!({
        "roots": roots,
        "docs": docs, "chunks": chunks, "by_kind": by_kind, "by_status": by_status, "failed": failed,
        "generation": g.as_ref().map(|g| g.n), "built": g.as_ref().map(|g| crate::util::iso_time(g.cat.built)),
        "embedder": {"id": e.embedder.as_ref().map(|x| x.id().to_string()), "health": health, "error": e.embedder_error,
                     "index_embedder": g.as_ref().and_then(|g| g.cat.embedder.clone()),
                     "hnsw": g.as_ref().and_then(|g| g.vec.as_ref().map(|v| v.hnsw_ready()))},
        "indexing": e.progress.json(),
        "metrics": crate::event::METRICS.snapshot(),
        "workspaces": e.ws.list().len(),
    });
    if caller == Caller::Desktop {
        v["denied_roots"] = json!(g.as_ref().map(|g| g.cat.denied.clone()).unwrap_or_default());
        v["data"] = json!(e.data.display().to_string());
        v["scan"] = g.as_ref().map(|g| g.cat.scan.clone()).unwrap_or(Value::Null);
    }
    Ok(v)
}

fn explain(e: &Engine, a: &Value, caller: Caller) -> R {
    let id = s_arg(a, "doc").ok_or_else(|| err("bad_request", "doc is required"))?;
    let query = s_arg(a, "query").ok_or_else(|| err("bad_request", "query is required"))?;
    let g = e.generation().ok_or_else(|| err("busy", "index not built yet"))?;
    let i = doc_visible(&g, id, caller).ok_or_else(|| err("not_found", "no such document"))?;
    let req = SearchReq {
        query: query.to_string(),
        limit: e.cfg.rank.candidates,
        kinds: None,
        roots: None,
        since: None,
        caller,
        deadline: deadline(e, a, caller),
        hidden: vec![],
        force_doc: Some(i),
    };
    let out = e.search(&req);
    let rank = out.hits.iter().filter(|h| h.gate).position(|h| h.doc == i).map(|p| p + 1);
    let h = out.hits.iter().find(|h| h.doc == i).ok_or_else(|| err("internal", "forced document missing"))?;
    let c = g.cat.calib;
    let rc = &e.cfg.rank;
    let d = &g.cat.docs[i as usize];
    Ok(json!({
        "doc": d.id, "title": d.title, "query": query, "rank": rank, "gate": h.gate, "partial": out.partial,
        "S": h.s, "score": crate::rank::shown(h.s),
        "parts": {
            "V": {"cos": h.cos, "floor": c.floor, "ceil": c.ceil, "value": h.parts.v, "v_min": rc.v_min},
            "B": {"bm25": h.bm25, "value": h.parts.b, "b_min": rc.b_min},
            "C": {"value": null, "note": "usage learning comes with CX-3"},
            "R": {"age_days": (crate::util::now_secs() - d.mtime) as f32 / 86_400.0, "t_half_days": rc.t_half_days, "value": h.parts.r},
            "X": {"value": h.parts.x},
        },
        "weights": out.weights.map(|w| json!({"v": w.v, "b": w.b, "c": w.c, "r": w.r, "x": w.x})),
        "matching_chunks": h.matching, "candidates": out.candidates, "embedder": out.embedder, "fusion": rc.fusion,
    }))
}

fn forget(e: &Engine, a: &Value, caller: Caller) -> R {
    if caller == Caller::Mcp && !e.cfg.claude.allow_forget {
        return Err(err(
            "denied",
            "context_forget is delete_overwrite: Georg's OK needed (context.claude.allow_forget)",
        ));
    }
    let scope = s_arg(a, "scope")
        .ok_or_else(|| err("bad_request", "scope is required (document, workspace, usage, index, all)"))?;
    let r = match scope {
        "document" => {
            let id = s_arg(a, "doc").ok_or_else(|| err("bad_request", "doc is required"))?;
            let g = e.generation().ok_or_else(|| err("busy", "index not built yet"))?;
            let i = doc_visible(&g, id, caller).ok_or_else(|| err("not_found", "no such document"))?;
            let hash = g.cat.docs[i as usize].hash.clone();
            let ws = e.ws.remove_doc_everywhere(id);
            e.store.drop_text(&hash);
            e.store.drop_vecs(&hash);
            json!({"workspaces_changed": ws, "note": "the file is still under a root; it returns at the next scan unless excluded"})
        }
        "workspace" => {
            let id = s_arg(a, "workspace").ok_or_else(|| err("bad_request", "workspace is required"))?;
            json!({"deleted": e.ws.delete(id).map_err(|x| err("internal", x.to_string()))?})
        }
        "usage" => json!({"files": e.store.wipe_dir("usage")}),
        "index" => {
            e.clear_generation();
            let n = e.store.wipe_derived();
            e.wake_indexer();
            json!({"files": n, "note": "rebuilt from the roots"})
        }
        "all" => {
            e.clear_generation();
            let n = e.store.wipe_derived() + e.store.wipe_dir("usage") + e.ws.wipe() as u64;
            e.wake_indexer();
            json!({"files": n})
        }
        _ => return Err(err("bad_request", "scope must be document, workspace, usage, index or all")),
    };
    crate::event!("warn", "forget", scope = scope);
    Ok(r)
}
