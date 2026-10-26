//! verify and list: every installed version against its manifest (hashes, sizes, modes, links, nothing extra),
//! the `current`/`previous` links, and with `store` every content file against its own name and every version
//! file against its store copy. Reports only: nothing is repaired, nothing written (PK-5).

use crate::fsx;
use crate::manifest::{FILE_NAME, Manifest};
use crate::store::Store;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

/// How long verify waits for a changing bitspkg to finish (same bound as the changing commands themselves).
pub const LOCK_WAIT: Duration = Duration::from_secs(60);

/// What one verify run looked at (the CLI prints it, so "all good" says how much was checked).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Checked {
    pub packages: usize,
    pub versions: usize,
    pub files: usize,
    pub store_files: usize,
    /// Bytes actually read and hashed: a version file and its store copy are one inode, read once (a part that
    /// looked wrong and was checked again under the lock is read twice and counted twice).
    pub bytes_hashed: u64,
}

impl Checked {
    fn add(&mut self, o: &Checked) {
        self.packages += o.packages;
        self.versions += o.versions;
        self.files += o.files;
        self.store_files += o.store_files;
    }
}

/// Hashes by inode: version files are hard links into `apps/.store/` and shared between versions, so each
/// inode is read once per run (Brave: 328 MB instead of twice that and more).
#[derive(Default)]
struct Hashes {
    seen: HashMap<(u64, u64), Result<(String, u64), String>>,
    bytes: u64,
}

impl Hashes {
    fn get(&mut self, p: &Path, md: &fs::Metadata) -> Result<(String, u64), String> {
        let key = (md.dev(), md.ino());
        if let Some(r) = self.seen.get(&key) {
            return r.clone();
        }
        let r = fsx::hash_file(p);
        if let Ok((_, n)) = &r {
            self.bytes += n;
        }
        self.seen.insert(key, r.clone());
        r
    }
}

pub struct Info {
    pub name: String,
    pub current: Option<String>,
    pub previous: Option<String>,
    pub versions: Vec<String>,
    pub program_id: Option<u64>,
}

pub fn list(st: &Store) -> Result<Vec<Info>, String> {
    st.packages()?
        .into_iter()
        .map(|name| {
            Ok(Info {
                current: st.current(&name),
                previous: st.previous(&name),
                versions: st.versions(&name)?,
                program_id: st.program_id(&name),
                name,
            })
        })
        .collect()
}

/// Problems found, one line each; empty = all good.
pub fn verify(st: &Store, only: Option<&str>, store: bool) -> Result<Vec<String>, String> {
    verify_counted(st, only, store).map(|(bad, _)| bad)
}

/// [`verify`] plus what it looked at.
pub fn verify_counted(st: &Store, only: Option<&str>, store: bool) -> Result<(Vec<String>, Checked), String> {
    verify_wait(st, only, store, LOCK_WAIT)
}

/// [`verify_counted`] with the lock wait as an argument (tests use a short one).
///
/// Locking: the whole store is read once without any lock, so a long run (Brave cold from a stick) never keeps
/// install/remove/register waiting. Only a package that looked wrong (or vanished, or was half-built) and a store
/// file that looked wrong are read again, one at a time under the store lock taken shared (bounded by `wait`)
/// and released right after: a changing command running at the same time is waited for instead of its
/// half-built folders being reported as damage, and it gets the lock between two re-checks. A package that is
/// gone by then was removed, not damaged; one installed while the run went on is checked too (one more listing
/// at the end). Where the lock file cannot be opened (no store yet, a copy without it) the re-check is unlocked.
pub fn verify_wait(st: &Store, only: Option<&str>, store: bool, wait: Duration) -> Result<(Vec<String>, Checked), String> {
    if let Some(n) = only {
        crate::names::check_name(n)?;
    }
    let mut bad = Vec::new();
    let mut checked = Checked::default();
    let mut h = Hashes::default();
    let mut again: Vec<String> = Vec::new();
    let mut listed: HashSet<String> = HashSet::new();
    let list = |st: &Store| -> Result<Vec<String>, String> {
        match only {
            Some(n) => Ok(vec![n.to_string()]),
            None => st.packages(),
        }
    };
    // Two listings: a package installed during the first round is checked in the second.
    for _ in 0..2 {
        for name in list(st)? {
            if !listed.insert(name.clone()) {
                continue;
            }
            let (b, c) = check_package(st, &name, store, &mut h);
            if b.is_empty() {
                checked.add(&c);
            } else {
                again.push(name);
            }
        }
    }
    for name in again {
        let _shared = lock_shared(st, wait)?;
        if !is_package(st, &name) {
            if only.is_some() {
                return Err(format!("{name} is not installed"));
            }
            continue;
        }
        let mut fresh = Hashes::default();
        let (b, c) = check_package(st, &name, store, &mut fresh);
        h.bytes += fresh.bytes;
        bad.extend(b);
        checked.add(&c);
    }
    if store {
        let (b, n) = check_store(st, &mut h);
        checked.store_files += n;
        for name in b {
            let _shared = lock_shared(st, wait)?;
            let mut fresh = Hashes::default();
            if let Some(line) = check_store_file(st, &name, &mut fresh) {
                bad.push(line);
            }
            h.bytes += fresh.bytes;
        }
    }
    checked.bytes_hashed = h.bytes;
    Ok((bad, checked))
}

fn is_package(st: &Store, name: &str) -> bool {
    fs::symlink_metadata(st.pkg(name)).map(|m| m.is_dir()).unwrap_or(false)
}

/// One package: its links and every version. Never an Err: a package that vanishes while it is read gives a
/// problem line, and the caller checks it again under the lock.
fn check_package(st: &Store, name: &str, store: bool, h: &mut Hashes) -> (Vec<String>, Checked) {
    let mut bad = Vec::new();
    let mut c = Checked { packages: 1, ..Checked::default() };
    if !is_package(st, name) {
        bad.push(format!("{name} is not installed"));
        return (bad, c);
    }
    let versions = match st.versions(name) {
        Ok(v) => v,
        Err(e) => {
            bad.push(e);
            return (bad, c);
        }
    };
    for (link, v) in [("current", st.current(name)), ("previous", st.previous(name))] {
        match v {
            Some(v) if !versions.contains(&v) => bad.push(format!("{name}: {link} points to missing version {v}")),
            None if link == "current" => bad.push(format!("{name}: no current version")),
            _ => {}
        }
    }
    for v in &versions {
        c.versions += 1;
        match st.manifest(name, v) {
            Ok(m) => check_version(st, &st.version_dir(name, v), &m, store, h, &mut c, &mut bad),
            Err(e) => bad.push(e),
        }
    }
    (bad, c)
}

fn lock_shared(st: &Store, wait: Duration) -> Result<Option<File>, String> {
    let p = st.apps.join(".lock");
    let Ok(f) = File::open(&p) else { return Ok(None) };
    let start = Instant::now();
    loop {
        match f.try_lock_shared() {
            Ok(()) => return Ok(Some(f)),
            Err(fs::TryLockError::WouldBlock) if start.elapsed() < wait => std::thread::sleep(Duration::from_millis(100)),
            Err(fs::TryLockError::WouldBlock) => return Err(format!("{}: another bitspkg is still working", p.display())),
            // A file system without flock: the read is still worth doing.
            Err(fs::TryLockError::Error(_)) => return Ok(None),
        }
    }
}

fn check_version(st: &Store, dir: &Path, m: &Manifest, store: bool, h: &mut Hashes, c: &mut Checked, bad: &mut Vec<String>) {
    let at = format!("{} {}", m.name, m.version);
    let mut expected: HashSet<String> = HashSet::new();
    expected.insert(FILE_NAME.into());
    for f in &m.files {
        add_with_parents(&mut expected, &f.path);
        let p = dir.join(&f.path);
        match fs::symlink_metadata(&p) {
            Ok(md) if md.is_file() => {
                c.files += 1;
                let want = if f.exec { 0o555 } else { 0o444 };
                if md.permissions().mode() & 0o7777 != want {
                    bad.push(format!("{at}: {} has mode {:o}, want {want:o}", f.path, md.permissions().mode() & 0o7777));
                }
                match h.get(&p, &md) {
                    Ok((sha, size)) if sha == f.sha256 && size == f.size => {}
                    Ok((sha, size)) => bad.push(format!("{at}: {} changed (sha256 {sha}, {size} bytes)", f.path)),
                    Err(e) => bad.push(format!("{at}: {e}")),
                }
                if store {
                    check_store_copy(st, &at, &f.path, &f.sha256, f.exec, bad);
                }
            }
            Ok(_) => bad.push(format!("{at}: {} is not a regular file", f.path)),
            Err(e) => bad.push(format!("{at}: {}: {e}", f.path)),
        }
    }
    for l in &m.symlinks {
        add_with_parents(&mut expected, &l.path);
        match fsx::read_link(&dir.join(&l.path)) {
            Some(t) if t == l.target => {}
            Some(t) => bad.push(format!("{at}: symlink {} points to {t}, want {}", l.path, l.target)),
            None => bad.push(format!("{at}: symlink {} is missing", l.path)),
        }
    }
    for d in &m.dirs {
        add_with_parents(&mut expected, d);
        if !dir.join(d).is_dir() {
            bad.push(format!("{at}: folder {d} is missing"));
        }
    }
    walk_extra(dir, "", &expected, &at, bad);
}

/// Every version file's content must also be in the store (install puts it there first, hard links or not;
/// gc and `install` sharing rely on it). A version file that is a separate copy of its store file is no damage
/// (file system without hard links, or a store file replaced after it went bad): the space report counts it.
fn check_store_copy(st: &Store, at: &str, path: &str, sha: &str, exec: bool, bad: &mut Vec<String>) {
    let name = crate::store::content_name(sha, exec);
    match fs::symlink_metadata(st.content_file(sha, exec)) {
        // Present but not a regular file: check_store names it.
        Ok(_) => {}
        Err(_) => bad.push(format!("{at}: {path}: store copy {name} is missing")),
    }
}

fn add_with_parents(set: &mut HashSet<String>, p: &str) {
    set.insert(p.to_string());
    let mut at = 0;
    while let Some(i) = p[at..].find('/') {
        set.insert(p[..at + i].to_string());
        at += i + 1;
    }
}

fn walk_extra(dir: &Path, rel: &str, expected: &HashSet<String>, at: &str, bad: &mut Vec<String>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        let r = if rel.is_empty() { n } else { format!("{rel}/{n}") };
        if !expected.contains(&r) {
            bad.push(format!("{at}: {r} is not part of the package"));
        } else if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            walk_extra(&e.path(), &r, expected, at, bad);
        }
    }
}

/// Every store file hashed against its own name, without the lock. Returns the names that looked wrong (checked
/// again under the lock by the caller) and how many regular files were read.
fn check_store(st: &Store, h: &mut Hashes) -> (Vec<String>, usize) {
    let (mut wrong, mut n) = (Vec::new(), 0);
    let Ok(rd) = fs::read_dir(st.content()) else { return (wrong, n) };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if fs::symlink_metadata(e.path()).map(|m| m.is_file()).unwrap_or(false) {
            n += 1;
        }
        if check_store_file(st, &name, h).is_some() {
            wrong.push(name);
        }
    }
    (wrong, n)
}

/// One store file against its own name; None = good or gone (gc removed it: not damage).
fn check_store_file(st: &Store, n: &str, h: &mut Hashes) -> Option<String> {
    let sha = n.strip_suffix(".x").unwrap_or(n);
    let p = st.content().join(n);
    let r = match fs::symlink_metadata(&p) {
        Ok(md) if md.is_file() => h.get(&p, &md),
        Ok(_) => fsx::hash_file(&p),
        Err(_) => return None,
    };
    match r {
        Ok((x, _)) if x == sha => None,
        Ok((x, _)) => Some(format!("store: {n} holds content {x}")),
        Err(x) => Some(format!("store: {x}")),
    }
}
