//! The PK-6a commands of `bitspkg`: refresh, sources, search, info --repo, vercmp; PK-6c's plan install; PK-6d's
//! install NAME and info --scripts; PK-6e's update [NAME] [--check].

use super::dbsig::{DbVerifier, NoKeyring, Pgp};
use super::download::Fetcher;
use super::refresh::{self, Outcome, Refresh};
use super::search;
use super::sources::{self, Config};
use crate::settings::Settings;
use crate::store::Store;
use std::path::Path;

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// `NAME VALUE` taken out of `a`; NAME without a value is an error (never silently the defaults, R9).
fn take_opt(a: &mut Vec<String>, name: &str) -> Result<Option<String>, String> {
    let Some(i) = a.iter().position(|x| x == name) else { return Ok(None) };
    a.remove(i);
    if i < a.len() && !a[i].starts_with("--") { Ok(Some(a.remove(i))) } else { Err(format!("{name} braucht einen Wert")) }
}

/// `--sources FILE`, else settingsd's keys (registered on the way), else D1's defaults (said in an event).
pub fn config(st: &Store, file: Option<&str>) -> Result<Config, String> {
    if let Some(f) = file {
        return sources::from_file(Path::new(f));
    }
    match st.hooks.settings_socket() {
        Some(sock) => {
            let s = Settings { socket: sock.to_path_buf() };
            match sources::from_settings(&s) {
                Ok(c) => Ok(c),
                Err(e) => {
                    refresh::event("pkg.sources.defaults", &[("reason", &e)]);
                    Ok(sources::defaults(&format!("Voreinstellung (settingsd: {e})")))
                }
            }
        }
        None => Ok(sources::defaults("Voreinstellung (--settings none)")),
    }
}

/// Every source's database (PK-6a) with PK-6b's verifier (a database `.sig` on the mirror is checked also where
/// it is optional, PK-6d); refresh and update share it, so an older database is refused on both ways.
pub fn refresh_all(st: &Store, cfg: &Config, fetcher: &Fetcher) -> Result<Vec<refresh::RepoReport>, String> {
    let (pgp, none);
    let verifier: &dyn DbVerifier = match Pgp::load(&st.root) {
        Ok(p) => {
            pgp = p;
            &pgp
        }
        Err(e) => {
            refresh::event("pkg.keyring.missing", &[("reason", &e)]);
            none = NoKeyring(e);
            &none
        }
    };
    Refresh { root: st.root.clone(), fetcher, verifier, now: now() }.run(cfg)
}

pub fn run(st: &Store, cmd: &str, mut a: Vec<String>) -> Result<i32, String> {
    let file = take_opt(&mut a, "--sources")?;
    let mut reset = Vec::new();
    while let Some(r) = take_opt(&mut a, "--reset")? {
        reset.push(r);
    }
    if !reset.is_empty() && cmd != "refresh" {
        return Err("--reset gehört zu bitspkg refresh".into());
    }
    let mut flag = |f: &str| {
        let n = a.len();
        a.retain(|x| x != f);
        a.len() != n
    };
    let repo_flag = flag("--repo");
    let scripts_flag = flag("--scripts");
    let _from_repo = flag("--from-repo");
    let check_flag = flag("--check");
    if check_flag && cmd != "update" {
        return Err("--check gehört zu bitspkg update".into());
    }
    if let Some(x) = a.iter().find(|x| x.starts_with("--")) {
        return Err(format!("unknown option {x}"));
    }
    match (cmd, a.len()) {
        ("vercmp", 2) => {
            println!("{}", super::vercmp::vercmp(&a[0], &a[1]));
            Ok(0)
        }
        ("refresh", 0) => {
            let cfg = config(st, file.as_deref())?;
            println!("Quellen: {} ({}, CPU-Stufe {})", cfg.sources.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(" "), cfg.origin, cfg.level.name());
            for r in &reset {
                refresh::reset(&st.root, r)?;
                println!("{r}: zurückgesetzt");
            }
            let rs = refresh_all(st, &cfg, &Fetcher::standard())?;
            print!("{}", refresh::render(&rs, now()));
            let bad = rs.iter().filter(|r| matches!(r.outcome, Outcome::Refused(_))).count();
            Ok(if bad > 0 { 1 } else { 0 })
        }
        ("sources", 0) => {
            let cfg = config(st, file.as_deref())?;
            println!("Quellen ({}, CPU-Stufe {}):", cfg.origin, cfg.level.name());
            let sync = refresh::sync_dir(&st.root);
            for s in &cfg.sources {
                let lvl = sources::Level::of_repo(&s.name);
                let cpu = if sources::cpu_can(lvl) { String::new() } else { format!(" (dieser Prozessor kann {} nicht)", lvl.name()) };
                let state = match refresh::load(&sync, &s.name) {
                    Ok(i) => {
                        let days = refresh::age_days(i.newest, now());
                        let stale = if days > refresh::STALE_DAYS { ", veraltet" } else { "" };
                        format!("{} Pakete, neuestes vom {} ({days} Tage{stale})", i.pkgs.len(), search::date(i.newest))
                    }
                    Err(_) => "noch nicht geladen".into(),
                };
                println!("{}: {state}; Datenbank-Signatur {}, Paket-Signatur {}, Schlüsselbund {}{cpu}", s.name, s.sig_db.name(), s.sig_pkg.name(), s.keyring);
                println!("  Server: {}", s.servers.join(" "));
            }
            Ok(0)
        }
        ("search", 1) => {
            let cfg = config(st, file.as_deref())?;
            let (idx, missing) = search::indexes(&st.root, &cfg);
            for m in &missing {
                eprintln!("bitspkg: {m}");
            }
            if idx.is_empty() {
                return Err("keine Quelle geladen: zuerst bitspkg refresh".into());
            }
            for h in search::search(&idx, &a[0]) {
                let rust = if h.pkg.rust() { " [Rust]" } else { "" };
                let hidden = h.shadowed_by.as_ref().map(|r| format!(" (verdeckt durch {r})")).unwrap_or_default();
                println!("{}/{} {}{rust}{hidden}", h.repo, h.pkg.name, h.pkg.version);
                if !h.pkg.desc.is_empty() {
                    println!("    {}", h.pkg.desc);
                }
            }
            Ok(0)
        }
        ("info", 1) if repo_flag => {
            let cfg = config(st, file.as_deref())?;
            let (idx, _) = search::indexes(&st.root, &cfg);
            let (repo, p) = search::find(&idx, &a[0]).ok_or_else(|| format!("{} ist in keiner geladenen Quelle", a[0]))?;
            for l in search::info_lines(repo, p) {
                println!("{l}");
            }
            Ok(0)
        }
        ("update", 0 | 1) => {
            // PK-6e: refresh, keyring and mirror list packages, then every program (or NAME) re-resolved.
            let cfg = config(st, file.as_deref())?;
            let fetcher = Fetcher::standard();
            let rs = refresh_all(st, &cfg, &fetcher)?;
            print!("{}", refresh::render(&rs, now()));
            let cx = super::update::Ctx { st, cfg: &cfg, fetcher: &fetcher, now: now() };
            let rep = super::update::run(&cx, &rs, a.first().map(String::as_str), check_flag);
            print!("{}", rep.text());
            Ok(if rep.failed > 0 { 1 } else { 0 })
        }
        ("install", 1) => {
            let cfg = config(st, file.as_deref())?;
            let rep = super::apply::install(st, &cfg, &a[0], &Fetcher::standard());
            print!("{}", rep.text());
            Ok(if rep.refused.is_some() { 1 } else { 0 })
        }
        ("info", 1) if scripts_flag => {
            for l in super::apply::scripts(st, &a[0])? {
                println!("{l}");
            }
            Ok(0)
        }
        ("plan", 2) if a[0] == "install" => {
            let cfg = config(st, file.as_deref())?;
            let (idx, missing) = search::indexes(&st.root, &cfg);
            for m in &missing {
                eprintln!("bitspkg: {m}");
            }
            if idx.is_empty() {
                return Err("keine Quelle geladen: zuerst bitspkg refresh".into());
            }
            let u = super::resolve::Universe::new(&idx, cfg.level);
            // PK-6e hands in the previous version's packages; a first install has none.
            let plan = super::resolve::Closure::resolve(&a[1], &u, &super::resolve::policy::Prefs::builtin(), &[]);
            print!("{}", plan.render());
            Ok(if plan.ok() { 0 } else { 1 })
        }
        _ => {
            eprintln!("usage: bitspkg refresh [--reset REPO]|sources|search QUERY|info --repo [REPO/]NAME|info --scripts NAME|plan install [REPO/]NAME|install [--from-repo] [REPO/]NAME|update [NAME] [--check] [--sources FILE]; bitspkg vercmp A B");
            Ok(2)
        }
    }
}
