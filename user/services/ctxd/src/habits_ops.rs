//! DB4: the memory's operations on the socket (C4 of the Durchblick contracts; the desktop's own, `Caller::Desktop`
//! only: none of it is an MCP tool, so no AI reads the habits). `learned_list` is the one page "Was BitsOS über
//! dich gelernt hat": the habits, the file learning (`learn.rs`), the search memory and the workspaces.

use crate::api::ApiError;
use crate::engine::Engine;
use crate::habits::PARTS;
use crate::habits_rules::{self as rules, Apply};
use serde_json::{Value, json};

pub const OPS: &[&str] = &["habit_event", "learned_list", "learned_forget", "learned_switch", "suggest", "history", "habit_rules"];
/// The page shows at most this many documents of the file learning.
const DOCS_MAX: usize = 30;
/// A workspace used this recently may be suggested.
const WORKSPACE_RECENT_S: i64 = 7 * 86_400;

fn bad(m: impl Into<String>) -> ApiError {
    ApiError { code: "bad_request", message: m.into() }
}

fn s<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// `desktop`: the caller is bitscomp (`peer::desktop`); only it speaks for Georg's own Vorrang input.
pub fn call(e: &Engine, op: &str, a: &Value, desktop: bool) -> Option<Result<Value, ApiError>> {
    if !OPS.contains(&op) {
        return None;
    }
    // The desktop asks every 30 s: old records are dropped from disk at most once a day.
    e.habits.prune_due();
    let empty = json!({});
    let a = if a.is_object() { a } else { &empty };
    Some(match op {
        "habit_event" => crate::habits_vorrang::screened(a, desktop).and_then(|a| e.habits.event(&a)).map(|_| json!({"ok": true})).map_err(bad),
        "learned_list" => Ok(list(e)),
        "learned_forget" => forget(e, a),
        "learned_switch" => switch(e, a),
        "suggest" => Ok(suggest(e, a)),
        "history" => {
            let since = a.get("since_s").and_then(Value::as_i64).unwrap_or(300).clamp(1, 3600);
            let max = a.get("max").and_then(Value::as_u64).unwrap_or(50).min(500) as usize;
            Ok(json!(e.habits.history(since, max)))
        }
        "habit_rules" => Ok(habit_rules(e)),
        _ => return None,
    })
}

/// What the desktop imitates at a window's start (C4 addition `habit_rules`), and the picks' ranks.
fn habit_rules(e: &Engine) -> Value {
    let h = &e.habits;
    let recs = h.records();
    let now = h.now();
    let mut rules_out = Vec::new();
    for it in rules::items(&recs, now) {
        let Some(apply) = &it.apply else { continue };
        if !h.on(&it.part) {
            continue;
        }
        let mut r = json!({"id": it.id, "app": it.app, "title": it.title, "reason": it.reason, "hinted": it.hinted});
        match apply {
            Apply::State(s) => r["state"] = json!(s),
            Apply::Width(w) => r["width"] = json!(w),
            Apply::Vorrang { value, ask } => {
                r["vorrang"] = json!(value);
                r["ask"] = json!(ask);
            }
        }
        rules_out.push(r);
    }
    let ranks = if h.on("auswahl") { rules::ranks(&recs, now) } else { Default::default() };
    json!({"enabled": h.on("enabled"), "rules": rules_out, "ranks": ranks})
}

fn title_of(e: &Engine, doc: &str) -> String {
    e.generation()
        .and_then(|g| g.by_id.get(doc).map(|i| g.cat.docs[*i as usize].title.clone()))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "ein Dokument, das nicht mehr da ist".into())
}

fn list(e: &Engine) -> Value {
    let h = &e.habits;
    let parts: Vec<Value> = PARTS.iter().map(|(p, n)| json!({"part": p, "name": n, "on": h.switch(p)})).collect();
    let mut entries: Vec<Value> = Vec::new();
    let recs = h.records();
    let items = rules::items(&recs, h.now());
    for (p, _) in PARTS {
        for it in items.iter().filter(|i| i.part == p) {
            let mut x = json!({"id": it.id, "part": it.part, "title": it.title, "reason": it.reason, "state": it.state, "count": it.count});
            if it.part == crate::habits_vorrang::PART {
                vorrang_fields(&mut x, it, &recs);
            }
            entries.push(x);
        }
    }
    for (doc, partners) in e.learn.documents(DOCS_MAX) {
        let Some((first, n)) = partners.first() else { continue };
        let more = match partners.len() - 1 {
            0 => String::new(),
            1 => " und einem weiteren".into(),
            k => format!(" und {k} weiteren"),
        };
        entries.push(json!({
            "id": format!("dateien:{doc}"), "part": "dateien", "state": "gelernt", "count": n,
            "title": format!("„{}“ gehört zu „{}“{more}", title_of(e, &doc), title_of(e, first)),
            "reason": format!("weil du sie {} zusammen geöffnet hast", if *n == 1 { "1 Mal".to_string() } else { format!("{n} Mal") }),
        }));
    }
    let n = e.learn.searches();
    if n > 0 {
        entries.push(json!({
            "id": "suche:alle", "part": "suche", "state": "gelernt", "count": n,
            "title": format!("Suchgedächtnis: {n} Suchen"),
            "reason": "damit eine Suche, die du oft machst, findet, was du danach geöffnet hast (gemerkt nur als Prüfzahl, nie der Text)",
        }));
    }
    for w in e.ws.list() {
        entries.push(json!({
            "id": format!("arbeitsbereich:{}", w.id), "part": "arbeitsbereiche", "state": "gelernt", "count": w.items.len(),
            "title": format!("Arbeitsbereich „{}“", w.name),
            "reason": format!("weil du ihn angelegt hast; {} Dokumente, zuletzt benutzt am {}", w.items.len(), crate::util::iso_time(w.last_used).get(..10).unwrap_or("")),
        }));
    }
    json!({"enabled": h.switch("enabled"), "parts": parts, "entries": entries})
}

/// K2's entry `{kind:"vorrang", app, value, reason}` (optional fields beside DB4's): the learned value, or for a
/// declined one the value Georg set last. `suggest` has no such entry: it offers programs to start now, and a
/// Vorrang is applied at a program's start (`habit_rules` carries it for bitscomp).
fn vorrang_fields(x: &mut Value, it: &rules::Item, recs: &[crate::habits::Rec]) {
    let value = match &it.apply {
        Some(Apply::Vorrang { value, .. }) => Some(value.clone()),
        _ => recs.iter().filter(|r| r.part == crate::habits_vorrang::PART && r.key == it.app).max_by_key(|r| r.ts).and_then(|r| r.val.clone()),
    };
    x["kind"] = json!(crate::habits_vorrang::PART);
    x["app"] = json!(it.app);
    if let Some(v) = value {
        x["value"] = json!(v);
    }
}

fn forget(e: &Engine, a: &Value) -> Result<Value, ApiError> {
    let id = s(a, "id").ok_or_else(|| bad("id is required (an entry's id, or all)"))?;
    // A forget the disk refused is no forget: the page says it could not delete (it would come back otherwise).
    let disk = |m: String| ApiError { code: "internal", message: m };
    let n = if id == "all" {
        // Everything learned goes; workspaces are Georg's own making and stay (the page's dialog says so).
        let habits = e.habits.forget_all().map_err(disk)? as u64;
        habits + e.learn.forget()
    } else if let Some(doc) = id.strip_prefix("dateien:") {
        e.learn.forget_doc(doc).map_err(disk)?
    } else if id == "suche:alle" {
        e.learn.forget_searches().map_err(disk)?
    } else if let Some(w) = id.strip_prefix("arbeitsbereich:") {
        u64::from(e.ws.delete(w).map_err(|x| disk(x.to_string()))?)
    } else {
        e.habits.forget(id).map_err(disk)? as u64
    };
    if n == 0 && id != "all" {
        return Err(ApiError { code: "not_found", message: format!("nothing learned under {:?}", crate::util::cut_bytes(id, 80)) });
    }
    crate::event!("warn", "habits.forget", part = id.split(':').next().unwrap_or(""), records = n);
    Ok(json!({"forgotten": n}))
}

fn switch(e: &Engine, a: &Value) -> Result<Value, ApiError> {
    let part = s(a, "part").ok_or_else(|| bad("part is required"))?;
    if part != "enabled" && !PARTS.iter().any(|(p, _)| *p == part) {
        return Err(bad(format!("unknown part {:?}", crate::util::cut_bytes(part, 40))));
    }
    let on = a.get("on").and_then(Value::as_bool).ok_or_else(|| bad("on must be true or false"))?;
    apply_switch(e, part, on);
    let kept = crate::habits_settings::store(part, on);
    Ok(json!({"part": part, "on": on, "kept": kept}))
}

/// One switch, applied at once (also from the settings watcher).
pub fn apply_switch(e: &Engine, part: &str, on: bool) {
    e.habits.set_switch(part, on);
    e.learn.set_switches(e.habits.on("dateien"), e.habits.on("suche"));
}

fn suggest(e: &Engine, a: &Value) -> Value {
    let h = &e.habits;
    if !h.on("enabled") {
        return json!([]);
    }
    let now = a.get("now").and_then(Value::as_i64).unwrap_or_else(|| h.now());
    let hour = a.get("hour").and_then(Value::as_u64).filter(|h| *h < 24).map_or(((now.rem_euclid(86_400)) / 3600) as u8, |h| h as u8);
    let open: Vec<String> = a.get("open_apps").and_then(Value::as_array).map(|v| v.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let recs: Vec<_> = h.records().into_iter().filter(|r| h.on(&r.part)).collect();
    let mut out: Vec<Value> = rules::suggest(&recs, h.now(), hour, &open)
        .into_iter()
        .map(|s| {
            let mut v = json!({"kind": s.kind, "name": s.name, "reason": s.reason});
            if let Some(a) = s.app {
                v["app"] = json!(a);
            }
            if let Some(x) = s.action {
                v["action"] = json!(x);
            }
            if !s.apps.is_empty() {
                v["apps"] = json!(s.apps);
            }
            v
        })
        .collect();
    if out.len() < rules::SUGGEST_MAX
        && let Some(w) = e.ws.list().into_iter().filter(|w| now - w.last_used <= WORKSPACE_RECENT_S).max_by_key(|w| w.last_used)
    {
        out.push(json!({"kind": "workspace", "action": format!("workspace:{}", w.id), "name": w.name,
                        "reason": format!("weil du diesen Arbeitsbereich zuletzt am {} benutzt hast", crate::util::iso_time(w.last_used).get(..10).unwrap_or(""))}));
    }
    json!(out)
}
