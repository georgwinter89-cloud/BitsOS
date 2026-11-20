//! Claude's tools (MC-6's client library, as bitscomp registers its own). D6: Claude may search and propose;
//! `program_install`, `programs_refresh` (class change) and `program_remove` (delete_overwrite) go through the
//! kernel's gate to the installer first (mcp-wire `Kind::Program`): only after Georg's yes at the machine (or
//! his time grant, Strg+Alt+G) does the call reach this service. So Claude can ask for the first refresh too,
//! and R9 holds: nothing is fetched from a mirror without that yes. Each call runs in a thread of its own (an
//! install takes minutes; `serve` would take one call at a time).

use crate::plans::Action;
use crate::{Ctx, ops, planning, progress, work};
use mcpreg::{Event, Replier, Reply, Safety, Service, Tool};
use settingsd::json::Json;
use std::sync::Arc;
use std::time::Duration;

pub const NAME: &str = "bitspkgd";
/// How long Claude's client waits for an install by default (its `timeout_ms`; the kernel reads this default).
const INSTALL_WAIT_MS: u64 = 1_800_000;

fn tools() -> Result<Vec<Tool>, mcpreg::Error> {
    let name_only = |what: &str| format!(r#"{{"type":"object","properties":{{"name":{{"type":"string","description":"{what}"}},"timeout_ms":{{"type":"integer","default":{INSTALL_WAIT_MS}}}}},"required":["name"]}}"#);
    Ok(vec![
        Tool::new(
            "programs_search",
            "Search the configured repositories (Arch, CachyOS) for programs: name, version, description, Rust programs first. Reads the loaded databases only; nothing is installed.",
            r#"{"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer","minimum":1,"default":20}},"required":["query"]}"#,
            Safety::Read,
        )?,
        Tool::new("programs_info", "What a program is: installed versions and the repository's description, depends, sizes.", r#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}"#, Safety::Read)?,
        Tool::new(
            "program_install",
            "Install a program from the repositories ([REPO/]NAME) as its own closure under /apps; the previous version stays for going back. Georg confirms at the machine (D6) before it runs. Answers with the install report.",
            &name_only("[REPO/]NAME of the program"),
            Safety::Change,
        )?,
        Tool::new("program_remove", "Remove an installed program with all its versions and its data folder. Georg confirms at the machine (D6) before it runs.", &name_only("the installed program"), Safety::DeleteOverwrite)?,
        Tool::new(
            "programs_refresh",
            "Load the package lists from the configured package sources (the page's \"Jetzt prüfen\"): needed once before programs_search and program_install can answer, and later to see newer versions. Installs nothing. Georg confirms at the machine (D6) before anything is fetched. Answers with one line per source: aktualisiert, unverändert, or abgelehnt with the reason.",
            &format!(r#"{{"type":"object","properties":{{"timeout_ms":{{"type":"integer","default":{}}}}}}}"#, ops::REFRESH_DEADLINE.as_millis()),
            Safety::Change,
        )?,
    ])
}

/// The socket's texts name the page's button; Claude's way to the first refresh is its own tool.
fn for_claude(msg: &str) -> String {
    if msg.contains("keine Quelle geladen") { format!("{msg}. Für Claude: das Werkzeug programs_refresh lädt die Paketlisten (nach Georgs Ja)") } else { msg.to_string() }
}

fn lines(j: &Json, key: &str) -> Vec<String> {
    match j.get(key) {
        Some(Json::Arr(a)) => a.iter().filter_map(|l| l.as_str().map(String::from)).collect(),
        _ => Vec::new(),
    }
}

fn search_text(j: &Json) -> String {
    let mut o = String::new();
    if let Some(Json::Arr(hits)) = j.get("hits") {
        for h in hits {
            let rust = if h.get("rust").and_then(Json::as_bool) == Some(true) { " [Rust]" } else { "" };
            let hidden = h.str("hidden_by").map(|r| format!(" (verdeckt durch {r})")).unwrap_or_default();
            o.push_str(&format!("{}/{} {}{rust}{hidden}\n", h.str("repo").unwrap_or(""), h.str("name").unwrap_or(""), h.str("version").unwrap_or("")));
            if let Some(d) = h.str("desc").filter(|d| !d.is_empty()) {
                o.push_str(&format!("    {d}\n"));
            }
        }
    }
    if o.is_empty() {
        o.push_str("keine Treffer\n");
    }
    o
}

/// install/remove: the plan, then the run with its phases as MCP progress.
fn change(cx: &Arc<Ctx>, a: Action, name: &str, id: u64, r: &Replier) -> Reply {
    let m = match planning::make(cx, a, name) {
        Ok(m) => m,
        Err(e) => return Reply::error(for_claude(&e)),
    };
    let shown = cx.plans().take_shown(a, name, std::time::Instant::now());
    let Some(p) = m.planned else {
        return Reply::error(format!("{}\n{}", m.reply.str("summary").unwrap_or(name), m.reply.str("text").unwrap_or("")));
    };
    // What Georg confirmed is what runs: the dialog's plan (bitsinstall asked with `shown`) must still hold. No
    // showing (a time grant, Strg+Alt+G, or a dialog that got no facts from here) leaves the plan made now.
    if let Some(was) = shown
        && !planning::holds(&was, Some(&p))
    {
        crate::event("pkg.daemon.do_refused", &[("action", a.name()), ("name", name), ("reason", "stale_plan")]);
        return Reply::error(format!("Plan veraltet: {name} bringt jetzt andere Teile, als der Dialog zeigte ({}); bitte neu fragen", m.reply.str("summary").unwrap_or(name)));
    }
    crate::event("pkg.daemon.do", &[("action", a.name()), ("name", name), ("peer", "mcp (bestätigt)")]);
    let mut b = progress::Bytes { done: 0, total: p.download };
    let what = format!("{} {name}", a.name());
    let res = work::run(cx, &what, ops::DO_DEADLINE, work::BUSY_WAIT, move |cx| planning::run(cx, &p), &mut |i| {
        let j = progress::line(&i, &mut b);
        if j.str("progress") != Some("report") && j.str("progress") != Some("event") {
            let _ = r.progress(id, &j.to_text());
        }
    });
    match res {
        Ok(j) if j.get("ok").and_then(Json::as_bool) == Some(true) => Reply::text(lines(&j, "report").join("\n")),
        Ok(j) => Reply::error(format!("{}\n{}", j.str("message").unwrap_or("abgelehnt"), lines(&j, "report").join("\n"))),
        Err(e) => Reply::error(e),
    }
}

/// The page's refresh for Claude: the socket's `refresh` (the same run, one at a time with every other changing
/// run), its steps as MCP progress, the report as the answer. An error only when no source at all could be loaded.
fn refresh(cx: &Arc<Ctx>, id: u64, r: &Replier) -> Reply {
    crate::event("pkg.daemon.do", &[("action", "refresh"), ("name", "-"), ("peer", "mcp (bestätigt)")]);
    let mut b = progress::Bytes { done: 0, total: 0 };
    let res = work::run(cx, "refresh", ops::REFRESH_DEADLINE, work::BUSY_WAIT, ops::refresh_op, &mut |i| {
        let j = progress::line(&i, &mut b);
        if j.str("progress") != Some("report") && j.str("progress") != Some("event") {
            let _ = r.progress(id, &j.to_text());
        }
    });
    match res {
        Ok(j) => refresh_reply(&j),
        Err(e) => Reply::error(e),
    }
}

/// `refresh`'s answer as Claude reads it.
pub fn refresh_text(j: &Json) -> Result<String, String> {
    let report = lines(j, "report").join("\n");
    if j.get("ok").and_then(Json::as_bool) != Some(true) {
        return Err(format!("{}\n{report}", j.str("message").unwrap_or("abgelehnt")).trim_end().to_string());
    }
    match (j.get("loaded").and_then(Json::as_i64), j.get("refused").and_then(Json::as_i64)) {
        (Some(0), Some(n)) if n > 0 => Err(format!("keine Paketquelle ließ sich laden ({n} abgelehnt)\n{report}")),
        (Some(0), _) => Err("keine Paketquelle eingerichtet: es gibt nichts zu laden".to_string()),
        _ => Ok(report),
    }
}

fn refresh_reply(j: &Json) -> Reply {
    match refresh_text(j) {
        Ok(t) => Reply::text(t),
        Err(e) => Reply::error(e),
    }
}

fn call(cx: &Arc<Ctx>, c: &mcpreg::Call, r: &Replier) -> Reply {
    let arg = |k: &str| c.arguments.get(k).and_then(|v| v.as_str()).map(String::from);
    match c.name.as_str() {
        "programs_search" => {
            let Some(q) = arg("query") else { return Reply::bad_argument("query") };
            let limit = c.arguments.get("limit").and_then(|v| v.as_u64()).filter(|n| *n > 0).unwrap_or(20) as usize;
            let j = ops::search_op(cx, &q, limit);
            match j.get("ok").and_then(Json::as_bool) {
                Some(true) => Reply::text(search_text(&j)),
                _ => Reply::error(for_claude(j.str("message").unwrap_or("Suche fehlgeschlagen"))),
            }
        }
        "programs_info" => {
            let Some(n) = arg("name") else { return Reply::bad_argument("name") };
            let j = ops::info_op(cx, &n);
            if j.get("ok").and_then(Json::as_bool) != Some(true) {
                return Reply::error(j.str("message").unwrap_or("unbekannt").to_string());
            }
            let mut o = String::new();
            if let Some(i) = j.get("installed") {
                o.push_str(&format!("installiert: {} (vorher {})\n", i.str("current").unwrap_or("-"), i.str("previous").unwrap_or("-")));
            }
            if let Some(rp) = j.get("repo") {
                o.push_str(&lines(rp, "lines").join("\n"));
            }
            Reply::text(o)
        }
        "program_install" => match arg("name") {
            Some(n) => change(cx, Action::Install, &n, c.id, r),
            None => Reply::bad_argument("name"),
        },
        "program_remove" => match arg("name") {
            Some(n) => change(cx, Action::Remove, &n, c.id, r),
            None => Reply::bad_argument("name"),
        },
        "programs_refresh" => refresh(cx, c.id, r),
        other => Reply::error(format!("unknown tool {other}")),
    }
}

fn session(cx: &Arc<Ctx>, mut svc: Service) -> Result<(), mcpreg::Error> {
    let names = svc.register(&tools()?)?;
    crate::event("pkg.mcp.registered", &[("tools", &names.join(","))]);
    while let Some(ev) = svc.next()? {
        if let Event::Call(c) = ev {
            let (cx, r) = (cx.clone(), svc.replier());
            let spawned = std::thread::Builder::new().name("pkg-mcp-call".into()).spawn(move || {
                let reply = call(&cx, &c, &r);
                let _ = r.reply(c.id, reply);
            });
            if let Err(e) = spawned {
                crate::event("pkg.mcp.thread", &[("reason", &e.to_string())]);
            }
        }
    }
    Ok(())
}

/// Registers the tools with the kernel and keeps them registered (a new connection after the kernel's registry
/// restarted), with a back-off of up to a minute.
pub fn spawn(cx: Arc<Ctx>) {
    let r = std::thread::Builder::new().name("pkg-mcp".into()).spawn(move || {
        let mut wait = Duration::from_secs(1);
        loop {
            match Service::connect(NAME) {
                Ok(svc) => {
                    wait = Duration::from_secs(1);
                    match session(&cx, svc) {
                        Ok(()) => crate::event("pkg.mcp.closed", &[]),
                        Err(e) => crate::event("pkg.mcp.failed", &[("reason", &e.to_string())]),
                    }
                }
                Err(e) => {
                    if wait >= Duration::from_secs(60) {
                        crate::event("pkg.mcp.unreachable", &[("reason", &e.to_string())]);
                    }
                }
            }
            std::thread::sleep(wait);
            wait = (wait * 2).min(Duration::from_secs(60));
        }
    });
    if let Err(e) = r {
        crate::event("pkg.mcp.thread", &[("reason", &e.to_string())]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_refresh_tool_is_a_changing_tool_without_arguments() {
        let t = tools().unwrap();
        let names: Vec<&str> = t.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["programs_search", "programs_info", "program_install", "program_remove", "programs_refresh"]);
        let r = &t[4];
        // Class change: the kernel's gate hands it to the installer first (R9: no fetch without Georg's yes).
        assert!(matches!(r.safety, Safety::Change), "programs_refresh must be class change");
        let schema = format!("{:?}", r.input_schema);
        assert!(schema.contains("timeout_ms") && schema.contains("1800000") && !schema.contains("required"), "{schema}");
        assert!(matches!(t[0].safety, Safety::Read) && matches!(t[1].safety, Safety::Read));
    }

    #[test]
    fn the_refresh_answer_is_the_report_and_an_error_only_when_nothing_loaded() {
        let report = |l: &[&str]| Json::Arr(l.iter().map(|x| Json::Str(x.to_string())).collect());
        let ok = Json::obj().with("ok", true).with("loaded", 2usize).with("refused", 1usize).with("report", report(&["core: aktualisiert, 300 Pakete, von http://m/core", "extra: unverändert, 9 Pakete, von http://m/extra", "x: abgelehnt: HTTP 404"]));
        assert_eq!(refresh_text(&ok).unwrap().lines().count(), 3);
        assert!(refresh_text(&ok).unwrap().contains("x: abgelehnt: HTTP 404"));
        let none = Json::obj().with("ok", true).with("loaded", 0usize).with("refused", 2usize).with("report", report(&["a: abgelehnt: zu alt", "b: abgelehnt: HTTP 404"]));
        let e = refresh_text(&none).unwrap_err();
        assert!(e.starts_with("keine Paketquelle ließ sich laden (2 abgelehnt)") && e.contains("b: abgelehnt: HTTP 404"), "{e}");
        let empty = Json::obj().with("ok", true).with("loaded", 0usize).with("refused", 0usize).with("report", report(&[]));
        assert!(refresh_text(&empty).unwrap_err().contains("keine Paketquelle eingerichtet"));
        assert_eq!(refresh_text(&ops::err("refused", "Schlüsselbund fehlt")).unwrap_err(), "Schlüsselbund fehlt");
        assert!(for_claude("keine Quelle geladen: zuerst Aktualisieren (refresh)").contains("programs_refresh"));
        assert_eq!(for_claude("jq: unbekannt"), "jq: unbekannt");
    }
}
