//! `bitspkg update [NAME] [--check]` (PK-6e, plan 1.9; D5, D8): refresh the databases (the same refresh, so an
//! older database stays refused), bring the keyring and mirror list packages up to date first (ownpart.rs: the
//! programs are then checked with the new keyring), then re-resolve every program from the repositories. A
//! program whose closure changed gets a new version through PK-6d's install (unchanged `.pkgs` entries are
//! reused, only the new packages are downloaded; `current` switches in one step and the old version stays
//! `previous`). `--check` only reports and writes nothing but the refresh's state. Model: pacman 7.1
//! sync.c:200 alpm_sync_sysupgrade (each installed package against the first source that has its name, newer
//! only; :85 check_literal: an older one is "local is newer" and kept; :124 check_replacers).

use super::apply;
use super::download::Fetcher;
use super::index::Index;
use super::ownpart;
use super::refresh::{self, Outcome, RepoReport, event};
use super::resolve::{Closure, Universe, dep, policy};
use super::search;
use super::sources::{Check, Config, Install};
use super::vercmp::vercmp;
use crate::fsx;
use crate::manifest::Origin;
use crate::names::program_of;
use crate::store::Store;
use std::path::{Path, PathBuf};

/// One closure against another, by pacman name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Diff {
    /// (name, old version, new version) where the new one is newer.
    pub newer: Vec<(String, String, String)>,
    pub older: Vec<(String, String, String)>,
    /// Same version, another build (other sha256: another repository's).
    pub rebuilt: Vec<String>,
    pub added: Vec<(String, String)>,
    pub removed: Vec<(String, String)>,
}

impl Diff {
    pub fn same(&self) -> bool {
        self.newer.is_empty() && self.older.is_empty() && self.rebuilt.is_empty() && self.added.is_empty() && self.removed.is_empty()
    }

    /// An update in pacman's sense: something newer, added or gone, nothing older.
    pub fn is_update(&self) -> bool {
        self.older.is_empty() && (!self.newer.is_empty() || !self.added.is_empty() || !self.removed.is_empty())
    }

    /// Something would be newer, added or gone, but a part would get older: not done, named (`Report::blocked`).
    pub fn is_blocked(&self) -> bool {
        !self.older.is_empty() && (!self.newer.is_empty() || !self.added.is_empty() || !self.removed.is_empty())
    }

    /// "3 Teile neuer (a 1 → 2, ...)", "1 Teil älter (...)", "1 Teil neu (...)", "1 Teil weg (...)".
    pub fn text(&self) -> String {
        let n = |k: usize, w: &str| format!("{k} {} {w}", if k == 1 { "Teil" } else { "Teile" });
        let ch = |v: &[(String, String, String)]| v.iter().map(|(p, a, b)| format!("{p} {a} → {b}")).collect::<Vec<_>>().join(", ");
        let one = |v: &[(String, String)]| v.iter().map(|(p, a)| format!("{p} {a}")).collect::<Vec<_>>().join(", ");
        let mut parts = Vec::new();
        if !self.newer.is_empty() {
            parts.push(format!("{} ({})", n(self.newer.len(), "neuer"), ch(&self.newer)));
        }
        if !self.older.is_empty() {
            parts.push(format!("{} ({})", n(self.older.len(), "älter"), ch(&self.older)));
        }
        if !self.added.is_empty() {
            parts.push(format!("{} ({})", n(self.added.len(), "dazu"), one(&self.added)));
        }
        if !self.removed.is_empty() {
            parts.push(format!("{} ({})", n(self.removed.len(), "weg"), one(&self.removed)));
        }
        if !self.rebuilt.is_empty() {
            parts.push(format!("{} ({})", n(self.rebuilt.len(), "anders gebaut, gleiche Version"), self.rebuilt.join(", ")));
        }
        if parts.is_empty() { "gleiche Teile".into() } else { parts.join("; ") }
    }
}

/// (name, version, sha256) of a closure.
pub type Part = (String, String, String);

pub fn parts_of(origins: &[Origin]) -> Vec<Part> {
    origins.iter().map(|o| (o.name.clone(), o.version.clone(), o.sha256.clone())).collect()
}

/// The difference from `old` to `new` (pacman's version order, vercmp).
pub fn diff(old: &[Part], new: &[Part]) -> Diff {
    let mut d = Diff::default();
    for (n, v, h) in new {
        match old.iter().find(|(o, _, _)| o == n) {
            None => d.added.push((n.clone(), v.clone())),
            Some((_, ov, oh)) => match vercmp(v, ov) {
                c if c > 0 => d.newer.push((n.clone(), ov.clone(), v.clone())),
                c if c < 0 => d.older.push((n.clone(), ov.clone(), v.clone())),
                _ if h != oh => d.rebuilt.push(n.clone()),
                _ => {}
            },
        }
    }
    for (n, v, _) in old {
        if !new.iter().any(|(x, _, _)| x == n) {
            d.removed.push((n.clone(), v.clone()));
        }
    }
    d
}

/// D5's schedule as a pure function (bitspkgd's timer calls it; the time is handed in): is a check due?
/// A clock that went back by more than a day counts as due (the last check is then unknown).
pub fn check_due(check: Check, last: Option<i64>, now: i64) -> bool {
    let every = match check {
        Check::Never => return false,
        Check::Daily => 86_400,
        Check::Weekly => 7 * 86_400,
    };
    match last {
        None => true,
        Some(l) => now - l >= every || l - now > 86_400,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum After {
    Nothing,
    /// D5: the page shows "N Aktualisierungen", one click does them all.
    Ask(usize),
    /// D5's alternative, only when chosen: installed at night by bitspkgd.
    InstallAtNight(usize),
}

pub fn after_check(install: Install, found: usize) -> After {
    match (found, install) {
        (0, _) => After::Nothing,
        (n, Install::Ask) => After::Ask(n),
        (n, Install::Night) => After::InstallAtNight(n),
    }
}

/// The record of the last check (`<sync>/update-check`): its time and how many updates it found.
pub fn check_record(root: &Path) -> PathBuf {
    refresh::sync_dir(root).join("update-check")
}

pub fn last_check(root: &Path) -> Option<(i64, usize)> {
    let t = std::fs::read_to_string(check_record(root)).ok()?;
    let get = |k: &str| t.lines().find_map(|l| l.strip_prefix(k)?.trim().parse::<i64>().ok());
    Some((get("checked =")?, get("found =").unwrap_or(0).max(0) as usize))
}

#[derive(Debug, Default)]
pub struct Report {
    pub lines: Vec<String>,
    /// Updates found (programs and parts).
    pub found: usize,
    /// Refusals: a database, a part, a program.
    pub failed: usize,
    /// Programs whose update needs a part older than the installed one (left as they are, named in the report).
    pub blocked: usize,
    /// Why the check did not see every source (a refused database, no index): the time of such a check is not
    /// recorded, so D5's schedule tries again instead of trusting it for a day.
    pub incomplete: Vec<String>,
}

impl Report {
    fn line(&mut self, s: impl Into<String>) {
        self.lines.push(s.into());
    }
    pub fn text(&self) -> String {
        self.lines.iter().map(|l| format!("{l}\n")).collect()
    }
}

/// One installed program from the repositories: its pacman name (the closure's top) and closure.
struct Prog {
    name: String,
    version: String,
    top: String,
    parts: Vec<Part>,
    previous: Vec<String>,
}

fn programs(st: &Store, only: Option<&str>) -> Result<(Vec<Prog>, Vec<String>), String> {
    let names = match only {
        Some(n) => {
            crate::names::check_name(n)?;
            vec![n.to_string()]
        }
        None => st.packages()?,
    };
    let (mut out, mut notes) = (Vec::new(), Vec::new());
    for n in names {
        let Some(v) = st.current(&n) else {
            if only.is_some() {
                return Err(format!("{n} ist nicht installiert"));
            }
            continue;
        };
        let m = match st.manifest(&n, &v) {
            Ok(m) => m,
            Err(e) => {
                notes.push(format!("{n}: Manifest unlesbar ({e}): bleibt"));
                continue;
            }
        };
        if m.origins.is_empty() {
            if only.is_some() {
                return Err(format!("{n} kommt nicht aus den Quellen (ein BitsOS-Paket): bitspkg install ORDNER ersetzt es"));
            }
            continue;
        }
        let top = m.origins.iter().find(|o| program_of(&o.name).as_deref() == Ok(n.as_str())).unwrap_or(&m.origins[0]).name.clone();
        out.push(Prog { name: n, version: v, top, parts: parts_of(&m.origins), previous: m.origins.into_iter().map(|o| o.name).collect() });
    }
    Ok((out, notes))
}

/// The closure `p` would get from the databases now (with the previous closure's names, as install does).
fn resolve_now(p: &Prog, idx: &[(String, Index)], u: &Universe) -> Result<Vec<Part>, String> {
    let plan = Closure::resolve(&p.top, u, &policy::Prefs::builtin(), &p.previous);
    if !plan.ok() {
        let why: Vec<String> = plan.refused.iter().map(|(n, r)| format!("{n}: {r}")).collect();
        return Err(if why.is_empty() { "keine Pakete".into() } else { why.join("; ") });
    }
    plan.packages
        .iter()
        .map(|x| {
            let pkg = idx.iter().find(|(r, _)| *r == x.repo).and_then(|(_, i)| i.pkgs.get(&x.name)).ok_or_else(|| format!("{}/{} fehlt im Index", x.repo, x.name))?;
            Ok((pkg.name.clone(), pkg.version.clone(), pkg.sha256.clone()))
        })
        .collect()
}

/// sync.c:124 check_replacers: a package of the sources (not of the same name) whose %REPLACES% entry, read as a
/// dependency, the installed package satisfies (deps.c:413 literal: `replaces=foo<2` only replaces foo before 2).
pub fn replacer(idx: &[(String, Index)], name: &str, version: &str) -> Option<String> {
    idx.iter().find_map(|(r, i)| {
        i.pkgs
            .values()
            .filter(|p| p.name != name)
            .find(|p| p.replaces.iter().any(|x| dep::parse(x).is_ok_and(|d| dep::literal(name, version, &d))))
            .map(|p| format!("{r}/{}", p.name))
    })
}

pub struct Ctx<'a> {
    pub st: &'a Store,
    pub cfg: &'a Config,
    pub fetcher: &'a Fetcher,
    pub now: i64,
}

/// After the refresh (`rs`, already printed by the caller): parts, then programs.
pub fn run(cx: &Ctx, rs: &[RepoReport], only: Option<&str>, check: bool) -> Report {
    let mut rep = Report::default();
    event("pkg.update.start", &[("target", only.unwrap_or("alle")), ("check", if check { "yes" } else { "no" })]);
    if let Err(e) = run_inner(cx, rs, only, check, &mut rep) {
        rep.failed += 1;
        event("pkg.update.refused", &[("reason", &e)]);
        rep.line(format!("abgelehnt: {e}"));
        rep.incomplete.push(e);
    }
    if rep.blocked > 0 {
        rep.line(format!("Blockiert: {} (ein Teil in den Quellen ist älter als der installierte; siehe oben)", rep.blocked));
    }
    if check && !rep.incomplete.is_empty() {
        // Only a check that saw every source counts for D5's schedule (check_due): this one is tried again.
        event("pkg.update.check_incomplete", &[("reason", &rep.incomplete.join("; ")), ("found", &rep.found.to_string())]);
        rep.line(format!(
            "Prüfung unvollständig ({}): {} Aktualisierungen bisher gefunden; der Zeitpunkt wird nicht gespeichert, die nächste Prüfung versucht es wieder",
            rep.incomplete.join("; "),
            rep.found
        ));
    } else if check {
        let rec = format!("checked = {}\nfound = {}\nblocked = {}\n", cx.now, rep.found, rep.blocked);
        if let Err(e) = fsx::write_atomic(&check_record(&cx.st.root), rec.as_bytes(), 0o644) {
            rep.line(format!("Hinweis: Zeitpunkt der Prüfung nicht gespeichert ({e})"));
        }
        match after_check(cx.cfg.updates.install, rep.found) {
            After::Nothing => rep.line("Nichts zu aktualisieren"),
            After::Ask(n) => rep.line(format!("{n} Aktualisierungen gefunden (installiert wird nur auf Wunsch: bitspkg update)")),
            After::InstallAtNight(n) => rep.line(format!("{n} Aktualisierungen gefunden (pkg.updates.install = night: bitspkgd installiert sie nachts)")),
        }
    }
    event("pkg.update.done", &[("found", &rep.found.to_string()), ("failed", &rep.failed.to_string()), ("check", if check { "yes" } else { "no" })]);
    rep
}

fn run_inner(cx: &Ctx, rs: &[RepoReport], only: Option<&str>, check: bool, rep: &mut Report) -> Result<(), String> {
    let st = cx.st;
    for r in rs {
        if let Outcome::Refused(_) = r.outcome {
            rep.failed += 1;
            rep.incomplete.push(format!("{}: Datenbank abgelehnt", r.repo));
            rep.line(format!("Hinweis: {}: Datenbank abgelehnt (oben); die bisherige gilt weiter", r.repo));
        }
    }
    let (idx, missing) = search::indexes(&st.root, cx.cfg);
    for m in missing {
        rep.line(format!("Hinweis: {m}"));
    }
    if idx.is_empty() {
        return Err("keine Quelle geladen: zuerst bitspkg refresh".into());
    }
    // ---- the keyring and mirror list packages first (the programs are checked with the new keyring) ----
    let (steps, notes) = ownpart::plan(&st.root, &idx, &ownpart::wanted(&st.root, cx.cfg));
    for n in notes {
        rep.line(n);
    }
    for s in &steps {
        rep.found += 1;
        rep.line(format!("Aktualisierung: {}", ownpart::describe(s)));
        if check {
            continue;
        }
        match ownpart::install(st, cx.cfg, s, cx.fetcher) {
            Ok(l) => rep.lines.extend(l.into_iter().map(|l| format!("  {l}"))),
            Err(e) => {
                // pacman's transaction stops at a bad signature: nothing else is updated in this run.
                rep.failed += 1;
                rep.line(format!("  abgelehnt: {e}"));
                let what = if matches!(s.want.what, ownpart::What::Keyring(_)) { "der bisherige Schlüsselbund bleibt aktiv" } else { "die bisherige Spiegelliste bleibt" };
                return Err(format!("{} {} nicht übernommen ({what}); Programme nicht aktualisiert", s.want.de(), s.want.pkg));
            }
        }
    }
    // ---- the programs ----
    let (progs, notes) = programs(st, only)?;
    for n in notes {
        rep.line(n);
    }
    let u = Universe::new(&idx, cx.cfg.level);
    let mut parts_newer = 0;
    for p in &progs {
        let top_version = p.parts.iter().find(|(n, _, _)| *n == p.top).map(|x| x.1.as_str()).unwrap_or("");
        let by = replacer(&idx, &p.top, top_version);
        if search::find(&idx, &p.top).is_none() {
            let to = by.map(|r| format!("; ersetzt durch {r} (bitspkg install {r})")).unwrap_or_default();
            rep.line(format!("{} {}: {} ist in keiner Quelle mehr{to}; bleibt", p.name, p.version, p.top));
            continue;
        }
        if let Some(r) = by {
            // check_replacers asks before replacing; bitspkg never swaps a program for another name by itself.
            event("pkg.update.replacer", &[("name", &p.name), ("by", &r)]);
            rep.line(format!("Hinweis: {} {}: {r} ersetzt {} laut %REPLACES% (bitspkg install {r}, dann bitspkg remove {}); {} wird weiter aktualisiert", p.name, p.version, p.top, p.name, p.name));
        }
        let new = match resolve_now(p, &idx, &u) {
            Ok(n) => n,
            Err(e) => {
                rep.failed += 1;
                rep.line(format!("{} {}: neu auflösen abgelehnt ({e}); bleibt", p.name, p.version));
                continue;
            }
        };
        let d = diff(&p.parts, &new);
        if d.is_blocked() {
            // pacman (sync.c:200) would take the newer packages and keep the newer installed part; a program
            // version here is one closure from the sources, so the update waits until the sources catch up.
            rep.blocked += 1;
            let older: Vec<String> = d.older.iter().map(|(n, a, b)| format!("{n} {b} (installiert {a})")).collect();
            event("pkg.update.blocked", &[("name", &p.name), ("version", &p.version), ("by", &older.join(", "))]);
            rep.line(format!("{} {}: Aktualisierung blockiert durch {}: in den Quellen älter als installiert (pacman: local is newer); sonst {}; bleibt", p.name, p.version, older.join(", "), d.text()));
            continue;
        }
        if !d.is_update() {
            let why = if d.older.is_empty() { String::new() } else { format!(": in den Quellen {} (installiert ist neuer, pacman: local is newer); bleibt", d.text()) };
            rep.line(format!("{} {}: aktuell{why}", p.name, p.version));
            continue;
        }
        rep.found += 1;
        parts_newer += d.newer.len();
        event("pkg.update.found", &[("name", &p.name), ("version", &p.version), ("newer", &d.newer.len().to_string())]);
        rep.line(format!("{} {}: {}", p.name, p.version, d.text()));
        if check {
            continue;
        }
        let r = apply::install(st, cx.cfg, &p.top, cx.fetcher);
        rep.lines.extend(r.lines.iter().map(|l| format!("  {l}")));
        match r.refused {
            Some(e) => {
                rep.failed += 1;
                rep.line(format!("  abgelehnt: {e}; {} {} bleibt aktuell", p.name, p.version));
            }
            None => rep.line(format!("  {} jetzt {} (vorher {}, bleibt zum Zurückgehen: bitspkg rollback {})", p.name, r.version, p.version, p.name)),
        }
    }
    if !check {
        let teile = if parts_newer == 1 { "Teil" } else { "Teile" };
        rep.line(format!("Aktualisiert: {} ({parts_newer} {teile} neuer); Fehler: {}", rep.found, rep.failed));
    }
    Ok(())
}

/// What `rollback` changed: the closure difference from the version left to the one now current ("1 Teil älter").
pub fn rollback_lines(st: &Store, name: &str, from: &str, to: &str) -> Vec<String> {
    let (Ok(a), Ok(b)) = (st.manifest(name, from), st.manifest(name, to)) else { return Vec::new() };
    if a.origins.is_empty() && b.origins.is_empty() {
        return Vec::new();
    }
    let when = std::fs::metadata(st.version_dir(name, to).join(crate::manifest::FILE_NAME))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| format!(" (installiert am {})", search::date(d.as_secs() as i64)))
        .unwrap_or_default();
    vec![format!("{name} {to}{when}: {}", diff(&parts_of(&a.origins), &parts_of(&b.origins)).text())]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(n: &str, v: &str, h: &str) -> Part {
        (n.into(), v.into(), h.into())
    }

    #[test]
    fn diff_counts_parts_in_pacman_order() {
        let old = [p("jq", "1.8.2-1", "a"), p("oniguruma", "1:6.9.10-1", "b"), p("glibc", "2.42-1", "c"), p("gone", "1-1", "d")];
        let new = [p("jq", "1.8.2-1", "a"), p("oniguruma", "1:6.9.10-2", "e"), p("glibc", "2.42-1", "f"), p("extra", "2-1", "g")];
        let d = diff(&old, &new);
        assert_eq!(d.newer, vec![("oniguruma".into(), "1:6.9.10-1".into(), "1:6.9.10-2".into())]);
        assert_eq!(d.rebuilt, vec!["glibc".to_string()]);
        assert_eq!((d.added.len(), d.removed.len()), (1, 1));
        assert!(d.is_update());
        assert_eq!(d.text(), "1 Teil neuer (oniguruma 1:6.9.10-1 → 1:6.9.10-2); 1 Teil dazu (extra 2-1); 1 Teil weg (gone 1-1); 1 Teil anders gebaut, gleiche Version (glibc)");
        let back = diff(&new[..2], &old[..2]);
        assert_eq!(back.text(), "1 Teil älter (oniguruma 1:6.9.10-2 → 1:6.9.10-1)");
        assert!(!back.is_update(), "an older part is never an update (check_literal: local is newer)");
        assert!(!back.is_blocked(), "only older: up to date");
        let skew = diff(&[p("jq", "1.8.1-1", "a"), p("oniguruma", "6.9.10-2", "b")], &[p("jq", "1.8.2-1", "c"), p("oniguruma", "6.9.10-1", "d")]);
        assert!(skew.is_blocked() && !skew.is_update(), "a newer jq with an older oniguruma in the sources is blocked, never a downgrade");
        assert!(diff(&old, &old).same());
    }

    #[test]
    fn replacer_honours_the_version_and_skips_itself() {
        let pk = |n: &str, r: &[&str]| (n.to_string(), crate::repo::syncdb::Pkg { name: n.into(), version: "2-1".into(), replaces: r.iter().map(|x| x.to_string()).collect(), ..Default::default() });
        let pkgs = [pk("foo", &["foo<1"]), pk("foo-ng", &["foo<2"]), pk("bar", &["baz"])].into_iter().collect();
        let idx = vec![("extra".to_string(), Index { db_sha256: String::new(), newest: 0, pkgs })];
        assert_eq!(replacer(&idx, "foo", "1.5-1").as_deref(), Some("extra/foo-ng"), "foo<2 replaces foo 1.5, foo itself never");
        assert_eq!(replacer(&idx, "foo", "2.1-1"), None, "foo 2.1 does not satisfy foo<2");
        assert_eq!(replacer(&idx, "baz", "9-1").as_deref(), Some("extra/bar"));
    }

    #[test]
    fn check_schedule_is_daily_weekly_or_never() {
        let day = 86_400;
        assert!(check_due(Check::Daily, None, 1000));
        assert!(!check_due(Check::Daily, Some(1000), 1000 + day - 1));
        assert!(check_due(Check::Daily, Some(1000), 1000 + day));
        assert!(!check_due(Check::Weekly, Some(1000), 1000 + 6 * day));
        assert!(check_due(Check::Weekly, Some(1000), 1000 + 7 * day));
        assert!(!check_due(Check::Never, None, 1000));
        assert!(check_due(Check::Daily, Some(10 * day), 8 * day), "a clock that went back two days");
        assert!(!check_due(Check::Daily, Some(10 * day), 10 * day - 3600));
        assert_eq!(after_check(Install::Ask, 3), After::Ask(3));
        assert_eq!(after_check(Install::Ask, 0), After::Nothing);
        assert_eq!(after_check(Install::Night, 2), After::InstallAtNight(2));
    }
}
