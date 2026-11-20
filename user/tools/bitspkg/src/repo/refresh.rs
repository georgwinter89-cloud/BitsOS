//! `bitspkg refresh` (plan 1.3): every source's database from the first mirror that gives a good one
//! (pacman/lib/libalpm/dload.c:438 curl_retry_next_server: the next server on any failure), checked, parsed,
//! indexed, then renamed into place: `<sync>/<repo>.db.part`, fsync, rename (be_sync.c:139 sync_db_update with
//! dload.c's `.part` files). A mirror's database is refused when it is older than the newest one accepted before
//! for that repository (its largest %BUILDDATE%, and the mirror's `lastupdate` where it has one): a replay or a
//! stale mirror, which pacman would take. Nothing outside `<sync>` is written (B2).

use super::dbsig::DbVerifier;
use super::download::{Fetcher, Got, read_small};
use super::index;
use super::sources::{self, Config, Level, Sig, Source};
use super::syncdb::{self, DB_MAX};
use crate::fsx;
use crate::toml::{self, Value};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const SIG_MAX: u64 = 64 << 10;
/// A database older than this is "veraltet" on the page (plan 1.3).
pub const STALE_DAYS: i64 = 14;
const LOCK_WAIT: Duration = Duration::from_secs(60);
/// How far the newest %BUILDDATE% may fall below the last accepted one when the mirror synced later (a dropped
/// or reverted newest package); a larger fall, or one without a later `lastupdate`, is refused as a replay.
pub const NEWEST_DROP_MAX: i64 = 3 * 86400;

pub fn sync_dir(root: &Path) -> PathBuf {
    root.join("data/bitspkg/sync")
}

/// The mirror lists (`include:`): those bitspkg installed from `cachyos-mirrorlist` and `pacman-mirrorlist`
/// (PK-6e, ownpart.rs: the folder is always complete), else the image's copy.
pub fn mirror_dir(root: &Path) -> PathBuf {
    let updated = root.join(MIRROR_DATA);
    if updated.is_dir() { updated } else { root.join("etc/pacman.d") }
}

/// Where PK-6e keeps the mirror lists of the installed mirror list packages.
pub const MIRROR_DATA: &str = "data/bitspkg/mirrorlist";

pub fn event(name: &str, kv: &[(&str, &str)]) {
    crate::progress::event(name, kv);
    let mut l = format!("bitspkg: event={name}");
    for (k, v) in kv {
        if v.is_empty() || v.contains([' ', '"', '=']) {
            l.push_str(&format!(" {k}=\"{}\"", v.replace('"', "'")));
        } else {
            l.push_str(&format!(" {k}={v}"));
        }
    }
    eprintln!("{l}");
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Fresh {
    pub builddate: i64,
    pub lastupdate: i64,
    pub mirror: String,
    pub accepted: i64,
    /// The database this freshness belongs to; another hash beside `<repo>.db` means a cut refresh.
    pub db_sha256: String,
    /// Who signed it (the verifier's answer), empty for an unsigned database.
    pub signer: String,
}

pub fn fresh_path(sync: &Path, repo: &str) -> PathBuf {
    sync.join(format!("{repo}.fresh"))
}

pub fn read_fresh(sync: &Path, repo: &str) -> Fresh {
    let Ok(t) = fs::read_to_string(fresh_path(sync, repo)) else { return Fresh::default() };
    let Ok(d) = toml::parse(&t) else { return Fresh::default() };
    let int = |k: &str| if let Some(Value::Int(n)) = d.root.get(k) { *n } else { 0 };
    let text = |k: &str| if let Some(Value::Str(s)) = d.root.get(k) { s.clone() } else { String::new() };
    Fresh { builddate: int("builddate"), lastupdate: int("lastupdate"), mirror: text("mirror"), accepted: int("accepted"), db_sha256: text("db_sha256"), signer: text("signer") }
}

fn write_fresh(sync: &Path, repo: &str, f: &Fresh) -> Result<(), String> {
    let t = format!(
        "# bitspkg: the newest database accepted for {repo} (freshness, plan 1.3)\nbuilddate = {}\nlastupdate = {}\nmirror = {}\naccepted = {}\ndb_sha256 = {}\nsigner = {}\n",
        f.builddate,
        f.lastupdate,
        toml::quote(&f.mirror),
        f.accepted,
        toml::quote(&f.db_sha256),
        toml::quote(&f.signer)
    );
    fsx::write_atomic(&fresh_path(sync, repo), t.as_bytes(), 0o644)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Updated { pkgs: usize, newest: i64, mirror: String },
    Unchanged { pkgs: usize, newest: i64, mirror: String },
    /// Not taken, with the reasons per mirror; the database from before (if any) stays.
    Refused(String),
}

#[derive(Debug, Clone)]
pub struct RepoReport {
    pub repo: String,
    pub outcome: Outcome,
    /// Every refusal and choice on the way (mirror fallbacks, missing lists, warnings of the database).
    pub notes: Vec<String>,
}

pub struct Refresh<'a> {
    pub root: PathBuf,
    pub fetcher: &'a Fetcher,
    pub verifier: &'a dyn DbVerifier,
    pub now: i64,
}

struct SyncLock(#[allow(dead_code)] File);

fn lock(sync: &Path) -> Result<SyncLock, String> {
    let p = sync.join(".lock");
    let f = File::options().create(true).truncate(false).write(true).open(&p).map_err(|e| fsx::err(&p, e))?;
    let start = Instant::now();
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(SyncLock(f)),
            Err(fs::TryLockError::WouldBlock) if start.elapsed() < LOCK_WAIT => std::thread::sleep(Duration::from_millis(100)),
            Err(fs::TryLockError::WouldBlock) => return Err("ein anderes bitspkg aktualisiert die Quellen gerade".into()),
            Err(fs::TryLockError::Error(e)) => return Err(fsx::err(&p, e)),
        }
    }
}

/// Makes `data/bitspkg/sync` (root 0755, each new name synced) and removes what a cut left (`*.part`, old
/// download folders).
pub fn ensure_sync(root: &Path) -> Result<PathBuf, String> {
    let sync = sync_dir(root);
    if !root.is_dir() {
        fs::create_dir_all(root).map_err(|e| fsx::err(root, e))?;
    }
    let mut p = root.to_path_buf();
    for part in ["data", "bitspkg", "sync"] {
        p = p.join(part);
        fsx::mkdir(&p, 0o755)?;
    }
    Ok(sync)
}

fn sweep(sync: &Path) {
    if let Ok(rd) = fs::read_dir(sync) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.ends_with(".part") || n.ends_with(".new") || n.starts_with("download-") {
                let _ = fsx::remove_tree(&e.path());
            }
        }
    }
}

/// `https://m/archlinux/$repo/os/$arch` → `https://m/archlinux/lastupdate`: the mirror root is what precedes
/// `$repo`. Only when the server template has `$repo` and is a URL (an include's entries are handled one by one).
fn lastupdate_url(template: &str, repo: &str) -> Option<String> {
    let i = template.find("$repo")?;
    let base = sources::fill(&template[..i], repo);
    let base = base.trim_end_matches('/');
    base.contains("://").then(|| format!("{base}/lastupdate"))
}

/// The server templates of `src` as (filled URL, lastupdate URL) pairs.
fn mirrors(src: &Source, mirror_dir: &Path) -> (Vec<(String, Option<String>)>, Vec<String>) {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut problems = Vec::new();
    for s in &src.servers {
        let one = Source { servers: vec![s.clone()], ..src.clone() };
        let (urls, p) = sources::servers(&one, mirror_dir);
        problems.extend(p);
        let templates: Vec<String> = match s.strip_prefix("include:") {
            Some(f) => fs::read_to_string(mirror_dir.join(f)).map(|t| sources::mirrorlist(&t)).unwrap_or_default(),
            None => vec![s.clone()],
        };
        for u in urls {
            let lu = templates.iter().find(|t| sources::fill(t, &src.name) == u).and_then(|t| lastupdate_url(t, &src.name));
            if !out.iter().any(|(x, _)| *x == u) {
                out.push((u, lu));
            }
        }
    }
    (out, problems)
}

impl Refresh<'_> {
    pub fn run(&self, cfg: &Config) -> Result<Vec<RepoReport>, String> {
        let sync = ensure_sync(&self.root)?;
        let _l = lock(&sync)?;
        sweep(&sync);
        let mut out = Vec::new();
        for src in &cfg.sources {
            event("pkg.refresh.start", &[("repo", &src.name)]);
            let r = self.repo(&sync, src);
            match &r.outcome {
                Outcome::Updated { pkgs, newest, mirror } => {
                    event("pkg.refresh.done", &[("repo", &src.name), ("pkgs", &pkgs.to_string()), ("newest", &newest.to_string()), ("mirror", mirror)])
                }
                Outcome::Unchanged { mirror, .. } => event("pkg.refresh.unchanged", &[("repo", &src.name), ("mirror", mirror)]),
                Outcome::Refused(why) => event("pkg.refresh.failed", &[("repo", &src.name), ("reason", why)]),
            }
            out.push(r);
        }
        let _ = fsx::write_atomic(&sync.join("last-refresh.txt"), render(&out, self.now).as_bytes(), 0o644);
        Ok(out)
    }

    fn repo(&self, sync: &Path, src: &Source) -> RepoReport {
        let mut notes = Vec::new();
        let refused = |why: String, notes: Vec<String>| RepoReport { repo: src.name.clone(), outcome: Outcome::Refused(why), notes };
        let level = Level::of_repo(&src.name);
        if !sources::cpu_can(level) {
            event("pkg.cpu.refused", &[("repo", &src.name), ("level", level.name())]);
            return refused(format!("{} ist für {} gebaut; dieser Prozessor kann diese Programme nicht ausführen", src.name, level.name()), notes);
        }
        let (list, problems) = mirrors(src, &mirror_dir(&self.root));
        for p in problems {
            event("pkg.mirror.list", &[("repo", &src.name), ("problem", &p)]);
            notes.push(p);
        }
        if list.is_empty() {
            return refused("keine Server eingetragen oder keine Spiegelliste da".into(), notes);
        }
        let wd = match self.fetcher.workdir(sync) {
            Ok(w) => w,
            Err(e) => return refused(e, notes),
        };
        let mut known = read_fresh(sync, &src.name);
        let cur = load(sync, &src.name).ok();
        if let Some(i) = &cur {
            known.builddate = known.builddate.max(i.newest);
        }
        let mut reasons = Vec::new();
        for (url, lu) in &list {
            match self.try_mirror(sync, &wd, src, url, lu.as_deref(), &known, cur.as_ref(), &mut notes) {
                Ok(o) => return RepoReport { repo: src.name.clone(), outcome: o, notes },
                Err(why) => {
                    event("pkg.mirror.next", &[("repo", &src.name), ("mirror", url), ("reason", &why)]);
                    notes.push(format!("{url}: {why}"));
                    reasons.push(why);
                }
            }
        }
        let why = if list.len() == 1 { reasons.pop().unwrap_or_default() } else { format!("kein Spiegel gab eine gute Datenbank ({} versucht; zuletzt: {})", list.len(), reasons.pop().unwrap_or_default()) };
        refused(why, notes)
    }

    #[allow(clippy::too_many_arguments)]
    fn try_mirror(
        &self,
        sync: &Path,
        wd: &super::download::WorkDir,
        src: &Source,
        url: &str,
        lu: Option<&str>,
        known: &Fresh,
        cur: Option<&index::Index>,
        notes: &mut Vec<String>,
    ) -> Result<Outcome, String> {
        let base = url.trim_end_matches('/');
        let dbname = format!("{}.db", src.name);
        // Every download first (database, .sig, lastupdate): no fetch runs once a file was checked, so the
        // download user never touches what root verified (pacman 7's DownloadUser model, dload.c:917).
        let db = match self.fetcher.get(wd, &format!("{base}/{dbname}"), &dbname, DB_MAX) {
            Got::File(p) => p,
            Got::Missing(m) => return Err(format!("Datenbank fehlt ({m})")),
            Got::Failed(m) => return Err(format!("Download: {m}")),
        };
        let sig = match self.fetcher.get(wd, &format!("{base}/{dbname}.sig"), &format!("{dbname}.sig"), SIG_MAX) {
            Got::File(p) => Some(read_small(&p, SIG_MAX)?),
            Got::Missing(m) if src.sig_db == Sig::Required => return Err(format!("Signatur der Datenbank fehlt ({m}), sie ist Pflicht")),
            Got::Missing(_) => None,
            Got::Failed(m) => return Err(format!("Signatur: {m}")),
        };
        let lastupdate: Option<i64> = lu.and_then(|lu| match self.fetcher.get(wd, lu, "lastupdate", 64) {
            Got::File(p) => read_small(&p, 64).ok().and_then(|b| String::from_utf8_lossy(&b).trim().parse().ok()),
            _ => None,
        });
        // Out of the download user's folder into a copy root made in root's own folder: a process of the
        // download user that is still alive (or holds the file open) cannot change what is checked below.
        let part = sync.join(format!("{dbname}.part"));
        let _ = fs::remove_file(&part);
        let copied = fsx::copy_hashed(&db, &part, 0o644);
        let _ = fs::remove_file(&db);
        let (sha, _) = copied?;
        let r = self.check_and_install(sync, src, url, &part, &sha, sig.as_deref(), lastupdate, known, cur, notes);
        let _ = fs::remove_file(&part);
        r
    }

    #[allow(clippy::too_many_arguments)]
    fn check_and_install(
        &self,
        sync: &Path,
        src: &Source,
        url: &str,
        part: &Path,
        sha: &str,
        sig: Option<&[u8]>,
        lastupdate: Option<i64>,
        known: &Fresh,
        cur: Option<&index::Index>,
        notes: &mut Vec<String>,
    ) -> Result<Outcome, String> {
        let dbname = format!("{}.db", src.name);
        let mut signer = String::new();
        match sig {
            Some(s) => match self.verifier.verify(&src.keyring, part, s) {
                Ok(who) => {
                    notes.push(format!("{url}: Signatur gut ({who})"));
                    signer = who;
                }
                Err(why) => {
                    event("pkg.db.bad", &[("repo", &src.name), ("mirror", url), ("reason", &why)]);
                    return Err(format!("Signatur der Datenbank abgelehnt: {why}"));
                }
            },
            // Stricter than pacman (DatabaseOptional takes it): once a signed database was accepted for this
            // repository, a mirror that strips the .sig could replay an older, once valid one. `reset` forgets it.
            None if !known.signer.is_empty() => {
                event("pkg.db.unsigned_after_signed", &[("repo", &src.name), ("mirror", url), ("signer", &known.signer)]);
                return Err(format!("Datenbank ohne Signatur, die zuletzt angenommene war signiert ({}): abgelehnt", known.signer));
            }
            None => notes.push(format!("{url}: Datenbank ohne Signatur (für {} erlaubt)", src.name)),
        }
        if let Some(n) = lastupdate
            && n < known.lastupdate
        {
            event("pkg.db.older", &[("repo", &src.name), ("mirror", url), ("lastupdate", &n.to_string()), ("known", &known.lastupdate.to_string())]);
            return Err(format!("Spiegel ist älter als der zuletzt benutzte (lastupdate {n} < {})", known.lastupdate));
        }
        let unchanged = cur.filter(|i| i.db_sha256 == sha);
        let (pkgs, newest, parsed) = match unchanged {
            // The very database accepted before: no replay question, nothing to parse again.
            Some(i) => (i.pkgs.len(), i.newest, None),
            None => {
                let parsed = match File::open(part).map_err(|e| fsx::err(part, e)).and_then(|f| syncdb::parse(f, &src.name)) {
                    Ok(p) => p,
                    Err(e) => {
                        event("pkg.db.bad", &[("repo", &src.name), ("mirror", url), ("reason", &e)]);
                        return Err(e);
                    }
                };
                for w in parsed.warnings.iter().take(20) {
                    notes.push(format!("{url}: {w}"));
                }
                let newest = parsed.newest();
                if newest < known.builddate {
                    // A mirror that synced later than the last one used may have dropped the newest package (a
                    // revert): taken when the drop is small. Otherwise a replay or a stale mirror: refused.
                    let later = lastupdate.is_some_and(|n| n > known.lastupdate);
                    let drop = known.builddate - newest;
                    if later && drop <= NEWEST_DROP_MAX {
                        event("pkg.db.newest_dropped", &[("repo", &src.name), ("mirror", url), ("newest", &newest.to_string()), ("known", &known.builddate.to_string())]);
                        notes.push(format!("{url}: das neueste Paket von vorher fehlt (Spiegel jünger, {} Stunden Abstand): angenommen", drop / 3600));
                    } else {
                        event("pkg.db.older", &[("repo", &src.name), ("mirror", url), ("newest", &newest.to_string()), ("known", &known.builddate.to_string())]);
                        return Err(format!(
                            "Datenbank älter als die zuletzt angenommene (neuestes Paket {newest} < {}): abgelehnt (bitspkg refresh --reset {} setzt die Quelle zurück)",
                            known.builddate, src.name
                        ));
                    }
                }
                (parsed.pkgs.len(), newest, Some(parsed))
            }
        };
        let fresh = Fresh {
            builddate: newest.max(known.builddate),
            lastupdate: lastupdate.unwrap_or(0).max(known.lastupdate),
            mirror: url.to_string(),
            accepted: self.now,
            db_sha256: sha.to_string(),
            signer,
        };
        // Into place: the database (`<repo>.db.part` → `<repo>.db`), then its .sig, the index, the freshness
        // naming the database's hash (a cut between them shows as a `.fresh` for another database: refresh
        // again). Once the database is in place nothing below is a refusal: the mirror loop must not go on
        // against a database it already replaced; a failure is a note, and the next load rebuilds the index.
        if parsed.is_some() {
            fsx::rename(part, &sync.join(&dbname))?;
        }
        let repo = src.name.as_str();
        let incomplete = |notes: &mut Vec<String>, what: &str, e: String| {
            event("pkg.refresh.incomplete", &[("repo", repo), ("step", what), ("reason", &e)]);
            notes.push(format!("{what}: {e} (die nächste Aktualisierung holt es nach)"));
        };
        let sig_dst = sync.join(format!("{dbname}.sig"));
        let r = match sig {
            Some(s) => fsx::write_atomic(&sig_dst, s, 0o644),
            None if fs::symlink_metadata(&sig_dst).is_ok() => fs::remove_file(&sig_dst).map_err(|e| fsx::err(&sig_dst, e)),
            None => Ok(()),
        };
        if let Err(e) = r {
            incomplete(notes, "Signatur", e);
        }
        if let Some(p) = &parsed
            && let Err(e) = index::write(&sync.join(format!("{}.idx", src.name)), p, sha)
        {
            event("pkg.idx.unsaved", &[("repo", &src.name), ("reason", &e)]);
            notes.push(format!("Index: {e} (wird beim nächsten Lesen neu gebaut)"));
        }
        if let Err(e) = write_fresh(sync, &src.name, &fresh) {
            incomplete(notes, "Frische", e);
        }
        Ok(match parsed {
            Some(_) => Outcome::Updated { pkgs, newest, mirror: url.to_string() },
            None => Outcome::Unchanged { pkgs, newest, mirror: url.to_string() },
        })
    }
}

/// `bitspkg refresh --reset REPO` ("Quelle zurücksetzen"): forgets REPO's database, signature, index and
/// freshness, so the next refresh takes any good database again (after a mirror legitimately went back).
pub fn reset(root: &Path, repo: &str) -> Result<(), String> {
    sources::check_repo_name(repo)?;
    let sync = ensure_sync(root)?;
    let _l = lock(&sync)?;
    for ext in ["db", "db.sig", "idx", "fresh"] {
        let p = sync.join(format!("{repo}.{ext}"));
        if fs::symlink_metadata(&p).is_ok() {
            fs::remove_file(&p).map_err(|e| fsx::err(&p, e))?;
        }
    }
    fsx::sync_dir(&sync)?;
    event("pkg.source.reset", &[("repo", repo)]);
    Ok(())
}

/// The index of `repo`, rebuilt from its database when missing or made for another database (a cut, or a
/// database changed by hand); when the sync folder is not ours to write (a user's search), the rebuilt index is
/// used without being saved.
pub fn load(sync: &Path, repo: &str) -> Result<index::Index, String> {
    let dbp = sync.join(format!("{repo}.db"));
    if !dbp.is_file() {
        return Err(format!("{repo}: noch nicht geladen (bitspkg refresh)"));
    }
    let (sha, _) = fsx::hash_file(&dbp)?;
    let ip = sync.join(format!("{repo}.idx"));
    if let Ok(t) = fs::read_to_string(&ip)
        && let Ok(i) = index::parse(&t)
        && i.db_sha256 == sha
    {
        return Ok(i);
    }
    event("pkg.idx.rebuilt", &[("repo", repo)]);
    let db = syncdb::parse(File::open(&dbp).map_err(|e| fsx::err(&dbp, e))?, repo)?;
    let text = index::render(&db, &sha);
    if let Err(e) = fsx::write_atomic(&ip, text.as_bytes(), 0o644) {
        event("pkg.idx.unsaved", &[("repo", repo), ("reason", &e)]);
    }
    index::parse(&text)
}

pub fn age_days(newest: i64, now: i64) -> i64 {
    (now - newest).max(0) / 86400
}

pub fn render(rs: &[RepoReport], now: i64) -> String {
    let mut o = String::new();
    for r in rs {
        match &r.outcome {
            Outcome::Updated { pkgs, newest, mirror } | Outcome::Unchanged { pkgs, newest, mirror } => {
                let how = if matches!(r.outcome, Outcome::Updated { .. }) { "aktualisiert" } else { "unverändert" };
                let days = age_days(*newest, now);
                let stale = if days > STALE_DAYS { format!(", veraltet ({days} Tage)") } else { String::new() };
                o.push_str(&format!("{}: {how}, {pkgs} Pakete, von {mirror}{stale}\n", r.repo));
            }
            Outcome::Refused(why) => o.push_str(&format!("{}: abgelehnt: {why}\n", r.repo)),
        }
        for n in &r.notes {
            o.push_str(&format!("  {n}\n"));
        }
    }
    o
}
