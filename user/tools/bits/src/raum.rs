//! `bits arbeitsraum ...` and `bits dateien suchen`: ctxd's workspaces and its index. From the assistant's tree the
//! call goes through ctxd's MCP door (`op: mcp`), with exactly the privacy the assistant's own context tools have
//! (no private folder, short snippets); from anywhere else it is a desktop call as ctxd's socket serves them.

use serde_json::{Value, json};

use crate::out::{Done, Fail, R};
use crate::sock;

fn call(tool: &str, args: Value) -> R<Value> {
    if !bitstree::of_self().inside() {
        return sock::ctxd(tool, args);
    }
    let rpc = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}});
    let r = sock::ctxd("mcp", rpc)?;
    if let Some(e) = r.get("error") {
        return Err(Fail::failed(e["message"].as_str().unwrap_or("abgelehnt").to_string()));
    }
    let res = &r["result"];
    if res["isError"] == true {
        let text = res["content"][0]["text"].as_str().unwrap_or("abgelehnt");
        return Err(if text.starts_with("not_found") { Fail::new("not_found", text) } else { Fail::failed(text) });
    }
    Ok(res.get("structuredContent").cloned().unwrap_or(Value::Null))
}

fn opt_num(a: &[String], flag: &str) -> R<Option<u64>> {
    match a.iter().position(|x| x == flag) {
        Some(i) => a.get(i + 1).and_then(|n| n.parse().ok()).map(Some).ok_or_else(|| Fail::usage(format!("{flag} braucht eine Zahl"))),
        None => Ok(None),
    }
}

fn opt_str<'a>(a: &'a [String], flag: &str) -> Option<&'a str> {
    a.iter().position(|x| x == flag).and_then(|i| a.get(i + 1)).map(String::as_str)
}

/// The words that are not options (nor an option's value).
fn plain(a: &[String], with_value: &[&str]) -> String {
    let mut out = Vec::new();
    let mut skip = false;
    for x in a {
        if skip {
            skip = false;
        } else if with_value.contains(&x.as_str()) {
            skip = true;
        } else if !x.starts_with("--") {
            out.push(x.as_str());
        }
    }
    out.join(" ")
}

pub fn dateien(args: &[String]) -> R {
    match args.first().map(String::as_str) {
        Some("suchen") => {
            let rest = &args[1..];
            let query = plain(rest, &["--anzahl", "--seit"]);
            if query.is_empty() {
                return Err(Fail::usage("dateien suchen <wörter> [--anzahl n] [--seit JJJJ-MM-TT]"));
            }
            let mut a = json!({"query": query});
            if let Some(n) = opt_num(rest, "--anzahl")? {
                a["limit"] = json!(n);
            }
            if let Some(s) = opt_str(rest, "--seit") {
                a["since"] = json!(s);
            }
            let r = call("context_search", a)?;
            let hits = r.get("hits").or_else(|| r.get("results")).cloned().unwrap_or(Value::Null);
            let text = hits.as_array().map(|h| h.iter().map(|x| format!("{}  {}", x["path"].as_str().unwrap_or(""), x["title"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n")).unwrap_or_default();
            Ok(Done::new(text).with("result", r))
        }
        Some(v) => Err(Fail::usage(format!("„dateien {v}“ gibt es nicht; möglich: suchen."))),
        None => Err(Fail::usage("dateien suchen <wörter>")),
    }
}

pub fn arbeitsraum(args: &[String]) -> R {
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    let id = || rest.first().cloned().ok_or_else(|| Fail::usage("Welcher Arbeitsraum? (seine Nummer aus „arbeitsraum liste“)"));
    let (tool, a) = match args.first().map(String::as_str) {
        Some("liste") | None => {
            let q = plain(rest, &[]);
            ("workspace_list", if q.is_empty() { json!({}) } else { json!({"query": q}) })
        }
        Some("oeffnen") => match opt_str(rest, "--nummer") {
            Some(n) => ("workspace_open", json!({"workspace": n})),
            None => {
                let name = plain(rest, &[]);
                if name.is_empty() {
                    return Err(Fail::usage("arbeitsraum oeffnen <name> | --nummer <id>"));
                }
                ("workspace_open", json!({"name": name}))
            }
        },
        Some("hinzufuegen") => {
            let doc = rest.get(1).ok_or_else(|| Fail::usage("arbeitsraum hinzufuegen <id> <datei>"))?;
            let full = std::fs::canonicalize(doc).map(|p| p.display().to_string()).unwrap_or_else(|_| doc.clone());
            ("workspace_add", json!({"workspace": id()?, "paths": [full]}))
        }
        Some("umbenennen") => {
            let name = plain(&rest[rest.len().min(1)..], &[]);
            if name.is_empty() {
                return Err(Fail::usage("arbeitsraum umbenennen <id> <neuer name>"));
            }
            ("workspace_rename", json!({"workspace": id()?, "name": name}))
        }
        Some("anheften") => ("workspace_pin", json!({"workspace": id()?, "pinned": !rest.iter().any(|x| x == "--los")})),
        Some(v) => return Err(Fail::usage(format!("„arbeitsraum {v}“ gibt es nicht; möglich: liste, oeffnen, hinzufuegen, umbenennen, anheften."))),
    };
    let r = call(tool, a)?;
    let text = match r.get("workspaces").and_then(Value::as_array) {
        Some(ws) => ws.iter().map(|w| format!("{}  {}{}", w["id"].as_str().unwrap_or(""), w["name"].as_str().unwrap_or(""), if w["pinned"] == true { " (angeheftet)" } else { "" })).collect::<Vec<_>>().join("\n"),
        None => serde_json::to_string_pretty(&r).unwrap_or_default(),
    };
    Ok(Done::new(text).with("result", r))
}
