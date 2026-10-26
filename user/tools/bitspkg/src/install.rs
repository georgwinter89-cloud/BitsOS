//! install, switch, rollback. The order is what makes a cut harmless: the new version is built whole in a
//! staging folder, renamed into place, and only then does `current` move (one rename of a symlink). Until that
//! rename the old version is what runs; after it the new one is complete.

use crate::cut::step;
use crate::fsx::{self, err};
use crate::manifest::{FILE_NAME, FileEntry, Manifest};
use crate::names::{check_name, check_version};
use crate::store::{Lock, Store, read_manifest_text};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Installed,
    /// The same version with the same manifest was there already; it is current now.
    AlreadyThere,
}

#[derive(Debug)]
pub struct Report {
    pub manifest: Manifest,
    pub outcome: Outcome,
    pub program_id: u64,
    pub previous: Option<String>,
    /// Files copied instead of hard-linked because the file system has no hard links (logged once).
    pub copied: usize,
    pub shared: usize,
    /// Hooks (settings, rules, actions) that failed after the commit; `bitspkg register` repeats them.
    pub hooks_failed: usize,
}

/// Errnos after which hard links are given up for this install: EPERM, EXDEV, EMLINK, ENOSYS, EOPNOTSUPP.
fn no_links(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(1 | 18 | 31 | 38 | 95))
}

struct Placer<'a> {
    st: &'a Store,
    src: &'a Path,
    links: bool,
    copied: usize,
    shared: usize,
    seq: u64,
    /// Store files whose content was checked (or written) in this run.
    verified: HashSet<PathBuf>,
}

impl Placer<'_> {
    fn place(&mut self, f: &FileEntry, dst: &Path) -> Result<(), String> {
        let mode = if f.exec { 0o555 } else { 0o444 };
        let from = self.src.join(&f.path);
        no_link_parents(self.src, &f.path)?;
        if !self.links {
            return self.copy_checked(&from, dst, f, mode);
        }
        let cf = self.st.content_file(&f.sha256, f.exec);
        if self.store_file_good(&cf, f) {
            self.shared += 1;
        } else {
            self.seq += 1;
            let tmp = self.st.content().join(format!(".tmp-{}-{}", std::process::id(), self.seq));
            self.copy_checked(&from, &tmp, f, mode)?;
            // Over a bad store file too: versions linked to the bad inode keep it (verify names them), new
            // versions get the checked copy.
            fsx::rename(&tmp, &cf)?;
            self.verified.insert(cf.clone());
        }
        step("link file");
        match fs::hard_link(&cf, dst) {
            Ok(()) => Ok(()),
            Err(e) if no_links(&e) => {
                eprintln!("bitspkg: event=fallback.copy reason=\"hard link refused: {e}\" (files are copied, not shared)");
                self.links = false;
                self.copy_checked(&cf, dst, f, mode)
            }
            Err(e) => Err(err(dst, e)),
        }
    }

    /// A store file is shared only when its content is the manifest's: hashed once per run (a store file is
    /// 0444 but its owner can still change it, and a disk can rot). Costs one read of each shared file per
    /// install, the cold path; a mismatch is an event and the file is replaced from the checked source.
    fn store_file_good(&mut self, cf: &Path, f: &FileEntry) -> bool {
        if self.verified.contains(cf) {
            return true;
        }
        match fs::symlink_metadata(cf) {
            Ok(md) if md.is_file() && md.len() == f.size => {}
            _ => return false,
        }
        match fsx::hash_file(cf) {
            Ok((sha, size)) if sha == f.sha256 && size == f.size => {
                self.verified.insert(cf.to_path_buf());
                true
            }
            r => {
                let why = r.map(|(sha, _)| format!("sha256 {sha}")).unwrap_or_else(|e| e);
                eprintln!("bitspkg: event=store.bad path={} reason=\"{why}\" (replaced from the package)", cf.display());
                false
            }
        }
    }

    fn copy_checked(&mut self, from: &Path, to: &Path, f: &FileEntry, mode: u32) -> Result<(), String> {
        let (sha, size) = fsx::copy_hashed(from, to, mode)?;
        if sha != f.sha256 || size != f.size {
            let _ = fs::remove_file(to);
            return Err(format!("{}: content does not match the manifest (sha256 {sha}, {size} bytes)", f.path));
        }
        if to.parent() != Some(self.st.content().as_path()) {
            self.copied += 1;
        }
        Ok(())
    }
}

/// A package folder whose sub-folder is a symlink (`lib -> /etc`) would let a manifest copy any file whose hash
/// it knows into a world-readable package; the last component is opened with O_NOFOLLOW (fsx::copy_hashed).
fn no_link_parents(src: &Path, rel: &str) -> Result<(), String> {
    let mut p = src.to_path_buf();
    let mut parts = rel.split('/').peekable();
    while let Some(c) = parts.next() {
        if parts.peek().is_none() {
            break;
        }
        p.push(c);
        match fs::symlink_metadata(&p) {
            Ok(md) if md.is_dir() => {}
            _ => return Err(err(&p, "not a folder of the package (a symlink or missing)")),
        }
    }
    Ok(())
}

/// The source's manifest, checked (a package folder from `bitspkg build`).
pub fn read_source(src: &Path) -> Result<Manifest, String> {
    let p = src.join(FILE_NAME);
    Manifest::from_text(&read_manifest_text(&p)?).map_err(|e| err(&p, e))
}

pub fn install(st: &Store, src: &Path) -> Result<Report, String> {
    let lock = st.lock()?;
    install_locked(st, &lock, src)
}

/// [`install`] for a caller that already holds the store lock (history::bring_back re-checks under it first).
pub(crate) fn install_locked(st: &Store, _lock: &Lock, src: &Path) -> Result<Report, String> {
    let m = read_source(src)?;
    if st.pending_removal(&m.name)? {
        return Err(format!("the removal of {} is not finished (settingsd was not reachable); run bitspkg gc once it is", m.name));
    }
    let pkg = st.pkg(&m.name);
    fsx::mkdir(&pkg, 0o755)?;
    let vdir = st.version_dir(&m.name, &m.version);
    let mut rep = Report { manifest: m, outcome: Outcome::Installed, program_id: 0, previous: None, copied: 0, shared: 0, hooks_failed: 0 };
    let m = &rep.manifest;
    if vdir.exists() {
        let there = read_manifest_text(&vdir.join(FILE_NAME))?;
        if there != m.to_text() {
            return Err(format!("{} {} is installed with other content; remove that version first", m.name, m.version));
        }
        rep.outcome = Outcome::AlreadyThere;
    } else if let Err(e) = build_version(st, src, m, &vdir, &mut rep.copied, &mut rep.shared) {
        // Store files this attempt added have no link left once the staging folder is gone.
        let _ = st.gc_content();
        if st.versions(&m.name).map(|v| v.is_empty()).unwrap_or(false) && st.current(&m.name).is_none() {
            let _ = fsx::remove_tree(&pkg);
        }
        return Err(e);
    }
    rep.program_id = st.ensure_id(&m.name)?;
    fsx::mkdir(&st.data, 0o755)?;
    fsx::mkdir(&st.data_dir(&m.name), 0o700)?;
    rep.previous = make_current(st, &m.name, &m.version)?;
    // The other versions' manifests before gc removes some: their keys may have to be taken back.
    let others = other_manifests(st, &m.name, &m.version);
    // Past the commit point (`current` switched): clean-up failures are events, the install stands.
    let gone = gc_versions(st, &m.name).unwrap_or_else(|e| {
        eprintln!("bitspkg: event=gc.failed reason=\"{e}\" (installed; run bitspkg gc)");
        Vec::new()
    });
    let (gone, kept): (Vec<_>, Vec<_>) = others.into_iter().partition(|(v, _)| gone.contains(v));
    st.gc_content_after_commit();
    let strip = |l: Vec<(String, Manifest)>| l.into_iter().map(|(_, m)| m).collect::<Vec<_>>();
    rep.hooks_failed = st.hooks.install(&rep.manifest, &strip(kept), &strip(gone));
    Ok(rep)
}

fn build_version(st: &Store, src: &Path, m: &Manifest, vdir: &Path, copied: &mut usize, shared: &mut usize) -> Result<(), String> {
    let staging = st.pkg(&m.name).join(format!(".new-{}", m.version));
    fsx::remove_tree(&staging)?;
    step("create staging");
    fsx::mkdir(&staging, 0o755)?;
    let mut pl = Placer { st, src, links: true, copied: 0, shared: 0, seq: 0, verified: HashSet::new() };
    let r = (|| {
        for d in &m.dirs {
            mkdirs(&staging, d)?;
        }
        for f in &m.files {
            let dst = staging.join(&f.path);
            mkdirs_parent(&staging, &f.path)?;
            pl.place(f, &dst)?;
        }
        for l in &m.symlinks {
            mkdirs_parent(&staging, &l.path)?;
            step("create symlink");
            let at = staging.join(&l.path);
            std::os::unix::fs::symlink(&l.target, &at).map_err(|e| err(&at, e))?;
        }
        let mp = staging.join(FILE_NAME);
        fsx::write_atomic(&mp, m.to_text().as_bytes(), 0o444)?;
        fsx::seal_dirs(&staging)
    })();
    if let Err(e) = r {
        let _ = fsx::remove_tree(&staging);
        return Err(e);
    }
    *copied = pl.copied;
    *shared = pl.shared;
    fsx::rename(&staging, vdir)
}

fn mkdirs(base: &Path, rel: &str) -> Result<(), String> {
    let mut p = base.to_path_buf();
    for c in rel.split('/') {
        p.push(c);
        fsx::mkdir(&p, 0o755)?;
    }
    Ok(())
}

fn mkdirs_parent(base: &Path, rel: &str) -> Result<(), String> {
    match rel.rsplit_once('/') {
        Some((dir, _)) => mkdirs(base, dir),
        None => Ok(()),
    }
}

/// `current` -> `v`, the old current becomes `previous`. Returns the old current.
fn make_current(st: &Store, name: &str, v: &str) -> Result<Option<String>, String> {
    let pkg = st.pkg(name);
    let old = st.current(name);
    if old.as_deref() == Some(v) {
        return Ok(st.previous(name));
    }
    if let Some(o) = &old
        && st.version_dir(name, o).is_dir()
    {
        fsx::set_link(&pkg.join("previous"), o)?;
    }
    fsx::set_link(&pkg.join("current"), v)?;
    Ok(old)
}

/// Removes versions that are neither current nor previous ("keep the previous version").
pub fn gc_versions(st: &Store, name: &str) -> Result<Vec<String>, String> {
    let keep = [st.current(name), st.previous(name)];
    let mut gone = Vec::new();
    for v in st.versions(name)? {
        if keep.iter().any(|k| k.as_deref() == Some(v.as_str())) {
            continue;
        }
        let t = st.trash_name("ver", name);
        fsx::move_dir(&st.version_dir(name, &v), &t)?;
        fsx::remove_tree(&t)?;
        gone.push(v);
    }
    Ok(gone)
}

/// Registers the current version's keys (and, once they exist, rules and actions) again: after a reboot
/// settingsd knows no package key until this runs (change request: init runs it once settingsd listens).
pub fn register(st: &Store, name: Option<&str>) -> Result<usize, String> {
    let _lock = st.lock()?;
    let names = match name {
        Some(n) => {
            check_name(n)?;
            vec![n.to_string()]
        }
        None => st.packages()?,
    };
    let mut failed = 0;
    for n in names {
        let Some(v) = st.current(&n) else {
            if name.is_some() {
                return Err(format!("{n} is not installed"));
            }
            continue;
        };
        failed += st.hooks.sync(&st.manifest(&n, &v)?, &others_of(st, &n, &v));
    }
    Ok(failed)
}

/// The manifests of `name`'s versions other than `v` (an unreadable one is left out: verify reports it).
fn other_manifests(st: &Store, name: &str, v: &str) -> Vec<(String, Manifest)> {
    let vs = st.versions(name).unwrap_or_default();
    vs.into_iter().filter(|o| o != v).filter_map(|o| st.manifest(name, &o).ok().map(|m| (o, m))).collect()
}

fn others_of(st: &Store, name: &str, v: &str) -> Vec<Manifest> {
    other_manifests(st, name, v).into_iter().map(|(_, m)| m).collect()
}

/// After a switch or rollback: the new current version's keys (and, once they exist, rules and actions).
fn sync(st: &Store, name: &str, v: &str) -> usize {
    match st.manifest(name, v) {
        Ok(m) => st.hooks.sync(&m, &others_of(st, name, v)),
        Err(e) => {
            eprintln!("bitspkg: event=hook.failed package={name} reason=\"{e}\" (run bitspkg register {name})");
            1
        }
    }
}

/// Makes an installed version current (the old current becomes previous). Returns the old current and the
/// number of hooks that failed (the switch stands; `bitspkg register NAME` repeats them).
pub fn switch(st: &Store, name: &str, v: &str) -> Result<(Option<String>, usize), String> {
    check_name(name)?;
    check_version(v)?;
    let lock = st.lock()?;
    switch_locked(st, &lock, name, v)
}

/// [`switch`] for a caller that already holds the store lock; name and version are checked by the caller.
pub(crate) fn switch_locked(st: &Store, _lock: &Lock, name: &str, v: &str) -> Result<(Option<String>, usize), String> {
    st.manifest(name, v).map_err(|e| format!("{name} {v} is not installed ({e})"))?;
    let old = make_current(st, name, v)?;
    Ok((old, sync(st, name, v)))
}

/// Swaps current and previous in one step (RENAME_EXCHANGE); where the file system cannot, in two steps of
/// which the first already makes the rollback take effect.
pub fn rollback(st: &Store, name: &str) -> Result<(String, String, usize), String> {
    check_name(name)?;
    let _lock = st.lock()?;
    let (Some(cur), Some(prev)) = (st.current(name), st.previous(name)) else {
        return Err(format!("{name}: no previous version to go back to"));
    };
    if !st.version_dir(name, &prev).is_dir() {
        return Err(format!("{name}: previous version {prev} is missing"));
    }
    let pkg = st.pkg(name);
    if let Err(e) = fsx::exchange(&pkg.join("current"), &pkg.join("previous")) {
        eprintln!("bitspkg: event=fallback.two_step_rollback reason=\"{e}\"");
        fsx::set_link(&pkg.join("current"), &prev)?;
        fsx::set_link(&pkg.join("previous"), &cur)?;
    }
    let failed = sync(st, name, &prev);
    Ok((cur, prev, failed))
}
