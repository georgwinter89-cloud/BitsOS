//! `/apps/.pkgs/<pkg>/<version>/`: one pacman package, unpacked once (plan 1.6, D8). Its files are hard links into
//! the content store (`/apps/.store`, PK-1), its folders read-only; `.meta/` keeps `.PKGINFO`, `.BUILDINFO`,
//! `.INSTALL`, `.MTREE` as information, plus `files` (the entry's list, what gc and the program trees read) and
//! `origin`. Unpacking follows pacman/lib/libalpm/add.c:191 extract_single_file in what it takes from an entry,
//! and drops what a program tree must not have (plan 1.6): owners other than root, setuid/setgid, file
//! capabilities, device files and FIFOs, non-ASCII names outside the locale folders. Every file's sha256 and size
//! is checked against `.MTREE`, every `.MTREE` file must be in the archive (pacman checks that only with -Qkk).

use super::meta::{self, PkgInfo, Scriptlet};
use super::mtree::{self, MEntry, MKind};
use super::tar::{Kind, Tar};
use super::unpack;
use crate::fsx::{self, err};
use crate::manifest::{FileEntry, Symlink};
use crate::names::{check_repo_path, check_view_link, non_ascii_allowed};
use crate::store::Store;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const META_DIR: &str = ".meta";
const LIST_MAGIC: &str = "bitspkg-pkgs 1";
/// The list file of one entry: a package's file list, at most as large as a manifest (16 MiB).
const LIST_MAX: u64 = crate::toml::MAX_BYTES as u64;

/// What one entry holds, paths relative to the package root.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct List {
    pub files: Vec<FileEntry>,
    pub symlinks: Vec<Symlink>,
    pub dirs: Vec<String>,
}

impl List {
    pub fn render(&self) -> String {
        let mut o = format!("{LIST_MAGIC}\n");
        for d in &self.dirs {
            o.push_str(&format!("d\t{d}\n"));
        }
        for f in &self.files {
            o.push_str(&format!("f\t{}\t{}\t{}\t{}\n", f.sha256, f.size, if f.exec { "x" } else { "-" }, f.path));
        }
        for l in &self.symlinks {
            o.push_str(&format!("l\t{}\t{}\n", l.path, l.target));
        }
        o
    }

    pub fn parse(text: &str) -> Result<List, String> {
        let mut lines = text.lines();
        if lines.next() != Some(LIST_MAGIC) {
            return Err("keine Liste eines .pkgs-Eintrags".into());
        }
        let mut l = List::default();
        for line in lines {
            let w: Vec<&str> = line.split('\t').collect();
            match w.as_slice() {
                ["d", p] => {
                    check_repo_path(p)?;
                    l.dirs.push(p.to_string());
                }
                ["f", sha, size, x, p] => {
                    check_repo_path(p)?;
                    crate::names::check_sha256(sha)?;
                    let size = size.parse().map_err(|_| format!("Liste: Größe {size:?}"))?;
                    l.files.push(FileEntry { path: p.to_string(), sha256: sha.to_string(), size, exec: *x == "x" });
                }
                ["l", p, t] => {
                    check_repo_path(p)?;
                    check_view_link(p, t)?;
                    l.symlinks.push(Symlink { path: p.to_string(), target: t.to_string() });
                }
                _ => return Err(format!("Liste: Zeile {line:?} unbekannt")),
            }
        }
        Ok(l)
    }
}

/// The entry folders of the store: (pkg, version) sorted.
pub fn entries(st: &Store) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let top = st.pkgs();
    let rd = match fs::read_dir(&top) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(err(&top, e)),
    };
    for e in rd.flatten() {
        let Some(p) = e.file_name().to_str().map(str::to_string) else { continue };
        if p.starts_with('.') || !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        for v in fs::read_dir(e.path()).map_err(|x| err(&e.path(), x))?.flatten() {
            if let Some(n) = v.file_name().to_str()
                && !n.starts_with('.')
                && v.file_type().map(|t| t.is_dir()).unwrap_or(false)
            {
                out.push((p.clone(), n.to_string()));
            }
        }
    }
    out.sort();
    Ok(out)
}

pub fn list_path(st: &Store, pkg: &str, ver: &str) -> PathBuf {
    st.pkgs_entry(pkg, ver).join(META_DIR).join("files")
}

pub fn read_list(st: &Store, pkg: &str, ver: &str) -> Result<List, String> {
    let p = list_path(st, pkg, ver);
    let md = fs::symlink_metadata(&p).map_err(|e| err(&p, e))?;
    if !md.is_file() || md.len() > LIST_MAX {
        return Err(err(&p, "keine Datei oder größer als 16 MiB"));
    }
    List::parse(&fs::read_to_string(&p).map_err(|e| err(&p, e))?).map_err(|e| err(&p, e))
}

/// The `origin` file of an entry: `sha256 <hex>` of the package file it was unpacked from, and its repository.
pub fn read_origin_sha(st: &Store, pkg: &str, ver: &str) -> Option<String> {
    let t = fs::read_to_string(st.pkgs_entry(pkg, ver).join(META_DIR).join("origin")).ok()?;
    t.lines().find_map(|l| l.strip_prefix("sha256 ").map(str::to_string))
}

// ---- the content store, for the entries and the program trees ----

/// Errnos after which a hard link is replaced by a copy: EPERM, EXDEV, EMLINK, ENOSYS, EOPNOTSUPP (install.rs's).
fn no_links(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(1 | 18 | 31 | 38 | 95))
}

#[cfg(test)]
thread_local! {
    /// Test builds only: a link count from which `link` acts as if the file system said EMLINK.
    pub static LINK_LIMIT: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
}

#[cfg(test)]
fn over_limit(cf: &Path) -> bool {
    let lim = LINK_LIMIT.with(|l| l.get());
    fs::symlink_metadata(cf).map(|m| m.nlink() >= lim).unwrap_or(false)
}

#[cfg(not(test))]
fn over_limit(_: &Path) -> bool {
    false
}

/// Writes content into the store and links it where it belongs; a file system that refuses the link (ext4's
/// 65000 links per inode: EMLINK) gets a second copy instead, logged once per install as `event=fallback.copy`.
pub struct Linker<'a> {
    pub st: &'a Store,
    seq: u64,
    /// Store files whose content was checked (or written) in this run.
    verified: HashSet<PathBuf>,
    pub copied: usize,
    pub shared: usize,
    said_copy: bool,
}

impl<'a> Linker<'a> {
    pub fn new(st: &'a Store) -> Linker<'a> {
        Linker { st, seq: 0, verified: HashSet::new(), copied: 0, shared: 0, said_copy: false }
    }

    /// A store file is shared only when its content is what its name says: hashed once per run (PK-1's rule).
    pub fn good(&mut self, cf: &Path, sha: &str, size: u64) -> bool {
        if self.verified.contains(cf) {
            return true;
        }
        match fs::symlink_metadata(cf) {
            Ok(md) if md.is_file() && md.len() == size => {}
            _ => return false,
        }
        match fsx::hash_file(cf) {
            Ok((h, n)) if h == sha && n == size => {
                self.verified.insert(cf.to_path_buf());
                true
            }
            r => {
                let why = r.map(|(h, _)| format!("sha256 {h}")).unwrap_or_else(|e| e);
                eprintln!("bitspkg: event=store.bad path={} reason=\"{why}\" (replaced from the package)", cf.display());
                false
            }
        }
    }

    fn tmp(&mut self) -> PathBuf {
        self.seq += 1;
        self.st.content().join(format!(".tmp-{}-{}", std::process::id(), self.seq))
    }

    /// Streams `r` into the store (hashing it on the way) and returns its entry; the store file is on disk.
    pub fn store(&mut self, r: &mut dyn FnMut(&mut [u8]) -> Result<usize, String>, path: &str, exec: bool) -> Result<FileEntry, String> {
        let tmp = self.tmp();
        let mode = if exec { 0o555 } else { 0o444 };
        let res = (|| {
            let mut o = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp).map_err(|e| err(&tmp, e))?;
            let mut h = Sha256::new();
            let mut buf = vec![0u8; 256 << 10];
            let mut size = 0u64;
            loop {
                let n = r(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
                o.write_all(&buf[..n]).map_err(|e| err(&tmp, e))?;
                size += n as u64;
            }
            o.sync_all().map_err(|e| err(&tmp, e))?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).map_err(|e| err(&tmp, e))?;
            Ok::<_, String>((crate::names::hex(&h.finalize()), size))
        })();
        let (sha, size) = match res {
            Ok(x) => x,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        };
        let cf = self.st.content_file(&sha, exec);
        if self.good(&cf, &sha, size) {
            let _ = fs::remove_file(&tmp);
            self.shared += 1;
        } else {
            fsx::rename(&tmp, &cf)?;
            self.verified.insert(cf);
        }
        Ok(FileEntry { path: path.to_string(), sha256: sha, size, exec })
    }

    /// Hard-links the store file of `f` to `dst`; a refused link (EMLINK and the like) becomes a checked copy.
    pub fn link(&mut self, f: &FileEntry, dst: &Path) -> Result<(), String> {
        let cf = self.st.content_file(&f.sha256, f.exec);
        let r = if over_limit(&cf) { Err(std::io::Error::from_raw_os_error(31)) } else { fs::hard_link(&cf, dst) };
        match r {
            Ok(()) => Ok(()),
            Err(e) if no_links(&e) => {
                if !self.said_copy {
                    eprintln!("bitspkg: event=fallback.copy reason=\"hard link refused: {e}\" (a second copy of the content)");
                    self.said_copy = true;
                }
                let mode = if f.exec { 0o555 } else { 0o444 };
                let (sha, size) = fsx::copy_hashed(&cf, dst, mode)?;
                if sha != f.sha256 || size != f.size {
                    let _ = fs::remove_file(dst);
                    return Err(format!("{}: Inhalt passt nicht (sha256 {sha})", f.path));
                }
                self.copied += 1;
                Ok(())
            }
            Err(e) => Err(err(dst, e)),
        }
    }
}

// ---- unpacking one package ----

/// What was dropped at unpack (plan 1.6), one list per kind: (path, detail).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dropped {
    pub owners: Vec<String>,
    pub setid: Vec<String>,
    pub xattrs: Vec<String>,
    pub specials: Vec<String>,
    pub non_ascii: Vec<String>,
}

impl Dropped {
    /// One line per kind with the count and the first example (the report logs each kind once).
    pub fn lines(&self) -> Vec<String> {
        let mut v = Vec::new();
        for (what, l) in [
            ("Besitzer außer root (Dateien gehören root, 0444/0555)", &self.owners),
            ("setuid/setgid-Bits", &self.setid),
            ("Datei-Fähigkeiten und erweiterte Attribute", &self.xattrs),
            ("Gerätedateien und FIFOs", &self.specials),
            ("Namen mit Nicht-ASCII-Zeichen außerhalb der Sprachdaten", &self.non_ascii),
        ] {
            if let Some(first) = l.first() {
                v.push(format!("{what}: {} (z. B. {first})", l.len()));
            }
        }
        v
    }
    pub fn merge(&mut self, o: &Dropped) {
        self.owners.extend(o.owners.iter().cloned());
        self.setid.extend(o.setid.iter().cloned());
        self.xattrs.extend(o.xattrs.iter().cloned());
        self.specials.extend(o.specials.iter().cloned());
        self.non_ascii.extend(o.non_ascii.iter().cloned());
    }
}

#[derive(Clone, Debug, Default)]
pub struct Unpacked {
    pub list: List,
    pub dropped: Dropped,
    pub info: PkgInfo,
    pub scriptlet: Option<Scriptlet>,
}

/// Facts about the package file for `.meta/origin`.
pub struct OriginFacts<'a> {
    pub repo: &'a str,
    pub filename: &'a str,
    pub sha256: &'a str,
    pub key: &'a str,
}

fn read_all(tar: &mut Tar<Box<dyn Read>>, max: u64, what: &str) -> Result<Vec<u8>, String> {
    let mut v = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = tar.read_data(&mut buf)?;
        if n == 0 {
            return Ok(v);
        }
        if (v.len() + n) as u64 > max {
            return Err(format!("{what} größer als {max} Bytes"));
        }
        v.extend_from_slice(&buf[..n]);
    }
}

fn mkdir_in(base: &Path, rel: &str) -> Result<(), String> {
    let mut p = base.to_path_buf();
    for c in rel.split('/') {
        p.push(c);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => continue,
            Ok(_) => return Err(err(&p, "kein Ordner (ein Verweis oder eine Datei)")),
            Err(_) => fsx::mkdir(&p, 0o755)?,
        }
    }
    Ok(())
}

fn parent_in(base: &Path, rel: &str) -> Result<(), String> {
    match rel.rsplit_once('/') {
        Some((d, _)) => mkdir_in(base, d),
        None => Ok(()),
    }
}

/// Unpacks the (verified) package file `file` into the staging folder `stage` and checks it against its
/// `.MTREE`. `isize` is the database's %ISIZE%: the archive may unpack to little more (tar headers).
pub fn unpack(lk: &mut Linker, file: &Path, stage: &Path, isize: u64) -> Result<Unpacked, String> {
    let f = File::open(file).map_err(|e| err(file, e))?;
    let bound = isize.saturating_add(isize / 8).saturating_add(64 << 20);
    let r = unpack::open(unpack::Bounded::new(f, bound, "Paket")).map_err(|e| format!("Paket: {e}"))?;
    let mut tar = Tar::new(r).keep_specials();
    let mut u = Unpacked::default();
    let mut mtree_raw: Option<Vec<u8>> = None;
    let mut meta_files: Vec<(&'static str, Vec<u8>)> = Vec::new();
    // Every path once, and no leaf (file or link) as a folder of another entry: nothing is written through a link.
    let mut seen: HashMap<String, char> = HashMap::new();
    let mut by_path: HashMap<String, usize> = HashMap::new();
    let mut skipped: HashSet<String> = HashSet::new();
    while let Some(e) = tar.next_entry()? {
        if let Some(m) = meta::MEMBERS.iter().find(|m| **m == e.path) {
            let data = read_all(&mut tar, if *m == meta::MTREE { mtree::MTREE_MAX } else { meta::META_MAX }, m)?;
            if *m == meta::MTREE {
                mtree_raw = Some(data);
            } else {
                meta_files.push((m, data));
            }
            continue;
        }
        if e.path == META_DIR || e.path.starts_with(".meta/") {
            return Err(format!("{}: der Name .meta ist für BitsOS reserviert", e.path));
        }
        if !e.path.is_ascii() && !non_ascii_allowed(&e.path) {
            u.dropped.non_ascii.push(e.path.clone());
            skipped.insert(e.path.clone());
            continue;
        }
        check_repo_path(&e.path)?;
        let mut at = 0;
        while let Some(i) = e.path[at..].find('/') {
            let pre = &e.path[..at + i];
            if skipped.contains(pre) {
                skipped.insert(e.path.clone());
                break;
            }
            if matches!(seen.get(pre), Some('f' | 'l')) {
                return Err(format!("{}: {pre} ist eine Datei oder ein Verweis, kein Ordner", e.path));
            }
            at += i + 1;
        }
        if skipped.contains(&e.path) {
            if matches!(e.kind, Kind::File) {
                u.dropped.non_ascii.push(e.path.clone());
            }
            continue;
        }
        if e.uid != 0 || e.gid != 0 {
            u.dropped.owners.push(format!("{} ({}:{})", e.path, e.uid, e.gid));
        }
        if e.mode & 0o6000 != 0 && matches!(e.kind, Kind::File | Kind::HardLink(_)) {
            u.dropped.setid.push(e.path.clone());
        }
        for x in &e.xattrs {
            u.dropped.xattrs.push(format!("{} ({x})", e.path));
        }
        let kind = match &e.kind {
            Kind::Dir => 'd',
            Kind::File | Kind::HardLink(_) => 'f',
            Kind::Symlink(_) => 'l',
            Kind::Special(what) => {
                u.dropped.specials.push(format!("{} ({what})", e.path));
                skipped.insert(e.path.clone());
                continue;
            }
        };
        match seen.get(&e.path) {
            Some('d') if kind == 'd' => continue,
            Some(_) => return Err(format!("{}: zweimal im Paket", e.path)),
            None => {}
        }
        seen.insert(e.path.clone(), kind);
        match e.kind {
            Kind::Dir => {
                mkdir_in(stage, &e.path)?;
                u.list.dirs.push(e.path.clone());
            }
            Kind::File => {
                parent_in(stage, &e.path)?;
                let exec = e.mode & 0o111 != 0;
                let fe = lk.store(&mut |b| tar.read_data(b), &e.path, exec)?;
                lk.link(&fe, &stage.join(&e.path))?;
                by_path.insert(e.path.clone(), u.list.files.len());
                u.list.files.push(fe);
            }
            Kind::HardLink(target) => {
                let i = *by_path.get(&target).ok_or_else(|| format!("{}: harter Verweis auf {target}, das nicht vorher im Paket steht", e.path))?;
                let mut fe = u.list.files[i].clone();
                fe.path = e.path.clone();
                parent_in(stage, &e.path)?;
                lk.link(&fe, &stage.join(&e.path))?;
                by_path.insert(e.path.clone(), u.list.files.len());
                u.list.files.push(fe);
            }
            Kind::Symlink(target) => {
                check_view_link(&e.path, &target)?;
                parent_in(stage, &e.path)?;
                let at = stage.join(&e.path);
                std::os::unix::fs::symlink(&target, &at).map_err(|x| err(&at, x))?;
                u.list.symlinks.push(Symlink { path: e.path.clone(), target });
            }
            Kind::Special(_) => {}
        }
    }
    let raw = mtree_raw.ok_or("das Paket hat kein .MTREE: abgelehnt")?;
    let text = {
        let mut r = unpack::open(unpack::Bounded::new(std::io::Cursor::new(raw.clone()), mtree::MTREE_MAX, ".MTREE")).map_err(|e| format!(".MTREE: {e}"))?;
        let mut s = String::new();
        r.read_to_string(&mut s).map_err(|e| format!(".MTREE: {e}"))?;
        s
    };
    check_mtree(&u.list, &mtree::parse(&text)?, &skipped)?;
    for (m, data) in &meta_files {
        let text = String::from_utf8_lossy(data);
        if *m == meta::PKGINFO {
            u.info = meta::pkginfo(&text);
        }
        if *m == meta::INSTALL {
            u.scriptlet = Some(meta::scriptlet(&text));
        }
    }
    meta_files.push((meta::MTREE, raw));
    let md = stage.join(META_DIR);
    fsx::mkdir(&md, 0o755)?;
    for (m, data) in &meta_files {
        fsx::write_atomic(&md.join(m.trim_start_matches('.')), data, 0o444)?;
    }
    Ok(u)
}

/// Writes `.meta/files` and `.meta/origin`, then seals the staging folder (folders 0555).
pub fn finish(stage: &Path, u: &Unpacked, o: &OriginFacts) -> Result<(), String> {
    let md = stage.join(META_DIR);
    fsx::write_atomic(&md.join("files"), u.list.render().as_bytes(), 0o444)?;
    let origin = format!("repo {}\nfilename {}\nsha256 {}\nkey {}\npackager {}\n", o.repo, o.filename, o.sha256, o.key, u.info.packager);
    fsx::write_atomic(&md.join("origin"), origin.as_bytes(), 0o444)?;
    fsx::seal_dirs(stage)
}

/// Every file and link of the archive as `.MTREE` says (type, size, sha256, target), and every `.MTREE` file and
/// link in the archive (apart from what was dropped).
fn check_mtree(l: &List, m: &BTreeMap<String, MEntry>, skipped: &HashSet<String>) -> Result<(), String> {
    let bad = |p: &str, why: String| Err(format!("{p}: passt nicht zu .MTREE ({why}): abgelehnt"));
    let mut have: HashSet<&str> = HashSet::new();
    for f in &l.files {
        have.insert(&f.path);
        match m.get(&f.path).map(|e| &e.kind) {
            Some(MKind::File { size, sha256 }) if *size == f.size && *sha256 == f.sha256 => {}
            Some(MKind::File { size, sha256 }) => return bad(&f.path, format!("Größe {} statt {size}, sha256 {} statt {sha256}", f.size, f.sha256)),
            Some(k) => return bad(&f.path, format!("dort {k:?}")),
            None => return bad(&f.path, "fehlt dort".into()),
        }
    }
    for s in &l.symlinks {
        have.insert(&s.path);
        match m.get(&s.path).map(|e| &e.kind) {
            Some(MKind::Link(t)) if *t == s.target => {}
            k => return bad(&s.path, format!("Verweis auf {}, dort {k:?}", s.target)),
        }
    }
    for (p, e) in m {
        if meta::MEMBERS.contains(&p.as_str()) || skipped.contains(p) || matches!(e.kind, MKind::Dir) {
            continue;
        }
        if !have.contains(p.as_str()) {
            return bad(p, "steht in .MTREE, fehlt im Archiv".into());
        }
    }
    Ok(())
}

/// Bytes of an entry's files (each once), for the space report and tests.
pub fn size_of(l: &List) -> u64 {
    l.files.iter().map(|f| f.size).fold(0u64, u64::saturating_add)
}

/// The modes `seal_dirs` leaves, for tests: folders 0555.
pub fn sealed(p: &Path) -> bool {
    fs::symlink_metadata(p).map(|m| m.mode() & 0o777 == 0o555).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str, ty: u8, size: u64, mode: u32, uid: u32, link: &str) -> [u8; 512] {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(format!("{mode:07o}").as_bytes());
        h[108..115].copy_from_slice(format!("{uid:07o}").as_bytes());
        h[116..123].copy_from_slice(b"0000000");
        h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
        h[136..147].copy_from_slice(b"14000000000");
        h[156] = ty;
        h[157..157 + link.len()].copy_from_slice(link.as_bytes());
        h[257..263].copy_from_slice(b"ustar\0");
        h[148..156].copy_from_slice(b"        ");
        let sum: u64 = h.iter().map(|&b| b as u64).sum();
        h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        h
    }

    /// (name, type, data, mode, uid, link)
    type Member<'a> = (&'a str, u8, &'a [u8], u32, u32, &'a str);

    /// Members as a tar, `.MTREE` first.
    fn package(entries: &[Member], mtree: &str) -> Vec<u8> {
        let mut t = Vec::new();
        let all = std::iter::once((".MTREE", b'0', mtree.as_bytes(), 0o644, 0, "")).chain(entries.iter().copied());
        for (n, ty, d, mode, uid, link) in all {
            t.extend_from_slice(&header(n, ty, d.len() as u64, mode, uid, link));
            t.extend_from_slice(d);
            t.resize(t.len().div_ceil(512) * 512, 0);
        }
        t.resize(t.len() + 1024, 0);
        t
    }

    fn sha(d: &[u8]) -> String {
        crate::names::hex(&Sha256::digest(d))
    }

    fn scratch(name: &str) -> (PathBuf, Store) {
        let d = std::env::temp_dir().join(format!("bitspkg-pkgs-{name}-{}", std::process::id()));
        if d.exists() {
            let _ = std::process::Command::new("chmod").args(["-R", "u+w"]).arg(&d).status();
            fs::remove_dir_all(&d).unwrap();
        }
        let st = Store::new(&d);
        st.ensure_layout().unwrap();
        (d, st)
    }

    #[test]
    fn unpack_checks_mtree_and_drops_what_a_tree_must_not_have() {
        let (d, st) = scratch("unpack");
        let bin = b"#!/bin/sh\necho hi\n";
        let mtree = format!(
            "#mtree\n/set type=file uid=0 gid=0 mode=644\n./usr type=dir mode=755\n./usr/bin type=dir mode=755\n./usr/bin/hi mode=4755 size={} sha256digest={}\n./usr/bin/hi2 mode=755 size={} sha256digest={}\n./usr/lib/hi.so type=link link=../bin/hi\n./usr/share/caf\\303\\251 size=1 sha256digest={}\n",
            bin.len(),
            sha(bin),
            bin.len(),
            sha(bin),
            sha(b"x")
        );
        let pkgf = d.join("p.pkg.tar");
        let entries: Vec<Member> = vec![
            ("usr", b'5', b"", 0o755, 0, ""),
            ("usr/bin", b'5', b"", 0o755, 0, ""),
            ("usr/bin/hi", b'0', bin, 0o4755, 7, ""),
            ("usr/bin/hi2", b'1', b"", 0o755, 0, "usr/bin/hi"),
            ("usr/lib", b'5', b"", 0o755, 0, ""),
            ("usr/lib/hi.so", b'2', b"", 0o777, 0, "../bin/hi"),
            ("usr/share", b'5', b"", 0o755, 0, ""),
            ("usr/share/café", b'0', b"x", 0o644, 0, ""),
            ("dev/null", b'3', b"", 0o644, 0, ""),
        ];
        fs::write(&pkgf, package(&entries, &mtree)).unwrap();
        let stage = d.join("stage");
        fs::create_dir(&stage).unwrap();
        let mut lk = Linker::new(&st);
        let u = unpack(&mut lk, &pkgf, &stage, 100).unwrap();
        assert_eq!(u.list.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["usr/bin/hi", "usr/bin/hi2"]);
        assert!(u.list.files.iter().all(|f| f.exec && f.sha256 == sha(bin)));
        assert_eq!(u.dropped.setid, ["usr/bin/hi"]);
        assert_eq!(u.dropped.owners.len(), 1);
        assert_eq!(u.dropped.specials.len(), 1);
        assert_eq!(u.dropped.non_ascii, ["usr/share/café"]);
        let md = fs::metadata(stage.join("usr/bin/hi")).unwrap();
        assert_eq!((md.mode() & 0o7777, md.nlink()), (0o555, 3), "store file + two names, no setuid");
        // One byte of the content differs from .MTREE: refused, naming the path.
        let bad = mtree.replace(&format!("size={} sha256digest={}\n./usr/bin/hi2", bin.len(), sha(bin)), &format!("size={} sha256digest={}\n./usr/bin/hi2", bin.len(), sha(b"other")));
        fs::write(&pkgf, package(&entries, &bad)).unwrap();
        let stage2 = d.join("stage2");
        fs::create_dir(&stage2).unwrap();
        let e = unpack(&mut lk, &pkgf, &stage2, 100).unwrap_err();
        assert!(e.contains("usr/bin/hi") && e.contains(".MTREE"), "{e}");
        // A file .MTREE names that the archive lacks.
        let more = format!("{mtree}./usr/bin/ghost size=1 sha256digest={}\n", sha(b"g"));
        fs::write(&pkgf, package(&entries, &more)).unwrap();
        let stage3 = d.join("stage3");
        fs::create_dir(&stage3).unwrap();
        assert!(unpack(&mut lk, &pkgf, &stage3, 100).unwrap_err().contains("ghost"));
        let _ = std::process::Command::new("chmod").args(["-R", "u+w"]).arg(&d).status();
        let _ = fs::remove_dir_all(&d);
    }

    /// Hostile archives: each is refused with a reason, nothing panics and nothing is written outside the stage.
    #[test]
    fn hostile_archives_are_refused() {
        let (d, st) = scratch("hostile");
        let mtree = "#mtree\n/set type=file uid=0 gid=0 mode=644\n";
        let cases: Vec<(&str, Vec<Member>, &str)> = vec![
            ("leaves the tree", vec![("../x", b'0', b"x", 0o644, 0, "")], ".."),
            ("absolute", vec![("/x", b'0', b"x", 0o644, 0, "")], "absolute"),
            ("hard link to a path not in the package", vec![("h", b'1', b"", 0o644, 0, "etc/passwd")], "harter Verweis"),
            ("write through a link", vec![("usr", b'2', b"", 0o777, 0, "/etc"), ("usr/x", b'0', b"x", 0o644, 0, "")], "kein Ordner"),
            ("write through a relative link", vec![("l", b'2', b"", 0o777, 0, ".."), ("l/x", b'0', b"x", 0o644, 0, "")], "kein Ordner"),
            ("twice", vec![("a", b'0', b"x", 0o644, 0, ""), ("a", b'0', b"y", 0o644, 0, "")], "zweimal"),
            ("reserved name", vec![(".meta/files", b'0', b"x", 0o644, 0, "")], ".meta"),
        ];
        let mut lk = Linker::new(&st);
        for (n, (what, entries, want)) in cases.iter().enumerate() {
            let pkgf = d.join(format!("h{n}.pkg.tar"));
            fs::write(&pkgf, package(entries, mtree)).unwrap();
            let stage = d.join(format!("s{n}"));
            fs::create_dir(&stage).unwrap();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unpack(&mut lk, &pkgf, &stage, 100)));
            let e = r.unwrap_or_else(|_| panic!("{what}: the unpacker panicked")).map(|_| ()).unwrap_err();
            assert!(e.contains(want), "{what}: {e}");
            assert!(!d.join("x").exists(), "{what}: written outside the stage");
        }
        let _ = std::process::Command::new("chmod").args(["-R", "u+w"]).arg(&d).status();
        let _ = fs::remove_dir_all(&d);
    }

    /// EMLINK (ext4's 65000 links per inode) with a link limit of 2 injected into this test build: the third
    /// name of one content gets a checked second copy, `fallback.copy`.
    #[test]
    fn emlink_falls_back_to_a_second_copy() {
        let (d, st) = scratch("emlink");
        let mut lk = Linker::new(&st);
        let data = b"shared content".to_vec();
        let mut at = 0usize;
        let fe = lk
            .store(
                &mut |b: &mut [u8]| {
                    let n = (data.len() - at).min(b.len());
                    b[..n].copy_from_slice(&data[at..at + n]);
                    at += n;
                    Ok(n)
                },
                "a",
                false,
            )
            .unwrap();
        LINK_LIMIT.with(|l| l.set(2));
        lk.link(&fe, &d.join("one")).unwrap();
        lk.link(&fe, &d.join("two")).unwrap();
        LINK_LIMIT.with(|l| l.set(u64::MAX));
        assert_eq!(lk.copied, 1);
        let cf = st.content_file(&fe.sha256, false);
        assert_eq!(fs::metadata(&cf).unwrap().nlink(), 2, "the store file and its first link");
        assert_eq!(fs::metadata(d.join("two")).unwrap().nlink(), 1, "the second name is a copy");
        assert_eq!(fs::read(d.join("two")).unwrap(), data);
        let _ = std::process::Command::new("chmod").args(["-R", "u+w"]).arg(&d).status();
        let _ = fs::remove_dir_all(&d);
    }
}
