//! `bitspkg install NAME` (PK-6d): a program from the repositories becomes a BitsOS package (plan 1.6, 1.7, 1.9
//! "Space"). The order is what makes a cut harmless (PK-1's rule): resolve (PK-6c), the space check (D4), the
//! downloads and their checks happen before the store lock and touch only `/data/bitspkg/cache`; then, under the
//! lock, the packages go into `/apps/.pkgs` (each entry whole or not at all), the program's root tree is built in
//! a staging folder, the hooks are emulated there, and only then does `current` move. A cut at any of the named
//! points (`after download`, `after verify`, `after unpack`, `after triggers`, `before switch`, `after switch`)
//! leaves the previous state running; the next run takes what is complete (cache, `.pkgs` entries) and finishes.
//! Every choice and refusal goes into the install report (German, one fact per line).

use super::download::Fetcher;
use super::fetchpkg::{self, Have, Job};
use super::meta;
use super::pkgs::{self, Dropped, Linker, List, OriginFacts};
use super::refresh::{self, event};
use super::resolve::{Closure, Universe, policy};
use super::search;
use super::sources::{self, Config};
use super::syncdb::Pkg;
use super::tree::{self, Part};
use super::trigger::Outcome;
use crate::cut::step;
use crate::fsx;
use crate::manifest::Origin;
use crate::names::{folder_of, pacman_of, program_of, program_version};
use crate::space::human;
use crate::store::Store;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

pub const GB: u64 = 1 << 30;

#[derive(Debug, Default)]
pub struct Report {
    pub lines: Vec<String>,
    pub program: String,
    pub version: String,
    /// The same closure was installed already: it is current now.
    pub already: bool,
    pub refused: Option<String>,
}

impl Report {
    fn line(&mut self, s: impl Into<String>) {
        let s = s.into();
        crate::progress::line(&s);
        self.lines.push(s);
    }
    pub fn text(&self) -> String {
        let mut o: String = self.lines.iter().map(|l| format!("{l}\n")).collect();
        if let Some(r) = &self.refused {
            o.push_str(&format!("abgelehnt: {r}\n"));
        }
        o
    }
}

/// (total, available) bytes of the file system holding `p`: statfs's f_blocks and f_bavail in its unit
/// (f_frsize, else f_bsize), what `df -P` shows.
pub fn free_space(p: &Path) -> Result<(u64, u64), String> {
    let path = fsx::s(p)?;
    match usys::statfs_blocks(path) {
        Ok((unit, blocks, _, avail)) => Ok((blocks.saturating_mul(unit), avail.saturating_mul(unit))),
        Err(e) => Err(format!("freier Platz von {path} nicht messbar (statfs: Fehler {e})")),
    }
}

/// Bytes of the content store (every program file is one of its files; a hard-linked inode once).
/// D4's verdict (diskspace.c:433 _alpm_check_diskspace's role): `unpacked` new bytes and `download` must fit in
/// `avail`, and the programs (`used`) may grow to `limit` (`how` names where it comes from). bitspkgd's plan asks
/// the same before anything runs.
pub fn space_fits(unpacked: u64, download: u64, avail: u64, used: u64, limit: u64, how: &str) -> Result<(), String> {
    let needed = unpacked.saturating_add(download);
    if needed > avail {
        return Err(format!("zu wenig Platz auf /data: gebraucht {} ({} entpackt + {} Download), frei {}; nichts wurde heruntergeladen", human(needed), human(unpacked), human(download), human(avail)));
    }
    if used.saturating_add(unpacked) > limit {
        return Err(format!(
            "Programme dürfen höchstens {} belegen ({how}); belegt sind {}, dazu kämen {}; nichts wurde heruntergeladen",
            human(limit),
            human(used),
            human(unpacked)
        ));
    }
    Ok(())
}

/// The bytes the store's content files take (what D4's limit counts).
pub fn store_bytes(st: &Store) -> u64 {
    fs::read_dir(st.content()).map(|rd| rd.flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()).unwrap_or(0)
}

/// The `.pkgs` folder for `pkg`: its version's folder name, or with the package's hash appended when another
/// build of the same version (another repository) holds that name. (folder, already unpacked)
fn folder_for(st: &Store, pkg: &Pkg) -> Result<(String, bool), String> {
    let base = folder_of(&pkg.version)?;
    for cand in [base.clone(), format!("{base}~{}", &pkg.sha256[..8])] {
        let e = st.pkgs_entry(&pkg.name, &cand);
        if !e.is_dir() {
            return Ok((cand, false));
        }
        if pkgs::read_origin_sha(st, &pkg.name, &cand).as_deref() == Some(pkg.sha256.as_str()) && pkgs::read_list(st, &pkg.name, &cand).is_ok() {
            return Ok((cand, true));
        }
    }
    Err(format!("{}: zwei andere Fassungen von {} liegen schon in .pkgs", pkg.name, pkg.version))
}

struct Item {
    repo: String,
    pkg: Pkg,
    folder: String,
    present: bool,
    top: bool,
}

/// An installed version of `prog` whose closure is exactly `uses`, and the largest closure count of `base`.
fn same_closure(st: &Store, prog: &str, base: &str, uses: &[(String, String)]) -> (Option<String>, u32) {
    let (mut same, mut max) = (None, 0);
    for v in st.versions(prog).unwrap_or_default() {
        let Some(n) = v.strip_prefix(&format!("{base}+")).and_then(|n| n.parse::<u32>().ok()) else { continue };
        max = max.max(n);
        if let Ok(m) = st.manifest(prog, &v)
            && m.uses.iter().map(|u| (u.pkg.clone(), u.version.clone())).collect::<Vec<_>>() == uses
        {
            same = Some(v);
        }
    }
    (same, max)
}

pub fn install(st: &Store, cfg: &Config, what: &str, fetcher: &Fetcher) -> Report {
    let mut rep = Report::default();
    if let Err(e) = run(st, cfg, what, fetcher, &mut rep) {
        event("pkg.install.refused", &[("target", what), ("reason", &e)]);
        rep.refused = Some(e);
    }
    rep
}

fn run(st: &Store, cfg: &Config, what: &str, fetcher: &Fetcher, rep: &mut Report) -> Result<(), String> {
    // ---- resolve (PK-6c) ----
    let (idx, missing) = search::indexes(&st.root, cfg);
    for m in missing {
        rep.line(format!("Hinweis: {m}"));
    }
    if idx.is_empty() {
        return Err("keine Quelle geladen: zuerst bitspkg refresh".into());
    }
    let u = Universe::new(&idx, cfg.level);
    let guess = what.rsplit('/').next().unwrap_or(what);
    let previous: Vec<String> = match program_of(guess).ok().and_then(|g| st.current(&g).map(|v| (g, v))) {
        Some((g, v)) => st.manifest(&g, &v).map(|m| m.origins.into_iter().map(|o| o.name).collect()).unwrap_or_default(),
        None => Vec::new(),
    };
    let plan = Closure::resolve(what, &u, &policy::Prefs::builtin(), &previous);
    for l in plan.render().lines() {
        rep.line(l);
    }
    if !plan.ok() {
        return Err(format!("{what} kann nicht installiert werden (Gründe oben)"));
    }
    let top = plan.packages.iter().find(|p| p.why == "Programm").ok_or("kein Programm im Plan")?;
    let prog = program_of(&top.name)?;
    if prog != top.name {
        rep.line(format!("Programmname in BitsOS: {prog} (pacman: {})", top.name));
    }
    // A mapped name can meet another package's (`a.b` and `a_b`): a folder from the repositories whose closure
    // does not hold this pacman name belongs to another program.
    if let Some(v) = st.current(&prog)
        && let Ok(m) = st.manifest(&prog, &v)
        && !m.origins.is_empty()
        && !m.origins.iter().any(|o| o.name == top.name)
    {
        return Err(format!("{prog}: der Ordner gehört schon zu einem anderen Programm ({}), nicht zu {}", m.origins.first().map(|o| o.name.as_str()).unwrap_or("?"), top.name));
    }
    rep.program = prog.clone();
    let mut items = Vec::new();
    for p in &plan.packages {
        let pkg = idx.iter().find(|(r, _)| *r == p.repo).and_then(|(_, i)| i.pkgs.get(&p.name)).cloned().ok_or_else(|| format!("{}/{} fehlt im Index", p.repo, p.name))?;
        crate::names::check_sha256(&pkg.sha256).map_err(|e| format!("{}: %SHA256SUM% {e}", pkg.name))?;
        if pkg.filename.is_empty() || pkg.filename.contains('/') || pkg.filename.starts_with('.') {
            return Err(format!("{}: %FILENAME% {:?} ist kein Dateiname", pkg.name, pkg.filename));
        }
        let (folder, present) = folder_for(st, &pkg)?;
        items.push(Item { repo: p.repo.clone(), top: p.name == top.name, pkg, folder, present });
    }
    let uses: Vec<(String, String)> = items.iter().map(|i| (i.pkg.name.clone(), i.folder.clone())).collect();
    let base = folder_of(&top.version)?;
    let cache = fetchpkg::cache_dir(&st.root);
    let mut cu = fetchpkg::CacheUse::take(&cache)?;
    if let (Some(v), _) = same_closure(st, &prog, &base, &uses)
        && st.current(&prog).as_deref() == Some(v.as_str())
    {
        rep.version = v.clone();
        rep.already = true;
        rep.line(format!("{prog} {v} ist mit genau diesen Paketen schon installiert und aktuell"));
        // A run cut after its switch did not register the version with the services (settings, rules, actions):
        // done again here (the hooks are idempotent), under the lock as every registration.
        let lock = st.lock()?;
        let (_, failed) = crate::install::switch_locked(st, &lock, &prog, &v)?;
        hooks_failed(rep, failed);
        // That run also left its downloads: they go now (plan 1.9).
        cu.done(file_names(&items));
        return Ok(());
    }

    // ---- space (D4): before any download ----
    let need: Vec<&Item> = items.iter().filter(|i| !i.present).collect();
    let unpacked: u64 = need.iter().map(|i| i.pkg.isize).fold(0, u64::saturating_add);
    let download: u64 = need
        .iter()
        .filter(|i| !fs::symlink_metadata(cache.join(&i.pkg.filename)).is_ok_and(|m| m.len() == i.pkg.csize))
        .map(|i| i.pkg.csize)
        .fold(0, u64::saturating_add);
    // The volume `/apps` lives on (PK-0: /data); before the first install `/apps` may not exist yet. Nothing is
    // made here: the store lock (and init's word on /apps, PK-0) comes after the downloads.
    let on = st.apps.ancestors().find(|a| a.is_dir()).unwrap_or(&st.root).to_path_buf();
    let (total, avail) = free_space(&on)?;
    let limit = cfg.space_max_gb.map(|g| g.saturating_mul(GB)).unwrap_or(total / 2);
    let used = store_bytes(st);
    let how = if cfg.space_max_gb.is_some() { "pkg.space.max_gb" } else { "die Hälfte von /data" };
    rep.line(format!(
        "Platz: entpackt {} neu, Download {}; frei {} von {}; Programme dürfen {} belegen ({how}), belegt {}",
        human(unpacked),
        human(download),
        human(avail),
        human(total),
        human(limit),
        human(used)
    ));
    space_fits(unpacked, download, avail, used, limit, how)?;

    // ---- download and verify (no store lock: a second bitspkg is never kept waiting by the network) ----
    let net = Net { st, cfg, fetcher, cache: &cache };
    let first: Vec<usize> = (0..items.len()).filter(|&n| !items[n].present).collect();
    let mut got = fetch_verify(&net, &items, &first, rep, true)?;

    // ---- under the store lock: .pkgs entries, the tree, the switch ----
    // `.pkgs` entries are made and removed only under this lock, so what was decided before it may have changed
    // meanwhile (another install made an entry this run also loaded, a gc removed one this run meant to reuse):
    // decided again here. An entry that went and whose package this run has not loaded is loaded now, once,
    // without the lock; if it goes again, the install stops with "bitte erneut versuchen".
    let mut retried = false;
    let lock = loop {
        let lock = st.lock()?;
        let mut lost = Vec::new();
        for (n, it) in items.iter_mut().enumerate() {
            let (folder, present) = folder_for(st, &it.pkg)?;
            if present != it.present {
                event("pkg.install.changed", &[("pkg", &it.pkg.name), ("folder", &folder), ("present", if present { "yes" } else { "no" })]);
            }
            (it.folder, it.present) = (folder, present);
            if !present && !got.contains_key(&n) {
                lost.push(n);
            }
        }
        if lost.is_empty() {
            break lock;
        }
        let names = lost.iter().map(|&n| items[n].pkg.name.as_str()).collect::<Vec<_>>().join(", ");
        if retried {
            return Err(format!("{names}: während der Installation von einem anderen bitspkg aus /apps/.pkgs entfernt; bitte erneut versuchen"));
        }
        retried = true;
        drop(lock);
        rep.line(format!("Hinweis: {names} inzwischen aus /apps/.pkgs entfernt (ein anderes bitspkg): wird geladen"));
        got.extend(fetch_verify(&net, &items, &lost, rep, false)?);
    };
    let uses: Vec<(String, String)> = items.iter().map(|i| (i.pkg.name.clone(), i.folder.clone())).collect();
    let (same, max) = same_closure(st, &prog, &base, &uses);
    if let Some(v) = same {
        let (old, failed) = crate::install::switch_locked(st, &lock, &prog, &v)?;
        rep.version = v.clone();
        rep.already = true;
        rep.line(format!("{prog} {v} war mit genau diesen Paketen schon installiert: jetzt aktuell (vorher {})", old.as_deref().unwrap_or("-")));
        hooks_failed(rep, failed);
        cu.done(file_names(&items));
        return Ok(());
    }
    let version = program_version(&top.version, max + 1)?;
    rep.version = version.clone();
    let mut lk = Linker::new(st);
    let mut parts = Vec::new();
    let mut dropped = Dropped::default();
    let mut scripts: Vec<(String, meta::Scriptlet)> = Vec::new();
    let mut fresh = 0;
    let r = (|| {
        for (n, it) in items.iter().enumerate() {
            let entry = st.pkgs_entry(&it.pkg.name, &it.folder);
            let (list, key, scr) = if it.present {
                let s = fs::read(entry.join(pkgs::META_DIR).join("INSTALL")).ok().map(|b| meta::scriptlet(&String::from_utf8_lossy(&b)));
                let key = fs::read_to_string(entry.join(pkgs::META_DIR).join("origin"))
                    .ok()
                    .and_then(|t| t.lines().find_map(|l| l.strip_prefix("key ").map(str::to_string)))
                    .unwrap_or_default();
                (pkgs::read_list(st, &it.pkg.name, &it.folder)?, key, s)
            } else {
                let (h, key) = got.get(&n).map(|(h, k)| (h, k.clone())).ok_or_else(|| format!("{}: nicht geladen", it.pkg.name))?;
                fsx::mkdir(&st.pkgs().join(&it.pkg.name), 0o755)?;
                let stage = st.pkgs().join(&it.pkg.name).join(format!(".new-{}", it.folder));
                fsx::remove_tree(&stage)?;
                fsx::mkdir(&stage, 0o755)?;
                let u = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pkgs::unpack(&mut lk, &h.file, &stage, it.pkg.isize)))
                    .unwrap_or_else(|_| Err("der Entpacker brach ab (ein Fehler in ihm, nicht im Paket)".into()));
                let u = match u {
                    Ok(u) => u,
                    Err(e) => {
                        let _ = fsx::remove_tree(&stage);
                        fetchpkg::drop_cached(h);
                        return Err(format!("{}: {e}", it.pkg.filename));
                    }
                };
                if let Err(e) = same_package(&u.info, &it.pkg).and_then(|_| policy_check(&u.list, it.top, &stage, &it.pkg.name)) {
                    let _ = fsx::remove_tree(&stage);
                    return Err(e);
                }
                let facts = OriginFacts { repo: &it.repo, filename: &it.pkg.filename, sha256: &it.pkg.sha256, key: &key };
                pkgs::finish(&stage, &u, &facts)?;
                step("unpack package");
                fsx::rename(&stage, &entry)?;
                event("pkg.unpack.done", &[("pkg", &it.pkg.name), ("version", &it.folder), ("files", &u.list.files.len().to_string())]);
                fresh += 1;
                dropped.merge(&u.dropped);
                (u.list, key, u.scriptlet)
            };
            if it.present {
                policy_check(&list, it.top, &entry, &it.pkg.name)?;
            }
            if let Some(s) = scr {
                scripts.push((it.pkg.name.clone(), s));
            }
            let origin = Origin {
                repo: it.repo.clone(),
                name: it.pkg.name.clone(),
                version: it.pkg.version.clone(),
                filename: it.pkg.filename.clone(),
                sha256: it.pkg.sha256.clone(),
                packager: it.pkg.packager.clone(),
                key,
            };
            parts.push(Part { pkg: it.pkg.name.clone(), folder: it.folder.clone(), list, origin });
        }
        Ok(())
    })();
    if let Err(e) = r {
        let _ = st.gc_content();
        return Err(e);
    }
    step("after unpack");
    rep.line(format!("Pakete: {} ({fresh} neu entpackt, {} schon in /apps/.pkgs)", parts.len(), parts.len() - fresh));
    let top_pkg = &items.iter().find(|i| i.top).ok_or("kein Programm")?.pkg;
    let built = match tree::build(st, &mut lk, &prog, &version, &parts, "", &top_pkg.desc) {
        Ok(b) => b,
        Err(e) => {
            let _ = st.gc_content();
            if st.versions(&prog).map(|v| v.is_empty()).unwrap_or(false) && st.current(&prog).is_none() {
                let _ = fsx::remove_tree(&st.pkg(&prog));
            }
            return Err(e);
        }
    };
    let (id, previous, failed) = crate::install::commit(st, &built.manifest)?;
    let m = &built.manifest;
    rep.line(format!(
        "Installiert: {prog} {version} (pacman {}; Programm-ID {id}; vorher {}); {} Dateien, {} Verweise, {} geteilt, {} kopiert",
        pacman_of(&version).0,
        previous.as_deref().unwrap_or("-"),
        m.files.len(),
        m.symlinks.len(),
        lk.shared,
        lk.copied
    ));
    if let Some(p) = &m.program {
        rep.line(format!("Programm: {p} (bitsrun {prog})"));
    }
    for l in &m.launch {
        rep.line(format!("Starter: {} ({}; {})", l.name, l.exec, l.desktop));
    }
    for l in dropped.lines() {
        rep.line(format!("Beim Entpacken weggelassen: {l}"));
    }
    for (p, s) in &scripts {
        let pats = if s.patterns.is_empty() { String::new() } else { format!("; {}", s.patterns.join(", ")) };
        rep.line(format!("Installationsskript von {p} (nie ausgeführt, bitspkg info --scripts {prog}): {}{pats}", s.functions.join(", ")));
        if s.users {
            rep.line(format!("  {p} legt Systembenutzer oder -gruppen an: trotzdem installiert, die Dateien laufen als uid des Programms"));
        }
    }
    for t in &built.triggers {
        let line = match &t.outcome {
            Outcome::Emulated(o) => format!("emuliert ({}): {}", t.tool, outputs(o.iter().map(|(r, _)| r.as_str()))),
            Outcome::Links(l) => format!("emuliert: {} Verweise in root/etc/fonts/conf.d", l.len()),
            Outcome::Later(w) => format!("später: {w}"),
            Outcome::Refused(r) => format!("abgelehnt: {r}"),
            Outcome::Failed(r) => format!("fehlgeschlagen: {r}"),
            Outcome::NotRun(r) => r.clone(),
        };
        rep.line(format!("Hook {}: {line}", t.hook));
    }
    for p in &m.permissions {
        rep.line(format!("Recht gewünscht (fragen, PM-3): {}", p.describe()));
    }
    hooks_failed(rep, failed);
    cu.done(file_names(&items));
    event("pkg.install.done", &[("name", &prog), ("version", &version), ("packages", &parts.len().to_string()), ("unpacked", &fresh.to_string())]);
    Ok(())
}

/// A hook's output files for one report line: up to three by name, more as a count per folder.
fn outputs<'a>(rels: impl Iterator<Item = &'a str>) -> String {
    let v: Vec<&str> = rels.collect();
    if v.len() <= 3 {
        return v.iter().map(|r| format!("root/{r}")).collect::<Vec<_>>().join(", ");
    }
    let mut by: Vec<(String, usize)> = Vec::new();
    for r in &v {
        let top = r.split('/').take(4).collect::<Vec<_>>().join("/");
        match by.iter_mut().find(|(d, _)| *d == top) {
            Some(e) => e.1 += 1,
            None => by.push((top, 1)),
        }
    }
    by.iter().map(|(d, n)| format!("{n} Dateien in root/{d}")).collect::<Vec<_>>().join(", ")
}

fn hooks_failed(rep: &mut Report, failed: usize) {
    if failed > 0 {
        rep.line(format!("{failed} Registrierung(en) fehlgeschlagen (bitspkg register wiederholt sie)"));
    }
}

/// Plan 1.9: downloads go once the program is installed (also those an earlier, cut run left): their names for
/// CacheUse, which removes them (and an empty cache folder, B2) when no other run is using the cache.
fn file_names(items: &[Item]) -> Vec<String> {
    items.iter().map(|i| i.pkg.filename.clone()).collect()
}

/// What the network side needs: download and verify run before the store lock.
struct Net<'a> {
    st: &'a Store,
    cfg: &'a Config,
    fetcher: &'a Fetcher,
    cache: &'a Path,
}

/// Downloads `which` of `items` (PARALLEL at a time, mirror fallback) and checks each signature (PK-6b); the
/// %SHA256SUM% is checked as each file comes in. The named cut points only on the run's first round.
fn fetch_verify(net: &Net, items: &[Item], which: &[usize], rep: &mut Report, steps: bool) -> Result<HashMap<usize, (Have, String)>, String> {
    let mirror = refresh::mirror_dir(&net.st.root);
    let mut jobs = Vec::new();
    for &n in which {
        let i = &items[n];
        let src = net.cfg.sources.iter().find(|s| s.name == i.repo).ok_or_else(|| format!("Quelle {} nicht eingestellt", i.repo))?;
        let (servers, problems) = sources::servers(src, &mirror);
        for p in problems {
            rep.line(format!("Hinweis: {}: {p}", src.name));
        }
        jobs.push(Job { repo: i.repo.clone(), pkg: i.pkg.clone(), servers });
    }
    let got = fetchpkg::fetch_all(net.fetcher, net.cache, &jobs);
    let mut haves: Vec<Have> = Vec::new();
    let mut fails = Vec::new();
    for (j, g) in jobs.iter().zip(got) {
        match g {
            Ok(h) => haves.push(h),
            Err(e) => fails.push(format!("{}: {e}", j.pkg.name)),
        }
    }
    let fetched: u64 = haves.iter().map(|h| h.bytes).sum();
    let from: HashSet<&str> = haves.iter().map(|h| h.from.as_str()).filter(|f| !f.is_empty()).collect();
    rep.line(format!(
        "Download: {} Pakete, {} geladen ({} gleichzeitig){}, {} schon im Zwischenspeicher",
        jobs.len(),
        human(fetched),
        fetchpkg::PARALLEL,
        if from.is_empty() { String::new() } else { format!(" von {}", from.into_iter().collect::<Vec<_>>().join(", ")) },
        haves.iter().filter(|h| h.from.is_empty()).count()
    ));
    for h in &haves {
        for f in &h.fallbacks {
            rep.line(format!("Ausweichen: {f}"));
        }
    }
    if !fails.is_empty() {
        return Err(format!("Download fehlgeschlagen: {}", fails.join("; ")));
    }
    if steps {
        step("after download");
    }
    let mut out = HashMap::new();
    if !jobs.is_empty() {
        let dir = crate::pgp::default_dir(&net.st.root)?;
        let kr = crate::pgp::Keyring::load(&dir)?;
        for ((j, h), &n) in jobs.iter().zip(haves).zip(which) {
            match fetchpkg::check_sig(&kr, &j.pkg, &h) {
                Ok(k) => {
                    out.insert(n, (h, k));
                }
                Err(e) => {
                    fetchpkg::drop_cached(&h);
                    return Err(format!("{}: {e}", j.pkg.filename));
                }
            }
        }
        rep.line(format!("Geprüft: sha256 und Signatur von {} Paketen (Schlüsselbund {})", jobs.len(), dir.display()));
    }
    if steps {
        step("after verify");
    }
    Ok(out)
}

/// The unpacked `.PKGINFO` names the package the database entry promised (the sha256 and the signature tie the
/// file to the database already; this catches a repository whose database and packages disagree).
fn same_package(info: &meta::PkgInfo, pkg: &Pkg) -> Result<(), String> {
    if info.name.is_empty() {
        return Err(format!("{}: kein .PKGINFO im Paket: abgelehnt", pkg.filename));
    }
    if info.name != pkg.name || info.version != pkg.version {
        return Err(format!("{}: das Paket nennt sich {} {}, die Datenbank {} {}: abgelehnt", pkg.filename, info.name, info.version, pkg.name, pkg.version));
    }
    if !pkg.arch.is_empty() && !info.arch.is_empty() && info.arch != pkg.arch {
        return Err(format!("{}: Architektur {} im Paket, {} in der Datenbank: abgelehnt", pkg.filename, info.arch, pkg.arch));
    }
    Ok(())
}

/// Plan 1.5 on the real file list: a system package refuses the program; the top level must be a program. Only
/// regular files of the list are read (a package's link, e.g. a `.desktop` pointing at a host file, never is).
fn policy_check(l: &List, top: bool, dir: &Path, name: &str) -> Result<(), String> {
    let paths: Vec<&str> = l.files.iter().map(|f| f.path.as_str()).chain(l.symlinks.iter().map(|s| s.path.as_str())).collect();
    if let Some(why) = policy::system_by_files(paths.iter().copied()) {
        return Err(format!("{name}: ein Systempaket ({why})"));
    }
    let regular: HashSet<&str> = l.files.iter().map(|f| f.path.as_str()).collect();
    let read = |p: &str| {
        let p = p.trim_start_matches("./");
        if regular.contains(p) { fs::read_to_string(dir.join(p)).ok() } else { None }
    };
    if top && !policy::is_program(paths.iter().copied(), read) {
        return Err(format!("{name}: Das ist kein Programm, nur ein Teil davon (kein Programm in usr/bin, kein Starter in usr/share/applications)"));
    }
    Ok(())
}

/// `bitspkg info --scripts NAME`: the `.INSTALL` texts of an installed program's closure (or of a package in
/// `.pkgs`), with what they would do; never run.
pub fn scripts(st: &Store, name: &str) -> Result<Vec<String>, String> {
    let mut entries: Vec<(String, String)> = Vec::new();
    if let Ok(prog) = program_of(name)
        && let Some(v) = st.current(&prog)
    {
        entries = st.manifest(&prog, &v)?.uses.into_iter().map(|u| (u.pkg, u.version)).collect();
    }
    if entries.is_empty() {
        entries = pkgs::entries(st)?.into_iter().filter(|(p, _)| p == name).collect();
    }
    if entries.is_empty() {
        return Err(format!("{name} ist weder installiert noch in /apps/.pkgs"));
    }
    let mut out = Vec::new();
    for (p, v) in entries {
        let f = st.pkgs_entry(&p, &v).join(pkgs::META_DIR).join("INSTALL");
        let Ok(b) = fs::read(&f) else { continue };
        let t = String::from_utf8_lossy(&b[..b.len().min(meta::META_MAX as usize)]).into_owned();
        let s = meta::scriptlet(&t);
        out.push(format!("== {p} {} ({}): nie ausgeführt", pacman_of(&v).0, f.display()));
        out.push(format!("Funktionen: {}", s.functions.join(", ")));
        for x in &s.patterns {
            out.push(format!("Muster: {x}"));
        }
        out.extend(t.lines().map(|l| format!("| {l}")));
    }
    if out.is_empty() {
        out.push(format!("{name}: kein Paket bringt ein Installationsskript (.INSTALL)"));
    }
    Ok(out)
}
