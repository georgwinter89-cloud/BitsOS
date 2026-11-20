//! `plan` and what `do` runs (plan 1.10). A plan is what pacman shows before its question (src/pacman/sync.c:704
//! sync_trans: the targets, sizes, download; lib/libalpm/sync.c:367 _alpm_sync_prepare for the closure,
//! remove.c:203 _alpm_remove_prepare for a removal, diskspace.c:433 for the space), here from bitspkg's own
//! resolver (PK-6c) over the loaded databases: nothing is downloaded. `run` does it with bitspkg's library and
//! checks first that the plan still holds (another refresh may have changed the closure meanwhile).

use crate::Ctx;
use crate::plans::{Action, Planned};
use bitspkg::names::{check_name, program_of};
use bitspkg::repo::download::Fetcher;
use bitspkg::repo::resolve::{Closure, Universe, policy::Prefs};
use bitspkg::repo::update::Part;
use bitspkg::repo::{apply, search, sources, update};
use bitspkg::space::human;
use bitspkg::store::Store;
use settingsd::json::Json;
use std::time::Instant;

fn s(v: &str) -> Json {
    Json::Str(v.to_string())
}

/// `pkg.optional.ask` (default yes): whether a plan lists the Zusätze.
fn optional_ask(st: &Store) -> bool {
    let Some(sock) = st.hooks.settings_socket() else { return true };
    let req = Json::obj().with("op", "get").with("key", sources::OPTIONAL_KEY);
    match (bitspkg::settings::Settings { socket: sock.to_path_buf() }).request(&req) {
        Ok(Ok(j)) => j.get("value").and_then(Json::as_bool).unwrap_or(true),
        _ => true,
    }
}

/// The BitsOS program `name` stands for: an installed folder, else the program a pacman name maps to.
fn program(st: &Store, name: &str) -> Result<String, String> {
    let guess = name.rsplit('/').next().unwrap_or(name);
    if check_name(guess).is_ok() && st.versions(guess).is_ok_and(|v| !v.is_empty()) {
        return Ok(guess.to_string());
    }
    program_of(guess)
}

/// The pacman name of an installed program's top package and its closure (name, version, sha256), as update.rs
/// reads them.
fn installed_closure(st: &Store, prog: &str) -> Result<(String, Vec<Part>), String> {
    let v = st.current(prog).ok_or_else(|| format!("{prog} ist nicht installiert"))?;
    let m = st.manifest(prog, &v)?;
    if m.origins.is_empty() {
        return Err(format!("{prog} kommt nicht aus den Quellen (ein BitsOS-Paket)"));
    }
    let top = m.origins.iter().find(|o| program_of(&o.name).as_deref() == Ok(prog)).unwrap_or(&m.origins[0]).name.clone();
    Ok((top, update::parts_of(&m.origins)))
}

/// Whether a plan made again (`now`) still is the one shown (`was`): the same parts, in the same versions from
/// the same sources. `do` and Claude's confirmed tools refuse a plan that no longer holds ("Plan veraltet").
pub fn holds(was: &[String], now: Option<&Planned>) -> bool {
    now.is_some_and(|n| n.parts == was)
}

pub struct Made {
    pub reply: Json,
    /// Some when the plan can run (`do`).
    pub planned: Option<Planned>,
}

/// An install or update plan: the closure over the loaded databases.
fn closure_plan(cx: &Ctx, st: &Store, action: Action, name: &str) -> Result<Made, String> {
    let cfg = cx.config(st)?;
    let (idx, missing) = search::indexes(&st.root, &cfg);
    if idx.is_empty() {
        return Err("keine Quelle geladen: zuerst Aktualisieren (refresh)".into());
    }
    let prog_guess = program(st, name)?;
    let (target, previous, before) = match action {
        Action::Update => {
            let (top, parts) = installed_closure(st, &prog_guess)?;
            // sync.c:124 check_replacers, as `bitspkg update` says it: a renamed or replaced program is named, never
            // swapped for another name by itself (Georg installs the new one, then removes the old).
            if search::find(&idx, &top).is_none() {
                let tv = parts.iter().find(|p| p.0 == top).map(|p| p.1.as_str()).unwrap_or("");
                let by = update::replacer(&idx, &top, tv);
                let summary = match &by {
                    Some(r) => format!("{prog_guess}: {top} ist in keiner Quelle mehr; ersetzt durch {r} (installieren: {r}, dann {prog_guess} entfernen); bleibt"),
                    None => format!("{prog_guess}: {top} ist in keiner Quelle mehr; bleibt, wie es ist"),
                };
                let reply = Json::obj().with("ok", true).with("action", action.name()).with("name", name).with("installable", false).with("replaced_by", by).with("summary", summary);
                return Ok(Made { reply, planned: None });
            }
            (top, parts.iter().map(|p| p.0.clone()).collect::<Vec<_>>(), parts)
        }
        _ => match installed_closure(st, &prog_guess) {
            Ok((_, parts)) => (name.to_string(), parts.iter().map(|p| p.0.clone()).collect(), parts),
            Err(_) => (name.to_string(), Vec::new(), Vec::new()),
        },
    };
    let u = Universe::new(&idx, cfg.level);
    let plan = Closure::resolve(&target, &u, &Prefs::builtin(), &previous);
    let refused: Vec<Json> = plan.refused.iter().map(|(n, r)| Json::obj().with("name", n.as_str()).with("reason", r.as_str())).collect();
    let mut reply = Json::obj().with("ok", true).with("action", action.name()).with("name", name).with("text", plan.render());
    if !missing.is_empty() {
        reply = reply.with("notes", Json::Arr(missing.iter().map(|m| s(m)).collect()));
    }
    if !plan.ok() {
        let why: Vec<String> = plan.refused.iter().map(|(n, r)| format!("{n}: {r}")).collect();
        let summary = format!("{name}: nicht installierbar ({})", if why.is_empty() { "keine Pakete".to_string() } else { why.join("; ") });
        return Ok(Made { reply: reply.with("installable", false).with("refused", Json::Arr(refused)).with("summary", summary), planned: None });
    }
    let top = plan.packages.iter().find(|p| p.why == "Programm").ok_or("kein Programm im Plan")?;
    let prog = program_of(&top.name)?;
    let parts: Vec<String> = plan.packages.iter().map(|p| format!("{}/{} {}", p.repo, p.name, p.version)).collect();
    let pkgs: Vec<Json> = plan
        .packages
        .iter()
        .map(|p| Json::obj().with("repo", p.repo.as_str()).with("name", p.name.as_str()).with("version", p.version.as_str()).with("csize", p.csize).with("isize", p.isize).with("why", p.why.as_str()).with("rust", p.rust))
        .collect();
    // The same predicate as `bitspkg update` (update.rs diff, pacman's vercmp, epoch first): an update is
    // something newer, added or gone with nothing older; an older part in the sources blocks it (sync.c:85
    // check_literal: "local is newer"), never a downgrade.
    let mut changed = Vec::new();
    let mut blocked = Vec::new();
    let mut is_update = false;
    if action == Action::Update {
        let now_parts: Vec<Part> = plan
            .packages
            .iter()
            .map(|p| {
                let sha = idx.iter().find(|(r, _)| *r == p.repo).and_then(|(_, i)| i.pkgs.get(&p.name)).map(|k| k.sha256.clone()).unwrap_or_default();
                (p.name.clone(), p.version.clone(), sha)
            })
            .collect();
        let d = update::diff(&before, &now_parts);
        is_update = d.is_update();
        for (n, a, b) in &d.newer {
            changed.push(Json::obj().with("name", n.as_str()).with("from", a.as_str()).with("to", b.as_str()));
        }
        for (n, v) in &d.added {
            changed.push(Json::obj().with("name", n.as_str()).with("from", Json::Null).with("to", v.as_str()));
        }
        for (n, v) in &d.removed {
            changed.push(Json::obj().with("name", n.as_str()).with("from", v.as_str()).with("to", Json::Null));
        }
        for (n, a, b) in &d.older {
            blocked.push(Json::obj().with("name", n.as_str()).with("installed", a.as_str()).with("sources", b.as_str()).with("reason", "in den Quellen älter als installiert (pacman: local is newer): kein Downgrade, wird übersprungen"));
        }
    }
    let optional: Vec<Json> = if optional_ask(st) {
        plan.optional.iter().filter(|o| !o.present).map(|o| Json::obj().with("dep", o.dep.as_str()).with("reason", o.reason.as_str())).collect()
    } else {
        Vec::new()
    };
    // Plan 1.6: the user folders always, devices and the web by the program's categories (known after the
    // download, from its .desktop files): both are asked (PM-3) and named in the install report.
    let mut perms: Vec<Json> = bitspkg::repo::tree::wanted(&[]).iter().map(|p| s(&p.describe())).collect();
    perms.push(s("Ton, Grafik und Netz (Port 80/443) dazu, falls es ein Netz-, Browser-, Medien- oder Spielprogramm ist (steht nach dem Herunterladen fest)"));
    let on = st.apps.ancestors().find(|a| a.is_dir()).unwrap_or(&st.root).to_path_buf();
    // D4 as apply.rs's "space (D4)" step judges it, before anything is fetched (diskspace.c:433's place in pacman):
    // a plan that would not fit is no plan.
    let mut space_refused = None;
    let space = match apply::free_space(&on) {
        Ok((total, avail)) => {
            let limit = cfg.space_max_gb.map(|g| g.saturating_mul(apply::GB)).unwrap_or(total / 2);
            let how = if cfg.space_max_gb.is_some() { "pkg.space.max_gb" } else { "die Hälfte von /data" };
            let used = apply::store_bytes(st);
            let verdict = apply::space_fits(plan.installed, plan.download, avail, used, limit, how);
            let j = Json::obj().with("free", avail).with("total", total).with("limit", limit).with("used", used).with("fits", verdict.is_ok());
            space_refused = verdict.err();
            j
        }
        Err(e) => Json::obj().with("error", e),
    };
    let choices: Vec<Json> = plan
        .choices
        .iter()
        .map(|c| Json::obj().with("dep", c.dep.as_str()).with("needed_by", c.needed_by.as_str()).with("chosen", format!("{}/{}", c.repo, c.chosen)).with("others", Json::Arr(c.others.iter().map(|o| s(o)).collect())))
        .collect();
    let summary = match action {
        _ if space_refused.is_some() => format!("{} {}: nicht installierbar ({})", top.name, top.version, space_refused.as_deref().unwrap_or("")),
        Action::Update if !blocked.is_empty() && !changed.is_empty() => format!("{prog} {}: Aktualisierung blockiert ({} Teile in den Quellen älter als installiert); bleibt", top.version, blocked.len()),
        Action::Update if !is_update => format!("{prog} {}: aktuell, nichts zu tun", top.version),
        Action::Update => format!("{prog} {}, {} Teile geändert, Download {}", top.version, changed.len(), human(plan.download)),
        _ => format!("{} {}, {} Teile, {} (Download {})", top.name, top.version, plan.packages.len(), human(plan.installed), human(plan.download)),
    };
    let mut refused = refused;
    if let Some(r) = &space_refused {
        refused.push(Json::obj().with("name", "Platz").with("reason", r.as_str()));
    }
    let runnable = space_refused.is_none() && (action != Action::Update || is_update);
    reply = reply
        .with("installable", space_refused.is_none())
        .with("choices", Json::Arr(choices))
        .with("program", prog.as_str())
        .with("version", top.version.as_str())
        .with("packages", Json::Arr(pkgs))
        .with("download", plan.download)
        .with("installed", plan.installed)
        .with("optional", Json::Arr(optional))
        .with("refused", Json::Arr(refused))
        .with("permissions", Json::Arr(perms))
        .with("space", space)
        .with("summary", summary);
    if action == Action::Update {
        reply = reply.with("changed", Json::Arr(changed)).with("blocked", Json::Arr(blocked));
    }
    let planned = runnable.then(|| Planned { action, name: name.to_string(), program: prog, parts, download: plan.download, keep_data: false, made: Instant::now() });
    Ok(Made { reply, planned })
}

fn remove_plan(st: &Store, name: &str, keep_data: bool) -> Result<Made, String> {
    let prog = program(st, name)?;
    let versions = st.versions(&prog).unwrap_or_default();
    if versions.is_empty() {
        let reply = Json::obj().with("ok", true).with("action", "remove").with("name", name).with("installable", false).with("summary", format!("{prog}: nicht installiert"));
        return Ok(Made { reply, planned: None });
    }
    let (progs, _) = bitspkg::space::report(st, Some(&prog))?;
    let p = progs.iter().find(|p| p.name == prog);
    let own = p.map(|p| p.own.bytes).unwrap_or(0);
    let data = p.and_then(|p| p.data.as_ref()).map(|d| d.size.bytes).unwrap_or(0);
    let current = st.current(&prog).unwrap_or_else(|| "-".into());
    // remove(.., keep_data = false) takes the data folder with it (pacman -R has no such folder; its .pacsave
    // files are the nearest), `keep_data` leaves it (CR-PK-7a-1, "Meine Daten behalten"): the plan says which.
    // D8: every program has its own parts, so no other program needs this one's (pacman -R's "required by",
    // remove.c:203, cannot happen); Georg's page names that.
    let has_data = st.data_dir(&prog).exists();
    let data_removed = has_data && !keep_data;
    let data_text = match (has_data, keep_data) {
        (false, _) => "kein Datenordner".to_string(),
        (true, true) => format!("Datenordner ({}) bleibt", human(data)),
        (true, false) => format!("Datenordner ({}) wird gelöscht", human(data)),
    };
    let freed = if data_removed { own.saturating_add(data) } else { own };
    let summary = format!("{prog} {current}, {} Fassungen, {} frei; {data_text}; andere Programme bleiben unberührt", versions.len(), human(freed));
    let reply = Json::obj()
        .with("ok", true)
        .with("action", "remove")
        .with("name", name)
        .with("installable", true)
        .with("program", prog.as_str())
        .with("version", current.as_str())
        .with("versions", Json::Arr(versions.iter().map(|v| s(v)).collect()))
        .with("frees", own)
        .with("data", data)
        .with("data_removed", data_removed)
        .with("keep_data", keep_data)
        .with("used_by", Json::Arr(Vec::new()))
        .with("summary", summary);
    Ok(Made { reply, planned: Some(Planned { action: Action::Remove, name: name.to_string(), program: prog, parts: versions, download: 0, keep_data, made: Instant::now() }) })
}

pub fn make(cx: &Ctx, action: Action, name: &str) -> Result<Made, String> {
    make_with(cx, action, name, false)
}

/// `make` with the removal's `keep_data` (only a removal reads it).
pub fn make_with(cx: &Ctx, action: Action, name: &str, keep_data: bool) -> Result<Made, String> {
    if name.is_empty() || name.len() > 256 || name.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(format!("{name:?} ist kein Programmname"));
    }
    let st = cx.store();
    match action {
        Action::Remove => remove_plan(&st, name, keep_data),
        _ => closure_plan(cx, &st, action, name),
    }
}

/// Runs a plan (in `work`'s thread). The final line of `do`: `{"ok":true,"done":true,...}` or the refusal.
pub fn run(cx: &Ctx, p: &Planned) -> Json {
    let fail = |code: &str, msg: String, report: Vec<String>| {
        crate::event("pkg.daemon.do_refused", &[("action", p.action.name()), ("name", &p.name), ("reason", &msg)]);
        Json::obj().with("ok", false).with("error", code).with("message", msg).with("report", Json::Arr(report.iter().map(|l| s(l)).collect()))
    };
    // The plan must still hold: the same packages as when Georg (or Claude) saw it.
    if p.action != Action::Remove {
        match make(cx, p.action, &p.name) {
            Ok(m) if holds(&p.parts, m.planned.as_ref()) => {}
            Ok(Made { planned: None, reply }) => {
                return fail("stale_plan", format!("Plan gilt nicht mehr: {}", reply.str("summary").unwrap_or("nicht mehr ausführbar")), Vec::new());
            }
            Ok(_) => return fail("stale_plan", format!("Plan veraltet: {} bringt jetzt andere Teile als geplant; bitte neu planen", p.name), Vec::new()),
            Err(e) => return fail("stale_plan", format!("Plan gilt nicht mehr: {e}"), Vec::new()),
        }
    } else if cx.store().versions(&p.program).unwrap_or_default() != p.parts {
        return fail("stale_plan", format!("Plan veraltet: die Fassungen von {} haben sich geändert; bitte neu planen", p.program), Vec::new());
    }
    let st = cx.store();
    let cfg = match cx.config(&st) {
        Ok(c) => c,
        Err(e) => return fail("config", e, Vec::new()),
    };
    let done = |lines: Vec<String>| Json::obj().with("ok", true).with("done", true).with("action", p.action.name()).with("program", p.program.as_str()).with("report", Json::Arr(lines.iter().map(|l| s(l)).collect()));
    match p.action {
        Action::Install => {
            let r = apply::install(&st, &cfg, &p.name, &Fetcher::standard());
            match r.refused {
                Some(why) => fail("refused", why, r.lines),
                None => done(r.lines.clone()).with("version", r.version.as_str()).with("already", r.already),
            }
        }
        Action::Update => {
            let fetcher = Fetcher::standard();
            let ucx = update::Ctx { st: &st, cfg: &cfg, fetcher: &fetcher, now: crate::now() };
            let r = update::run(&ucx, &[], Some(&p.program), false);
            if r.failed > 0 { fail("refused", format!("{}: Aktualisierung abgelehnt (Gründe im Bericht)", p.program), r.lines) } else { done(r.lines) }
        }
        Action::Remove => match bitspkg::remove::remove(&st, &p.program, p.keep_data) {
            Ok(r) => done(vec![format!("{} entfernt ({}; Datenordner {}; {} frei)", p.program, r.versions.join(" "), if r.data_removed { "entfernt" } else if p.keep_data { "behalten" } else { "keiner" }, human(r.freed))]).with("freed", r.freed),
            Err(e) => fail("refused", e, Vec::new()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned(parts: &[&str]) -> Planned {
        Planned { action: Action::Install, name: "jq".into(), program: "jq".into(), parts: parts.iter().map(|p| p.to_string()).collect(), download: 0, keep_data: false, made: Instant::now() }
    }

    /// CR-PK-7a-1: a removal plan with `keep_data` says the data folder stays, carries it, and the removal it
    /// runs leaves the folder; without it the folder goes.
    #[test]
    fn keep_data_rides_from_the_plan_to_the_removal() {
        let d = std::env::temp_dir().join(format!("bitspkgd-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let t = d.join("tree");
        std::fs::create_dir_all(t.join("bin")).unwrap();
        std::fs::write(t.join("package.toml"), "format = 1\nname = \"demo\"\nversion = \"1.0\"\nen = \"A demo\"\nprogram = \"bin/demo\"\n").unwrap();
        std::fs::write(t.join("bin/demo"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(t.join("bin/demo"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let pkg = d.join("pkg");
        bitspkg::build::build(&t, None, Some(&pkg)).unwrap();
        let st = Store::new(&d.join("root"));
        for keep in [true, false] {
            bitspkg::install::install(&st, &pkg).unwrap();
            let data = st.data_dir("demo");
            std::fs::create_dir_all(&data).unwrap();
            std::fs::write(data.join("mine.txt"), "Georgs Datei").unwrap();
            let m = remove_plan(&st, "demo", keep).unwrap();
            assert_eq!(m.reply.get("keep_data").and_then(Json::as_bool), Some(keep), "{}", m.reply.to_text());
            assert_eq!(m.reply.get("data_removed").and_then(Json::as_bool), Some(!keep));
            assert!(m.reply.str("summary").unwrap().contains(if keep { "bleibt" } else { "wird gelöscht" }));
            let p = m.planned.unwrap();
            assert_eq!(p.keep_data, keep);
            let r = bitspkg::remove::remove(&st, &p.program, p.keep_data).unwrap();
            assert_eq!((r.data_removed, data.join("mine.txt").exists()), (!keep, keep));
            let _ = std::fs::remove_dir_all(&data);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_plan_holds_only_with_the_same_parts() {
        let was = vec!["fixture/jq 1.8.2-1".to_string(), "fixture/oniguruma 6.9.10-1".to_string()];
        assert!(holds(&was, Some(&planned(&["fixture/jq 1.8.2-1", "fixture/oniguruma 6.9.10-1"]))));
        assert!(!holds(&was, Some(&planned(&["fixture/jq 1.8.3-1", "fixture/oniguruma 6.9.10-1"]))), "a refresh brought another version: stale");
        assert!(!holds(&was, Some(&planned(&["fixture/jq 1.8.2-1"]))), "another closure: stale");
        assert!(!holds(&was, None), "no longer runnable: stale");
    }
}
