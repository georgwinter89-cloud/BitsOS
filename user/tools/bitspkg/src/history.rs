//! The B1 link (PK-5, V2-PLAN section 1a B1): for a snapshot of the volume that holds the package store, which
//! version of a program was current then and where its data folder was, and what bringing that version back
//! takes now. VS-3 picks the snapshot for a time and mounts it (read-only, /run/bitsos/snap/<id>); this module
//! only reads the mounted tree, so the host tests use a plain folder.
//!
//! The store keeps no switch log: `current` and the one `previous` are symlinks (install.rs make_current), so a
//! snapshot's own `apps/<name>/current` is the record of what ran then. A snapshot is data from the past and
//! may be hostile: no symlink inside it is followed, names and versions pass names::check_* before any path is
//! built, and nothing is ever written into it.

use crate::install;
use crate::manifest::FILE_NAME;
use crate::names::{check_name, check_version};
use crate::store::{Store, read_manifest_text};
use std::fs;
use std::path::{Path, PathBuf};

/// Where the parts of a mounted snapshot are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapRoot {
    /// The package store in the snapshot (`<mnt>/apps`).
    pub apps: PathBuf,
    /// The data folders in the snapshot (`<mnt>/appdata`).
    pub data: PathBuf,
}

impl SnapRoot {
    /// A snapshot of the /data volume mounted at `mnt`: the data folders are `<mnt>/appdata/<name>` (live
    /// /data/appdata/<name>), the store `<mnt>/apps` once /apps lives on that volume (plan/PACKAGES.md section 1).
    pub fn volume(mnt: &Path) -> SnapRoot {
        SnapRoot { apps: mnt.join("apps"), data: mnt.join("appdata") }
    }
}

/// The package part of a program in the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Then {
    /// The snapshot holds no package store (apps/ was not on that volume, or it is not a real folder): which
    /// version ran is not known. Not an error and never a guess.
    Unknown(String),
    /// The store is in the snapshot; the program was not installed then.
    NotInstalled,
    Installed {
        /// The version `current` named; None when the link was missing or not a legal version name.
        current: Option<String>,
        previous: Option<String>,
        /// Version folders in the snapshot.
        versions: Vec<String>,
        program_id: Option<u64>,
        /// `<snap apps>/<name>/<current>` when that folder is in the snapshot with a readable manifest.
        current_dir: Option<PathBuf>,
        /// Why `current_dir` is None although `current` is set.
        problem: Option<String>,
    },
}

/// What bringing the snapshot's version back takes now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The live current version is that version, with the same manifest.
    Nothing,
    /// The version is still installed live with the same manifest: `bitspkg switch NAME VERSION`.
    Switch(String),
    /// The version is gone live: `bitspkg install DIR` from the snapshot's version folder (install re-hashes every
    /// file against the manifest and links it into the live store again).
    Install(PathBuf),
    /// The program was not installed then but is now: only `bitspkg remove NAME` would match (VS-3 decides).
    Remove,
    /// Nothing to do: not installed then and not now.
    Absent,
    /// Not possible from this snapshot; the reason.
    Blocked(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramAt {
    pub name: String,
    pub then: Then,
    /// The data folder in the snapshot, when it is a real folder there.
    pub data_then: Option<PathBuf>,
    /// The live data folder (where VS-3 restores `data_then` to).
    pub data_now: PathBuf,
    pub current_now: Option<String>,
    pub id_now: Option<u64>,
    pub step: Step,
}

impl ProgramAt {
    /// The ProgramId changed (removed and installed again since): restored data must be given to the live
    /// program's uid, not kept with the old owner.
    pub fn id_changed(&self) -> bool {
        match &self.then {
            Then::Installed { program_id: Some(t), .. } => self.id_now.is_some_and(|n| n != *t),
            _ => false,
        }
    }
}

fn real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

fn link(p: &Path) -> Option<String> {
    let md = fs::symlink_metadata(p).ok()?;
    if !md.file_type().is_symlink() {
        return None;
    }
    crate::fsx::read_link(p).filter(|v| check_version(v).is_ok())
}

/// A small regular file's text, never through a symlink and never a FIFO (a snapshot may hold anything).
fn small_text(p: &Path) -> Option<String> {
    let md = fs::symlink_metadata(p).ok()?;
    if !md.is_file() || md.len() > 64 {
        return None;
    }
    fs::read_to_string(p).ok()
}

/// What the snapshot at `snap` says about program `name`, compared with the live store `live`. Read-only.
pub fn program_at(snap: &SnapRoot, live: &Store, name: &str) -> Result<ProgramAt, String> {
    check_name(name)?;
    let then = then(snap, name);
    let data = snap.data.join(name);
    let current_now = live.current(name);
    let step = step(&then, live, name, current_now.as_deref());
    Ok(ProgramAt {
        name: name.to_string(),
        data_then: (real_dir(&snap.data) && real_dir(&data)).then_some(data),
        data_now: live.data_dir(name),
        current_now,
        id_now: live.program_id(name),
        then,
        step,
    })
}

fn then(snap: &SnapRoot, name: &str) -> Then {
    if !real_dir(&snap.apps) {
        return Then::Unknown(format!("{} is not in the snapshot (the package store is not on that volume)", snap.apps.display()));
    }
    let pkg = snap.apps.join(name);
    if !real_dir(&pkg) {
        return Then::NotInstalled;
    }
    let mut versions: Vec<String> = fs::read_dir(&pkg)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|v| check_version(v).is_ok())
                .collect()
        })
        .unwrap_or_default();
    versions.sort();
    let current = link(&pkg.join("current"));
    let (mut current_dir, mut problem) = (None, None);
    match &current {
        None => problem = Some(format!("{name}: no valid current link in the snapshot")),
        Some(v) => {
            let d = pkg.join(v);
            if !real_dir(&d) {
                problem = Some(format!("{name}: current {v} has no version folder in the snapshot"));
            } else if let Err(e) = read_manifest_text(&d.join(FILE_NAME)) {
                problem = Some(format!("{name} {v} in the snapshot: {e}"));
            } else {
                current_dir = Some(d);
            }
        }
    }
    Then::Installed {
        current,
        previous: link(&pkg.join("previous")),
        versions,
        program_id: small_text(&pkg.join("id")).and_then(|s| s.trim().parse().ok()),
        current_dir,
        problem,
    }
}

fn step(then: &Then, live: &Store, name: &str, now: Option<&str>) -> Step {
    let installed_now = real_dir(&live.pkg(name));
    match then {
        Then::Unknown(why) => Step::Blocked(why.clone()),
        Then::NotInstalled if installed_now => Step::Remove,
        Then::NotInstalled => Step::Absent,
        Then::Installed { current: Some(v), current_dir: Some(dir), .. } => {
            let snap_text = read_manifest_text(&dir.join(FILE_NAME)).unwrap_or_default();
            let live_dir = live.version_dir(name, v);
            if !installed_now || !real_dir(&live_dir) {
                return Step::Install(dir.clone());
            }
            match read_manifest_text(&live_dir.join(FILE_NAME)) {
                Ok(t) if t == snap_text && now == Some(v.as_str()) => Step::Nothing,
                Ok(t) if t == snap_text => Step::Switch(v.clone()),
                Ok(_) => Step::Blocked(format!("{name} {v} is installed now with other content than in the snapshot")),
                Err(e) => Step::Blocked(format!("{name} {v} is installed now but unreadable: {e}")),
            }
        }
        Then::Installed { problem, .. } => Step::Blocked(problem.clone().unwrap_or_else(|| format!("{name}: no current version in the snapshot"))),
    }
}

/// Carries out `at.step` for the package part (the data folder is VS-3's restore): Switch and Install make the
/// snapshot's version current, the version current before becomes `previous`, so `bitspkg rollback NAME` undoes
/// it. Nothing does nothing; Remove, Absent and Blocked are refused here (removing a program is VS-3's explicit
/// call to remove::remove). Returns what was done, one line.
///
/// `at` may be old (VS-3 takes its before-restore snapshot in between): under the store lock the step is worked
/// out again from the live store, and when it differs from `at.step` nothing is done and the answer is an error
/// that says so (ask program_at again).
pub fn bring_back(live: &Store, at: &ProgramAt) -> Result<String, String> {
    check_name(&at.name)?;
    let writes = matches!(at.step, Step::Switch(_) | Step::Install(_));
    let lock = if writes { Some(live.lock()?) } else { None };
    let now = step(&at.then, live, &at.name, live.current(&at.name).as_deref());
    if now != at.step {
        return Err(format!("{}: the live store changed since the snapshot was looked at (step now {now:?}, was {:?}); ask again", at.name, at.step));
    }
    match &at.step {
        Step::Nothing => Ok(format!("{}: already current", at.name)),
        Step::Switch(v) => {
            let lock = lock.as_ref().ok_or("no store lock")?;
            check_version(v)?;
            let (old, _) = install::switch_locked(live, lock, &at.name, v)?;
            Ok(format!("{}: switched to {v} (was {})", at.name, old.as_deref().unwrap_or("none")))
        }
        Step::Install(dir) => {
            // The folder is checked before anything is written: a snapshot's `brave/1.0` must hold brave 1.0.
            let m = install::read_source(dir)?;
            let want = match &at.then {
                Then::Installed { current: Some(v), .. } => v.as_str(),
                _ => "",
            };
            if m.name != at.name || m.version != want {
                return Err(format!("{}: the snapshot's folder {} holds {} {}", at.name, dir.display(), m.name, m.version));
            }
            let lock = lock.as_ref().ok_or("no store lock")?;
            let r = install::install_locked(live, lock, dir)?;
            Ok(format!("{}: installed {} from the snapshot (was {})", at.name, r.manifest.version, r.previous.as_deref().unwrap_or("none")))
        }
        Step::Remove => Err(format!("{}: was not installed then; removing it is a separate step (bitspkg remove)", at.name)),
        Step::Absent => Ok(format!("{}: not installed then or now", at.name)),
        Step::Blocked(why) => Err(why.clone()),
    }
}
