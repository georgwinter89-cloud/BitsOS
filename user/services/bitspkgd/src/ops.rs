//! The socket's ops (plan 1.10), one JSON request per line; every answer ends with one line that has `ok`
//! (settingsd's style: `{"ok":false,"error":<code>,"message":<German>}`); `do` and `refresh` send `progress`
//! lines before it.
//!
//! | op | fields | answer | who |
//! |---|---|---|---|
//! | `search` | `query`, `limit` (default 50) | `hits: [{repo,name,version,desc,rust,csize,isize,hidden_by}]` | all |
//! | `info` | `name` | `installed` (current, previous, versions) and `repo` (the database's lines) | all |
//! | `plan` | `action` install/remove/update, `name`, `keep_data` (remove only) | the closure, sizes, refusals, Zusätze, permissions, `summary`, `plan_id` | all |
//! | `do` | `plan_id` | `progress` lines, then `done` with the report, or the error | change |
//! | `switch` | `name`, `version` | `current`, `previous` | change |
//! | `rollback` | `name` | `from`, `to`, the closure difference | change |
//! | `space` | `name` (optional) | per program: package, own, shared, data | all |
//! | `list` | – | `programs: [{name,current,previous,versions,id}]` | all |
//! | `sources` | – | per source: servers, packages, age, signatures | all |
//! | `refresh` | – | `progress` lines, then the report (`loaded`, `refused`: how many sources) | change |
//! | `status` | – | what runs, the last update check, plans kept, `features` beyond the first protocol | all |
//! | `update_*` | see `update` | the update page's ops (UPDATE) | status: all; the rest: change |
//!
//! Errors: `bad_request`, `read_only` (the peer may only read: `peer`), `unknown_plan`, `stale_plan`, `refused`
//! (the library's German reason), `busy`, `timeout`, `failed`.

use crate::Ctx;
use crate::peer::Right;
use crate::plans::Action;
use crate::{planning, progress, work};
use bitspkg::repo::{refresh, search, sources, update};
use settingsd::json::{self, Json};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const REQUEST_MAX: usize = 64 * 1024;
/// `status.features`: the ops and fields beyond the first protocol (PK-6f) that this bitspkgd has.
pub const FEATURES: &[&str] = &["keep_data"];
/// B4: a reader's deadline (local files only; settingsd answers within its own 10 s).
pub const READ_DEADLINE: Duration = Duration::from_secs(30);
/// A `do`: downloads have their own deadlines (fetch: 30 s idle, 600 s per file); this bounds the client's wait.
pub const DO_DEADLINE: Duration = Duration::from_secs(3600);
pub const REFRESH_DEADLINE: Duration = Duration::from_secs(1800);
pub const SHORT_DEADLINE: Duration = Duration::from_secs(120);

pub fn err(code: &str, msg: impl Into<String>) -> Json {
    Json::obj().with("ok", false).with("error", code).with("message", msg.into())
}

fn s(v: &str) -> Json {
    Json::Str(v.to_string())
}

fn need<'a>(j: &'a Json, k: &str) -> Result<&'a str, Json> {
    j.str(k).filter(|v| !v.is_empty()).ok_or_else(|| err("bad_request", format!("{k} fehlt")))
}

/// A reader in a thread of its own, answered within `READ_DEADLINE` (it never waits for a changing run). Readers
/// that outlive their deadline keep running to their end; how many run at once follows the machine
/// (`Ctx::reader_limit`), beyond that a reader is told `busy` instead of adding threads (B4, a flood of local
/// clients cannot grow the service without bound).
fn read(cx: &Arc<Ctx>, what: &str, f: impl FnOnce(&Ctx) -> Json + Send + 'static) -> Json {
    let Some(slot) = cx.reader() else {
        crate::event("pkg.op.busy", &[("op", what), ("readers", &cx.reader_limit().to_string())]);
        return err("busy", format!("{what}: gerade laufen {} Leseanfragen; bitte gleich erneut versuchen", cx.reader_limit()));
    };
    let (tx, rx) = mpsc::channel();
    let c = cx.clone();
    if let Err(e) = std::thread::Builder::new().name("pkg-read".into()).spawn(move || {
        let _slot = slot;
        let _ = tx.send(f(&c));
    }) {
        return err("failed", format!("kein Thread: {e}"));
    }
    match rx.recv_timeout(READ_DEADLINE) {
        Ok(j) => j,
        Err(_) => {
            crate::event("pkg.op.timeout", &[("op", what), ("after_s", &READ_DEADLINE.as_secs().to_string())]);
            err("timeout", format!("{what}: keine Antwort nach {} s", READ_DEADLINE.as_secs()))
        }
    }
}

/// A changing run with its progress lines through `out`.
fn change(cx: &Arc<Ctx>, what: &str, deadline: Duration, total: u64, job: impl FnOnce(&Ctx) -> Json + Send + 'static, out: &mut dyn FnMut(&Json) -> bool) -> Json {
    let mut b = progress::Bytes { done: 0, total };
    let mut gone = false;
    let r = work::run(cx, what, deadline, work::BUSY_WAIT, job, &mut |i| {
        if !gone && !out(&progress::line(&i, &mut b)) {
            // The client left: the run goes on, its report stays in the journal and in the library's history.
            gone = true;
        }
    });
    match r {
        Ok(j) => j,
        Err(e) if e.contains("arbeitet gerade") => err("busy", e),
        Err(e) => err("timeout", e),
    }
}

/// One request. `peer` judges the asking process now (SO_PEERCRED, its program, the kernel's holder file): it is
/// asked again for every changing op and every plan, so a compositor that lost fb0's grant since it connected
/// changes nothing more.
pub fn handle(cx: &Arc<Ctx>, peer: &dyn Fn() -> Right, line: &str, out: &mut dyn FnMut(&Json) -> bool) {
    let j = match json::parse(line) {
        Ok(j @ Json::Obj(_)) => j,
        Ok(_) => {
            out(&err("bad_request", "eine Anfrage ist ein JSON-Objekt"));
            return;
        }
        Err(e) => {
            out(&err("bad_request", format!("kein JSON: {e}")));
            return;
        }
    };
    let op = j.str("op").unwrap_or("").to_string();
    let changing = matches!(op.as_str(), "do" | "switch" | "rollback" | "refresh") || crate::update::changing(&op);
    let right = if changing || op == "plan" { peer() } else { Right::Read(String::from("Leseanfrage")) };
    if changing && !right.may_change() {
        crate::event("pkg.daemon.read_only", &[("op", &op), ("peer", right.who())]);
        out(&err("read_only", format!("{op}: {}", right.who())).with("peer", right.who()));
        return;
    }
    let reply = match dispatch(cx, &right, &op, &j, out) {
        Ok(r) | Err(r) => r,
    };
    out(&reply);
}

fn dispatch(cx: &Arc<Ctx>, right: &Right, op: &str, j: &Json, out: &mut dyn FnMut(&Json) -> bool) -> Result<Json, Json> {
    Ok(match op {
        "search" => {
            let q = need(j, "query")?.to_string();
            let limit = match j.get("limit") {
                None | Some(Json::Null) => 50,
                Some(v) => v.as_i64().filter(|n| *n > 0).ok_or_else(|| err("bad_request", "limit: eine Zahl über 0"))? as usize,
            };
            read(cx, "search", move |cx| search_op(cx, &q, limit))
        }
        "info" => {
            let n = need(j, "name")?.to_string();
            read(cx, "info", move |cx| info_op(cx, &n))
        }
        "plan" => {
            let a = Action::parse(need(j, "action")?).ok_or_else(|| err("bad_request", "action: install, remove oder update"))?;
            let n = need(j, "name")?.to_string();
            // `shown`: the installer's dialog asks (bitsinstall, root); what it shows Georg is what Claude's
            // confirmed call must still find (D6).
            let shown = j.get("shown").and_then(Json::as_bool) == Some(true);
            // CR-PK-7a-1: "Meine Daten behalten"; only a removal has a data folder to keep.
            let keep_data = match j.get("keep_data") {
                None | Some(Json::Null) => false,
                Some(v) => v.as_bool().ok_or_else(|| err("bad_request", "keep_data: true oder false"))?,
            };
            if keep_data && a != Action::Remove {
                return Err(err("bad_request", "keep_data gilt nur beim Entfernen"));
            }
            let keep = right.may_change();
            read(cx, "plan", move |cx| match planning::make_with(cx, a, &n, keep_data) {
                Ok(m) => match m.planned {
                    Some(p) if keep => {
                        if shown {
                            cx.plans().show(a, &n, p.parts.clone(), Instant::now());
                        }
                        let id = cx.plans().add(p);
                        m.reply.with("plan_id", id)
                    }
                    Some(_) => m.reply.with("plan_id", Json::Null).with("note", "nur lesen: dieser Plan bekommt keine Nummer (ausführen dürfen root und der Compositor)"),
                    None => m.reply,
                },
                Err(e) => err("refused", e),
            })
        }
        "do" => {
            let id = need(j, "plan_id")?.to_string();
            let unknown = |id: &str| err("unknown_plan", format!("Plan {id} unbekannt, schon ausgeführt oder älter als 30 min: bitte neu planen"));
            let p = cx.plans().peek(&id, Instant::now()).ok_or_else(|| unknown(&id))?;
            crate::event("pkg.daemon.do", &[("action", p.action.name()), ("name", &p.name), ("peer", right.who())]);
            let what = format!("{} {}", p.action.name(), p.name);
            // Taken only once the run holds the service: a busy refusal or a second `do` waiting meanwhile leaves
            // or finds it as it should (a plan runs once).
            change(cx, &what, DO_DEADLINE, p.download, move |cx| match cx.plans().take(&id, Instant::now()) {
                Some(p) => planning::run(cx, &p),
                None => unknown(&id),
            }, out)
        }
        "switch" => {
            let (n, v) = (need(j, "name")?.to_string(), need(j, "version")?.to_string());
            change(cx, &format!("switch {n}"), SHORT_DEADLINE, 0, move |cx| match bitspkg::install::switch(&cx.store(), &n, &v) {
                Ok((old, failed)) => Json::obj().with("ok", true).with("current", v.as_str()).with("previous", old).with("hooks_failed", failed),
                Err(e) => err("refused", e),
            }, out)
        }
        "rollback" => {
            let n = need(j, "name")?.to_string();
            change(cx, &format!("rollback {n}"), SHORT_DEADLINE, 0, move |cx| {
                let st = cx.store();
                match bitspkg::install::rollback(&st, &n) {
                    Ok((from, to, failed)) => {
                        let lines = update::rollback_lines(&st, &n, &from, &to);
                        Json::obj().with("ok", true).with("from", from).with("to", to).with("hooks_failed", failed).with("report", Json::Arr(lines.iter().map(|l| s(l)).collect()))
                    }
                    Err(e) => err("refused", e),
                }
            }, out)
        }
        "refresh" => change(cx, "refresh", REFRESH_DEADLINE, 0, refresh_op, out),
        "space" => {
            let n = j.str("name").map(String::from);
            read(cx, "space", move |cx| space_op(cx, n.as_deref()))
        }
        "list" => read(cx, "list", list_op),
        "sources" => read(cx, "sources", sources_op),
        "status" => read(cx, "status", status_op),
        // UPDATE: the update page (crate::update); network and kernel waits have their own bounds (fetch's).
        op if crate::update::OPS.contains(&op) => crate::update::handle(op, j, out),
        "" => return Err(err("bad_request", "op fehlt")),
        other => return Err(err("bad_request", format!("unbekannte op {other:?}"))),
    })
}

pub fn search_op(cx: &Ctx, q: &str, limit: usize) -> Json {
    let st = cx.store();
    let cfg = match cx.config(&st) {
        Ok(c) => c,
        Err(e) => return err("failed", e),
    };
    let (idx, missing) = search::indexes(&st.root, &cfg);
    if idx.is_empty() {
        return err("refused", "keine Quelle geladen: zuerst Aktualisieren (refresh)");
    }
    let all = search::search(&idx, q);
    let hits: Vec<Json> = all
        .iter()
        .take(limit)
        .map(|h| Json::obj().with("repo", h.repo.as_str()).with("name", h.pkg.name.as_str()).with("version", h.pkg.version.as_str()).with("desc", h.pkg.desc.as_str()).with("rust", h.pkg.rust()).with("csize", h.pkg.csize).with("isize", h.pkg.isize).with("hidden_by", h.shadowed_by.clone()))
        .collect();
    Json::obj().with("ok", true).with("hits", Json::Arr(hits)).with("more", all.len().saturating_sub(limit)).with("notes", Json::Arr(missing.iter().map(|m| s(m)).collect()))
}

pub fn info_op(cx: &Ctx, name: &str) -> Json {
    let st = cx.store();
    let mut r = Json::obj().with("ok", true).with("name", name);
    let prog = name.rsplit('/').next().unwrap_or(name);
    if bitspkg::names::check_name(prog).is_ok() && st.versions(prog).is_ok_and(|v| !v.is_empty()) {
        r = r.with(
            "installed",
            Json::obj().with("current", st.current(prog)).with("previous", st.previous(prog)).with("versions", Json::Arr(st.versions(prog).unwrap_or_default().iter().map(|v| s(v)).collect())),
        );
    }
    if let Ok(cfg) = cx.config(&st) {
        let (idx, _) = search::indexes(&st.root, &cfg);
        if let Some((repo, p)) = search::find(&idx, name) {
            r = r.with("repo", Json::obj().with("repo", repo).with("name", p.name.as_str()).with("version", p.version.as_str()).with("desc", p.desc.as_str()).with("lines", Json::Arr(search::info_lines(repo, p).iter().map(|l| s(l)).collect())));
        }
    }
    if r.get("installed").is_none() && r.get("repo").is_none() {
        return err("refused", format!("{name}: weder installiert noch in einer geladenen Quelle"));
    }
    r
}

/// One refresh of every source (the socket's `refresh`, the page's "Jetzt prüfen", Claude's `programs_refresh`).
pub fn refresh_op(cx: &Ctx) -> Json {
    let st = cx.store();
    let cfg = match cx.config(&st) {
        Ok(c) => c,
        Err(e) => return err("failed", e),
    };
    match bitspkg::repo::cli::refresh_all(&st, &cfg, &bitspkg::repo::download::Fetcher::standard()) {
        Ok(rs) => {
            let bad = rs.iter().filter(|r| matches!(r.outcome, refresh::Outcome::Refused(_))).count();
            let text = refresh::render(&rs, crate::now());
            Json::obj().with("ok", true).with("done", true).with("loaded", rs.len() - bad).with("refused", bad).with("report", Json::Arr(text.lines().map(s).collect()))
        }
        Err(e) => err("refused", e),
    }
}

fn space_op(cx: &Ctx, only: Option<&str>) -> Json {
    let st = cx.store();
    match bitspkg::space::report(&st, only) {
        Ok((progs, store)) => {
            let ps: Vec<Json> = progs
                .iter()
                .map(|p| Json::obj().with("name", p.name.as_str()).with("package", p.package.bytes).with("own", p.own.bytes).with("shared", p.shared.bytes).with("data", p.data.as_ref().map(|d| d.size.bytes)).with("versions", p.versions.len()))
                .collect();
            Json::obj().with("ok", true).with("programs", Json::Arr(ps)).with("store", store.size.bytes).with("unused", store.unused.bytes)
        }
        Err(e) => err("refused", e),
    }
}

fn list_op(cx: &Ctx) -> Json {
    match bitspkg::verify::list(&cx.store()) {
        Ok(l) => Json::obj().with(
            "programs",
            Json::Arr(l.iter().map(|i| Json::obj().with("name", i.name.as_str()).with("current", i.current.clone()).with("previous", i.previous.clone()).with("versions", Json::Arr(i.versions.iter().map(|v| s(v)).collect())).with("id", i.program_id)).collect()),
        ),
        Err(e) => return err("failed", e),
    }
    .with("ok", true)
}

fn sources_op(cx: &Ctx) -> Json {
    let st = cx.store();
    let cfg = match cx.config(&st) {
        Ok(c) => c,
        Err(e) => return err("failed", e),
    };
    let sync = refresh::sync_dir(&st.root);
    let list: Vec<Json> = cfg
        .sources
        .iter()
        .map(|src| {
            let (pkgs, age) = match refresh::load(&sync, &src.name) {
                Ok(i) => (Json::from(i.pkgs.len()), Json::from(refresh::age_days(i.newest, crate::now()))),
                Err(_) => (Json::Null, Json::Null),
            };
            Json::obj()
                .with("name", src.name.as_str())
                .with("servers", Json::Arr(src.servers.iter().map(|x| s(x)).collect()))
                .with("sig_db", src.sig_db.name())
                .with("sig_pkg", src.sig_pkg.name())
                .with("keyring", src.keyring.as_str())
                .with("packages", pkgs)
                .with("age_days", age)
                .with("cpu_ok", sources::cpu_can(sources::Level::of_repo(&src.name)))
        })
        .collect();
    Json::obj().with("ok", true).with("origin", cfg.origin.as_str()).with("cpu_level", cfg.level.name()).with("sources", Json::Arr(list))
}

fn status_op(cx: &Ctx) -> Json {
    let last = update::last_check(&cx.root);
    Json::obj()
        .with("ok", true)
        .with("busy", cx.work.busy())
        .with("plans", cx.plans().len())
        .with("last_check", last.map(|l| l.0))
        .with("updates_found", last.map(|l| l.1))
        .with("up_s", cx.started.elapsed().as_secs())
        // CR-PK-7a-8: what the page may offer; `with` and `clean` join once they exist (CR-PK-7a-2, -4).
        .with("features", Json::Arr(FEATURES.iter().map(|f| s(f)).collect()))
}
