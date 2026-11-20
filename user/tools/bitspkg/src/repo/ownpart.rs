//! PK-6e (plan 1.4 "Keyring updates", 1.2 "Mirrors"): the keyring and mirror list packages bitspkg itself uses
//! (`archlinux-keyring`, `cachyos-keyring`, `pacman-mirrorlist`, `cachyos-*-mirrorlist`) are ordinary repository
//! packages, installed here as data packages: downloaded and checked like every package (sha256, signature) with
//! the keyring that is active NOW, then their few files go into a new, complete folder beside the active one
//! (`data/bitspkg/keyrings`, `data/bitspkg/mirrorlist`), which is checked again (a keyring must load and keep a
//! valid main key of its ring) and only then exchanged with the active folder in one step. A refused package
//! leaves the active folder as it was. pacman's way is `pacman-key --populate` from the keyring package's
//! install script (pacman/scripts/pacman-key.sh.in:283 populate_keyring: every `<ring>.gpg` with its `-trusted`
//! and `-revoked`); bitspkg has no gpg database, so the new folder IS the populated keyring (pgp::keyring::read_dir
//! reads exactly those three files per ring).

use super::download::Fetcher;
use super::fetchpkg::{self, Job};
use super::index::Index;
use super::meta;
use super::refresh::{self, MIRROR_DATA, event};
use super::search;
use super::sources::{self, Config};
use super::syncdb::Pkg;
use super::tar::{Kind, Tar};
use super::unpack;
use super::vercmp::vercmp;
use crate::fsx;
use crate::pgp::{self, Keyring};
use crate::store::Store;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// One file of a keyring or mirror list package (archlinux.gpg is about 1.5 MB today).
pub const FILE_MAX: u64 = 64 << 20;
/// The record of the installed parts inside each folder (so record and files are exchanged together).
pub const STATE: &str = ".bitspkg-parts";
/// The sha256 of each mirror list file as its package brought it (pacman's backup hash, add.c:308), so a list the
/// user changed is recognised on the next update.
pub const FILES: &str = ".bitspkg-files";

#[derive(Clone, Debug, PartialEq)]
pub enum What {
    /// `<ring>-keyring`: `usr/share/pacman/keyrings/<ring>.gpg`, `-trusted`, `-revoked`.
    Keyring(String),
    /// A mirror list package: `etc/pacman.d/<file>`.
    Mirrorlist(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Want {
    pub pkg: String,
    pub what: What,
}

impl Want {
    pub fn de(&self) -> &'static str {
        match self.what {
            What::Keyring(_) => "Schlüsselbund",
            What::Mirrorlist(_) => "Spiegelliste",
        }
    }
}

fn ring_name_ok(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && !n.starts_with(['-', '.']) && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// The parts to keep current: `<ring>-keyring` for every ring of the active keyring folder and every source's
/// `keyring`; for every `include:<list>` server the mirror list package (`mirrorlist` comes with
/// `pacman-mirrorlist`, CachyOS names its packages after their lists).
pub fn wanted(root: &Path, cfg: &Config) -> Vec<Want> {
    let mut rings: Vec<String> = Vec::new();
    if let Ok(d) = pgp::default_dir(root)
        && let Ok(rd) = fs::read_dir(&d)
    {
        rings.extend(rd.flatten().filter_map(|e| e.file_name().to_str()?.strip_suffix(".gpg").map(str::to_string)));
    }
    rings.extend(cfg.sources.iter().map(|s| s.keyring.clone()));
    rings.sort();
    rings.dedup();
    let mut out: Vec<Want> = rings.into_iter().filter(|r| ring_name_ok(r)).map(|r| Want { pkg: format!("{r}-keyring"), what: What::Keyring(r) }).collect();
    for s in &cfg.sources {
        for f in s.servers.iter().filter_map(|x| x.strip_prefix("include:")) {
            if !ring_name_ok(f) {
                continue;
            }
            let pkg = if f == "mirrorlist" { "pacman-mirrorlist".to_string() } else { f.to_string() };
            if !out.iter().any(|w| w.pkg == pkg) {
                out.push(Want { pkg, what: What::Mirrorlist(f.to_string()) });
            }
        }
    }
    out
}

/// The active folder a part lives in on /data.
pub fn data_dir(root: &Path, w: &What) -> PathBuf {
    match w {
        What::Keyring(_) => root.join(pgp::KEYRING_DIRS[0]),
        What::Mirrorlist(_) => root.join(MIRROR_DATA),
    }
}

/// Where the files come from today: the folder on /data, else the image's (keyring: pgp::default_dir).
fn active_dir(root: &Path, w: &What) -> Option<PathBuf> {
    match w {
        What::Keyring(_) => pgp::default_dir(root).ok(),
        What::Mirrorlist(_) => Some(refresh::mirror_dir(root)),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Installed {
    pub pkg: String,
    pub version: String,
    pub sha256: String,
    pub repo: String,
    pub key: String,
}

/// The record of the folder `dir` (one line per part: `pkg version sha256 repo key`); a damaged line is left out.
pub fn state(dir: &Path) -> Vec<Installed> {
    let t = fs::read(dir.join(STATE)).map(|b| String::from_utf8_lossy(&b[..b.len().min(1 << 20)]).into_owned()).unwrap_or_default();
    t.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(' ').collect();
            (f.len() == 5).then(|| Installed { pkg: f[0].into(), version: f[1].into(), sha256: f[2].into(), repo: f[3].into(), key: f[4].into() })
        })
        .collect()
}

fn render_state(v: &[Installed]) -> String {
    v.iter().map(|i| format!("{} {} {} {} {}\n", i.pkg, i.version, i.sha256, i.repo, if i.key.is_empty() { "-" } else { &i.key })).collect()
}

#[derive(Clone, Debug)]
pub struct Step {
    pub want: Want,
    pub repo: String,
    pub pkg: Pkg,
    /// The installed version; None: the image's copy (no version known), installed for the first time.
    pub installed: Option<String>,
}

/// What `update` does with the parts (pacman/lib/libalpm/sync.c:56 alpm_sync_get_new_version: the first source
/// that has the name, an update only when newer; :85 check_literal: an older one is "local is newer", kept).
pub fn plan(root: &Path, idx: &[(String, Index)], wants: &[Want]) -> (Vec<Step>, Vec<String>) {
    let (mut steps, mut lines) = (Vec::new(), Vec::new());
    for w in wants {
        let Some((repo, p)) = search::find(idx, &w.pkg) else {
            event("pkg.part.absent", &[("pkg", &w.pkg)]);
            continue;
        };
        let inst = state(&data_dir(root, &w.what)).into_iter().find(|i| i.pkg == w.pkg);
        match &inst {
            Some(i) if vercmp(&p.version, &i.version) == 0 => {}
            Some(i) if vercmp(&p.version, &i.version) < 0 => {
                lines.push(format!("{} {}: installiert {} ist neuer als {repo} ({}): bleibt", w.de(), w.pkg, i.version, p.version));
            }
            _ => steps.push(Step { want: w.clone(), repo: repo.to_string(), pkg: p.clone(), installed: inst.map(|i| i.version) }),
        }
    }
    (steps, lines)
}

pub fn describe(s: &Step) -> String {
    format!("{} {}: {} → {} ({})", s.want.de(), s.want.pkg, s.installed.as_deref().unwrap_or("Abbild"), s.pkg.version, s.repo)
}

/// Downloads, checks and activates one part. Ok: the report lines; Err: the refusal (the active folder stays).
pub fn install(st: &Store, cfg: &Config, s: &Step, fetcher: &Fetcher) -> Result<Vec<String>, String> {
    let r = install_inner(st, cfg, s, fetcher);
    match &r {
        Ok(_) => event("pkg.part.updated", &[("pkg", &s.want.pkg), ("version", &s.pkg.version)]),
        Err(e) => event("pkg.part.refused", &[("pkg", &s.want.pkg), ("version", &s.pkg.version), ("reason", e)]),
    }
    r
}

fn install_inner(st: &Store, cfg: &Config, s: &Step, fetcher: &Fetcher) -> Result<Vec<String>, String> {
    let p = &s.pkg;
    crate::names::check_sha256(&p.sha256).map_err(|e| format!("{}: %SHA256SUM% {e}", p.name))?;
    if p.filename.is_empty() || p.filename.contains('/') || p.filename.starts_with('.') {
        return Err(format!("{}: %FILENAME% {:?} ist kein Dateiname", p.name, p.filename));
    }
    let mut lines = Vec::new();
    let src = cfg.sources.iter().find(|x| x.name == s.repo).ok_or_else(|| format!("Quelle {} nicht eingestellt", s.repo))?;
    let (servers, problems) = sources::servers(src, &refresh::mirror_dir(&st.root));
    lines.extend(problems.into_iter().map(|x| format!("Hinweis: {}: {x}", src.name)));
    let cache = fetchpkg::cache_dir(&st.root);
    let mut cu = fetchpkg::CacheUse::take(&cache)?;
    let job = Job { repo: s.repo.clone(), pkg: p.clone(), servers };
    let have = fetchpkg::fetch_all(fetcher, &cache, std::slice::from_ref(&job)).pop().ok_or("kein Ergebnis")??;
    lines.push(format!("Download: {} ({} Bytes{})", p.filename, have.bytes, if have.from.is_empty() { ", schon im Zwischenspeicher".into() } else { format!(" von {}", have.from) }));
    // With the keyring active now: a new keyring never vouches for itself.
    let kdir = pgp::default_dir(&st.root)?;
    let kr = Keyring::load(&kdir)?;
    let key = match fetchpkg::check_sig(&kr, p, &have) {
        Ok(k) => k,
        Err(e) => {
            fetchpkg::drop_cached(&have);
            return Err(format!("{}: {e}", p.filename));
        }
    };
    lines.push(format!("Geprüft: sha256 und Signatur von {} (Schlüssel {key}, Schlüsselbund {})", p.filename, kdir.display()));
    let files = match read_files(&have.file, p, &s.want.what) {
        Ok(f) => f,
        Err(e) => {
            fetchpkg::drop_cached(&have);
            return Err(e);
        }
    };
    let old: Vec<String> = match &s.want.what {
        What::Keyring(ring) => kr.entries.iter().filter(|e| e.cert.ring == *ring).map(|e| e.cert.primary.fpr_hex()).collect(),
        What::Mirrorlist(_) => Vec::new(),
    };
    let lock = st.lock()?;
    let r = activate(st, s, &key, files, &old, &mut lines);
    drop(lock);
    cu.done(vec![p.filename.clone()]);
    r.map(|_| lines)
}

/// The part's files from the package (bounded, nothing written): the `.PKGINFO` must name the database's package.
fn read_files(file: &Path, p: &Pkg, w: &What) -> Result<Vec<(String, Vec<u8>)>, String> {
    let f = fs::File::open(file).map_err(|e| fsx::err(file, e))?;
    let bound = p.isize.saturating_add(p.isize / 8).saturating_add(64 << 20);
    let r = unpack::open(unpack::Bounded::new(f, bound, "Paket")).map_err(|e| format!("{}: {e}", p.filename))?;
    let mut tar = Tar::new(r);
    let (dir, names): (&str, Vec<String>) = match w {
        What::Keyring(ring) => ("usr/share/pacman/keyrings/", vec![format!("{ring}.gpg"), format!("{ring}-trusted"), format!("{ring}-revoked")]),
        What::Mirrorlist(f) => ("etc/pacman.d/", vec![f.clone()]),
    };
    let (mut out, mut info) = (Vec::new(), None);
    while let Some(e) = tar.next_entry().map_err(|e| format!("{}: {e}", p.filename))? {
        if e.path == meta::PKGINFO {
            info = Some(meta::pkginfo(&String::from_utf8_lossy(&read_data(&mut tar, meta::META_MAX, &e.path)?)));
            continue;
        }
        let Some(n) = e.path.strip_prefix(dir) else { continue };
        if e.kind != Kind::File || !names.iter().any(|x| x == n) {
            continue;
        }
        if out.iter().any(|(x, _): &(String, Vec<u8>)| x == n) {
            return Err(format!("{}: {} zweimal im Paket: abgelehnt", p.filename, e.path));
        }
        out.push((n.to_string(), read_data(&mut tar, FILE_MAX, &e.path)?));
    }
    let info = info.ok_or_else(|| format!("{}: kein .PKGINFO im Paket: abgelehnt", p.filename))?;
    if info.name != p.name || info.version != p.version {
        return Err(format!("{}: das Paket nennt sich {} {}, die Datenbank {} {}: abgelehnt", p.filename, info.name, info.version, p.name, p.version));
    }
    if !out.iter().any(|(n, _)| *n == names[0]) {
        return Err(format!("{}: kein {dir}{} im Paket: abgelehnt", p.filename, names[0]));
    }
    Ok(out)
}

fn read_data<R: Read>(tar: &mut Tar<R>, max: u64, what: &str) -> Result<Vec<u8>, String> {
    let mut v = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = tar.read_data(&mut buf)?;
        if n == 0 {
            return Ok(v);
        }
        if (v.len() + n) as u64 > max {
            return Err(format!("{what} größer als {max} Bytes: abgelehnt"));
        }
        v.extend_from_slice(&buf[..n]);
    }
}

/// The recorded package hashes of the mirror list files in `dir` (`<sha256> <name>` per line).
pub fn file_hashes(dir: &Path) -> Vec<(String, String)> {
    let t = fs::read(dir.join(FILES)).map(|b| String::from_utf8_lossy(&b[..b.len().min(1 << 20)]).into_owned()).unwrap_or_default();
    t.lines().filter_map(|l| l.split_once(' ').map(|(h, n)| (h.to_string(), n.to_string()))).collect()
}

fn sha256_hex(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    crate::names::hex(&Sha256::digest(b))
}

/// pacman/lib/libalpm/add.c:365-404 for a file in backup=: local = the file on disk, orig = the hash recorded when
/// its package was installed, pkg = the new one. Returns whether the new file goes in under its own name (else
/// beside it as `.pacnew`, or not at all when the package did not change it: `None`).
fn backup_rule(local: Option<&str>, orig: Option<&str>, pkg: &str) -> Option<bool> {
    match (local, orig) {
        (None, _) => Some(true),
        (Some(l), _) if l == pkg => Some(true),
        (_, Some(o)) if o == pkg => None,
        (Some(l), Some(o)) if l == o => Some(true),
        _ => Some(false),
    }
}

/// Builds `<data>.new` complete (today's files, the part's on top, the record), checks it, then exchanges it with
/// the active folder on /data in one step (renameat2 RENAME_EXCHANGE; the first time a plain rename). `old`: the
/// fingerprints of the ring's keys in the keyring active now (for the report of keys that go away).
fn activate(st: &Store, s: &Step, key: &str, files: Vec<(String, Vec<u8>)>, old: &[String], lines: &mut Vec<String>) -> Result<(), String> {
    let w = &s.want.what;
    let data = data_dir(&st.root, w);
    let new = data.with_extension("new");
    fsx::remove_tree(&new)?;
    fs::create_dir_all(data.parent().ok_or("kein Elternordner")?).map_err(|e| fsx::err(&data, e))?;
    fsx::mkdir(&new, 0o755)?;
    let r = (|| {
        let from = active_dir(&st.root, w);
        let mut copied = 0;
        if let Some(from) = &from
            && let Ok(rd) = fs::read_dir(from)
        {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                let md = fs::symlink_metadata(e.path()).map_err(|x| fsx::err(&e.path(), x))?;
                if n.starts_with('.') || !md.is_file() || md.len() > FILE_MAX {
                    continue;
                }
                let b = fs::read(e.path()).map_err(|x| fsx::err(&e.path(), x))?;
                fsx::write_atomic(&new.join(&n), &b, 0o644)?;
                copied += 1;
            }
        }
        let mut hashes = file_hashes(&data);
        for (n, b) in &files {
            let mut name = n.clone();
            if let What::Mirrorlist(_) = w {
                let pkg = sha256_hex(b);
                let local = fs::read(new.join(n)).ok().map(|x| sha256_hex(&x));
                let orig = hashes.iter().find(|(_, x)| x == n).map(|(h, _)| h.clone());
                hashes.retain(|(_, x)| x != n);
                hashes.push((pkg.clone(), n.clone()));
                match backup_rule(local.as_deref(), orig.as_deref(), &pkg) {
                    None => {
                        lines.push(format!("{n}: das Paket ändert sie nicht, die bisherige (auch von Hand geänderte) bleibt"));
                        continue;
                    }
                    Some(false) => {
                        name = format!("{n}.pacnew");
                        lines.push(format!("Hinweis: {n} wurde geändert (oder kam mit dem Abbild): sie bleibt, die neue liegt als {name} daneben (pacman: .pacnew)"));
                    }
                    Some(true) if local.is_some() && sources::mirrorlist(&String::from_utf8_lossy(b)).is_empty() => {
                        // Beyond pacman: a list without one active server never replaces a working one (Arch's
                        // pacman-mirrorlist ships every server commented out; bitspkg could not fetch anything).
                        name = format!("{n}.pacnew");
                        lines.push(format!("Hinweis: die neue {n} nennt keinen aktiven Server: die bisherige bleibt, die neue liegt als {name} daneben"));
                    }
                    Some(true) => {}
                }
            }
            fsx::write_atomic(&new.join(&name), b, 0o644)?;
        }
        if let What::Mirrorlist(_) = w {
            let t: String = hashes.iter().map(|(h, n)| format!("{h} {n}\n")).collect();
            fsx::write_atomic(&new.join(FILES), t.as_bytes(), 0o644)?;
        }
        let mut rec: Vec<Installed> = state(&data).into_iter().filter(|i| i.pkg != s.want.pkg).collect();
        rec.push(Installed { pkg: s.want.pkg.clone(), version: s.pkg.version.clone(), sha256: s.pkg.sha256.clone(), repo: s.repo.clone(), key: key.to_string() });
        fsx::write_atomic(&new.join(STATE), render_state(&rec).as_bytes(), 0o644)?;
        if let What::Keyring(ring) = w {
            // The new keyring must load and keep a valid main key of the ring the package brought.
            let kr = Keyring::load(&new).map_err(|e| format!("der neue Schlüsselbund ist unlesbar ({e}): abgelehnt, der bisherige bleibt"))?;
            if !kr.entries.iter().any(|e| e.cert.ring == *ring && e.main) {
                return Err(format!("der neue Schlüsselbund hat keinen gültigen Hauptschlüssel für {ring}: abgelehnt, der bisherige bleibt"));
            }
            let (main, valid) = kr.counts();
            lines.push(format!("Neuer Schlüsselbund geprüft: {} Schlüssel, {main} Hauptschlüssel, {valid} gültig", kr.entries.len()));
            let now: Vec<String> = kr.entries.iter().filter(|e| e.cert.ring == *ring).map(|e| e.cert.primary.fpr_hex()).collect();
            keys_gone(st, ring, old, &now, lines);
        }
        lines.push(format!("{} {} {}: {} Dateien übernommen, {} aus dem Paket", s.want.de(), s.want.pkg, s.pkg.version, copied, files.len()));
        fsx::sync_dir(&new)
    })();
    if let Err(e) = r {
        let _ = fsx::remove_tree(&new);
        return Err(e);
    }
    crate::cut::step("before part switch");
    if data.is_dir() {
        if let Err(e) = fsx::exchange(&new, &data) {
            // exchange also syncs the parent: if only that failed, the new folder is already the active one.
            if !state(&data).iter().any(|i| i.pkg == s.want.pkg && i.sha256 == s.pkg.sha256) {
                let _ = fsx::remove_tree(&new);
                return Err(e);
            }
            lines.push(format!("Hinweis: nach dem Tausch nicht gesichert ({e}); der neue Ordner ist aktiv"));
        }
        crate::cut::step("after part switch");
        if let Err(e) = fsx::remove_tree(&new) {
            // The new folder is active: what is left is the old one under `.new`, removed by the next run.
            event("pkg.part.leftover", &[("pkg", &s.want.pkg), ("path", &new.to_string_lossy()), ("reason", &e)]);
            lines.push(format!("Hinweis: der alte Ordner {} blieb liegen ({e}); der nächste Lauf räumt ihn weg", new.display()));
        }
    } else {
        fsx::rename(&new, &data)?;
    }
    lines.push(format!("Aktiv: {} ({})", data.display(), describe(s)));
    Ok(())
}

/// pacman-key --populate (pacman-key.sh.in:283) only adds to gpg's database: keys an older keyring package brought
/// stay. Here the ring follows its newest package, so keys that go away are named, and how many installed parts
/// they signed (those stay installed; their next packages need a key of the new keyring).
fn keys_gone(st: &Store, ring: &str, old: &[String], now: &[String], lines: &mut Vec<String>) {
    let gone: Vec<&String> = old.iter().filter(|o| !now.iter().any(|n| n.eq_ignore_ascii_case(o))).collect();
    if gone.is_empty() {
        return;
    }
    let mut signed = 0;
    for p in st.packages().unwrap_or_default() {
        if let Some(v) = st.current(&p)
            && let Ok(m) = st.manifest(&p, &v)
        {
            signed += m.origins.iter().filter(|o| gone.iter().any(|g| g.eq_ignore_ascii_case(&o.key))).count();
        }
    }
    let short: Vec<String> = gone.iter().map(|g| g.chars().take(16).collect()).collect();
    event("pkg.keyring.keys_gone", &[("ring", ring), ("count", &gone.len().to_string()), ("signed_installed", &signed.to_string())]);
    lines.push(format!("Hinweis: {} Schlüssel von {ring} sind im neuen Schlüsselbund nicht mehr da ({}); sie unterschrieben {signed} installierte Teile, die installiert bleiben", gone.len(), short.join(", ")));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::sources::{Level, Sig, Source, Updates};

    fn cfg(servers: &[&str], keyring: &str) -> Config {
        let src = Source { name: "core".into(), servers: servers.iter().map(|s| s.to_string()).collect(), sig_db: Sig::Optional, sig_pkg: Sig::Required, keyring: keyring.into() };
        Config { level: Level::V3, sources: vec![src], origin: "test".into(), space_max_gb: None, updates: Updates::default() }
    }

    #[test]
    fn wanted_parts_follow_rings_and_includes() {
        let d = std::env::temp_dir().join(format!("bitspkg-ownpart-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("etc/bitspkg/keyrings")).unwrap();
        fs::write(d.join("etc/bitspkg/keyrings/cachyos.gpg"), b"x").unwrap();
        fs::write(d.join("etc/bitspkg/keyrings/bad name.gpg"), b"x").unwrap();
        let w = wanted(&d, &cfg(&["include:mirrorlist", "include:cachyos-v3-mirrorlist", "include:../x", "https://a/$repo"], "archlinux"));
        let names: Vec<&str> = w.iter().map(|w| w.pkg.as_str()).collect();
        assert_eq!(names, ["archlinux-keyring", "cachyos-keyring", "pacman-mirrorlist", "cachyos-v3-mirrorlist"]);
        assert_eq!(w[2].what, What::Mirrorlist("mirrorlist".into()));
        fs::remove_dir_all(&d).unwrap();
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bitspkg-own{tag}-{}", std::process::id()));
        let _ = fsx::remove_tree(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// A plain ustar archive (unpack::open takes uncompressed tar as well).
    fn tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data) in entries {
            let mut h = [0u8; 512];
            h[..name.len()].copy_from_slice(name.as_bytes());
            h[100..108].copy_from_slice(b"0000644\0");
            h[108..116].copy_from_slice(b"0000000\0");
            h[116..124].copy_from_slice(b"0000000\0");
            h[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
            h[136..148].copy_from_slice(b"00000000000\0");
            h[156] = b'0';
            h[257..263].copy_from_slice(b"ustar\0");
            h[263..265].copy_from_slice(b"00");
            h[148..156].copy_from_slice(b"        ");
            let sum: u32 = h.iter().map(|&b| b as u32).sum();
            h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
            out.extend_from_slice(&h);
            out.extend_from_slice(data);
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
        out.resize(out.len() + 1024, 0);
        out
    }

    fn pkg(name: &str, version: &str) -> Pkg {
        Pkg { name: name.into(), version: version.into(), filename: format!("{name}-{version}-any.pkg.tar"), sha256: format!("{:064}", version.len()), isize: 1 << 20, ..Default::default() }
    }

    #[test]
    fn hostile_part_packages_are_refused_or_cut_to_their_own_files() {
        let d = tmp("tar");
        let read = |entries: &[(&str, &[u8])], p: &Pkg| {
            let f = d.join("p.pkg.tar");
            fs::write(&f, tar(entries)).unwrap();
            read_files(&f, p, &What::Keyring("bitstest".into()))
        };
        let info = b"pkgname = bitstest-keyring\npkgver = 2-1\n";
        let p = pkg("bitstest-keyring", "2-1");
        let ok = read(&[(".PKGINFO", info), ("usr/share/pacman/keyrings/bitstest.gpg", b"K"), ("usr/share/pacman/keyrings/archlinux.gpg", b"FOREIGN"), ("usr/share/pacman/keyrings/bitstest-trusted", b"T"), ("etc/x", b"X")], &p).unwrap();
        let names: Vec<&str> = ok.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["bitstest.gpg", "bitstest-trusted"], "another ring's file and other paths are never taken");
        let e = read(&[(".PKGINFO", b"pkgname = archlinux-keyring\npkgver = 2-1\n"), ("usr/share/pacman/keyrings/bitstest.gpg", b"K")], &p).unwrap_err();
        assert!(e.contains("nennt sich archlinux-keyring"), "{e}");
        let e = read(&[(".PKGINFO", info), ("usr/share/pacman/keyrings/bitstest-trusted", b"T")], &p).unwrap_err();
        assert!(e.contains("kein usr/share/pacman/keyrings/bitstest.gpg"), "{e}");
        let e = read(&[("usr/share/pacman/keyrings/bitstest.gpg", b"K")], &p).unwrap_err();
        assert!(e.contains("kein .PKGINFO"), "{e}");
        let e = read(&[(".PKGINFO", info), ("usr/share/pacman/keyrings/bitstest.gpg", b"K"), ("usr/share/pacman/keyrings/bitstest.gpg", b"L")], &p).unwrap_err();
        assert!(e.contains("zweimal"), "{e}");
        fsx::remove_tree(&d).unwrap();
    }

    #[test]
    fn an_unreadable_new_keyring_is_refused_and_the_old_folder_kept() {
        let root = tmp("ring");
        let st = Store::new(&root);
        let data = root.join(pgp::KEYRING_DIRS[0]);
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("bitstest.gpg"), b"old").unwrap();
        let s = Step { want: Want { pkg: "bitstest-keyring".into(), what: What::Keyring("bitstest".into()) }, repo: "fixture".into(), pkg: pkg("bitstest-keyring", "2-1"), installed: None };
        for bad in [&b"not a keyring"[..], &b""[..]] {
            let mut lines = Vec::new();
            let e = activate(&st, &s, "C", vec![("bitstest.gpg".into(), bad.to_vec())], &[], &mut lines).unwrap_err();
            assert!(e.contains("abgelehnt, der bisherige bleibt"), "{e}");
            assert_eq!(fs::read(data.join("bitstest.gpg")).unwrap(), b"old");
            assert!(!data.with_extension("new").exists(), "no keyrings.new left behind");
            assert!(state(&data).is_empty());
        }
        fsx::remove_tree(&root).unwrap();
    }

    #[test]
    fn mirror_lists_follow_pacmans_backup_rule() {
        let root = tmp("ml");
        let st = Store::new(&root);
        let data = root.join(MIRROR_DATA);
        let step = |v: &str| Step { want: Want { pkg: "bitstest-mirrorlist".into(), what: What::Mirrorlist("ml".into()) }, repo: "fixture".into(), pkg: pkg("bitstest-mirrorlist", v), installed: None };
        let go = |v: &str, body: &str| {
            let mut lines = Vec::new();
            activate(&st, &step(v), "A", vec![("ml".into(), body.as_bytes().to_vec())], &[], &mut lines).unwrap();
            lines.join("\n")
        };
        let read = |n: &str| fs::read_to_string(data.join(n)).ok();
        let (v1, v2, v3) = ("Server = http://a/$repo\n", "Server = http://b/$repo\n", "Server = http://c/$repo\n");
        go("1-1", v1);
        assert_eq!(read("ml").as_deref(), Some(v1), "first install");
        go("2-1", v2);
        assert_eq!(read("ml").as_deref(), Some(v2), "unchanged by the user: replaced");
        fs::write(data.join("ml"), "Server = http://mine/$repo\n").unwrap();
        let out = go("3-1", v3);
        assert_eq!(read("ml").as_deref(), Some("Server = http://mine/$repo\n"), "changed by the user: kept\n{out}");
        assert_eq!(read("ml.pacnew").as_deref(), Some(v3));
        let out = go("3-2", v3);
        assert!(out.contains("ändert sie nicht"), "{out}");
        assert_eq!(read("ml").as_deref(), Some("Server = http://mine/$repo\n"), "the package did not change the list: the user's stays");
        fs::write(data.join("ml"), v3).unwrap();
        go("4-1", "#Server = http://d/$repo\n");
        assert_eq!(read("ml").as_deref(), Some(v3), "a list without an active server never replaces a working one");
        assert_eq!(read("ml.pacnew").as_deref(), Some("#Server = http://d/$repo\n"));
        assert_eq!(state(&data)[0].version, "4-1");
        fsx::remove_tree(&root).unwrap();
    }

    #[test]
    fn backup_rule_is_add_c() {
        assert_eq!(backup_rule(None, None, "p"), Some(true));
        assert_eq!(backup_rule(Some("p"), Some("o"), "p"), Some(true));
        assert_eq!(backup_rule(Some("l"), Some("p"), "p"), None);
        assert_eq!(backup_rule(Some("o"), Some("o"), "p"), Some(true));
        assert_eq!(backup_rule(Some("l"), Some("o"), "p"), Some(false));
        assert_eq!(backup_rule(Some("l"), None, "p"), Some(false), "a file with no recorded hash (the image's) is kept");
    }

    #[test]
    fn state_round_trip_skips_damaged_lines() {
        let d = std::env::temp_dir().join(format!("bitspkg-ownstate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let i = Installed { pkg: "a-keyring".into(), version: "1-1".into(), sha256: "00".into(), repo: "core".into(), key: String::new() };
        fs::write(d.join(STATE), format!("{}kaputt\n", render_state(std::slice::from_ref(&i)))).unwrap();
        assert_eq!(state(&d), vec![Installed { key: "-".into(), ..i }]);
        fs::remove_dir_all(&d).unwrap();
    }
}
