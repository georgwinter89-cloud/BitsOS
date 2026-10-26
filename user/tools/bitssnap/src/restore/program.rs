//! A program back as it was in a snapshot (VS-3, B1/B2): its package version (what `/apps/<name>/current`
//! named then) and its data folder `/data/appdata/<name>`. The version comes back the package way, through
//! bitspkg's own install with the snapshot's version folder as the source: a version still in the store
//! (current or previous) is only switched to; one bitspkg removed since is linked into the content store again,
//! every file checked against its manifest hash. `current` switches in one step and the version before stays as
//! `previous` (B2), and the settings keys follow (PK-2's hooks). The data folder is swapped in like a folder
//! restore. One "before-restore" snapshot first holds both as they were, so the same restore from it is the undo.
//!
//! Only what lies on /data is in a snapshot: packages live there once `/apps` is on the data volume (FS-8,
//! PACKAGES.md section 1); while `/apps` is the initramfs's, only the data folder comes back (the answer says so).

use super::{BEFORE_LABEL, DATA, Report, below_data, in_place, in_snapshot, with_snapshot};
use crate::dev::Dev;
use bitspkg::install::Outcome;
use bitspkg::store::Store;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProgramReport {
    pub name: String,
    pub from: u64,
    pub before: u64,
    /// The version current in the snapshot (None: none there, or the packages are not on /data).
    pub version: Option<String>,
    /// The version current before the restore.
    pub was: Option<String>,
    /// What happened to the package, in words.
    pub package: String,
    /// The data folder's restore (None: none in the snapshot, it stays).
    pub data: Option<Report>,
    pub notes: Vec<String>,
}

impl ProgramReport {
    /// The answer in words (the CLI prints it; MCP sends it as `text`).
    pub fn text(&self) -> String {
        let mut t = format!("{}: package: {}", self.name, self.package);
        if let Some(d) = &self.data {
            t.push_str(&format!("; data folder {} restored ({} files, {} folders)", d.path, d.files, d.dirs));
        }
        for n in &self.notes {
            t.push_str(&format!("; {n}"));
        }
        t.push_str(&format!(". The state before is snapshot {} (undo: restore program {} from snapshot {}).", self.before, self.name, self.before));
        t
    }
}

/// The part below /data of `p` as the kernel resolves it (symlinks of existing parents followed, the missing
/// rest appended): None when it is not on /data.
pub fn on_data(p: &Path) -> Option<String> {
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cur: &Path = p;
    let real = loop {
        match fs::canonicalize(cur) {
            Ok(r) => break r,
            Err(_) => {
                rest.push(cur.file_name()?);
                cur = cur.parent()?;
            }
        }
    };
    let mut full = real;
    for c in rest.iter().rev() {
        full.push(c);
    }
    below_data(full.to_str()?).ok()
}

/// The version folder current in the snapshot under `apps_rel/name` (checked: real folders, a valid version).
fn snapshot_version(snap: &Path, apps_rel: &str, name: &str) -> Option<(String, PathBuf)> {
    let pkg = in_snapshot(snap, &format!("{apps_rel}/{name}/current")).ok()?;
    let v = bitspkg::fsx::read_link(&pkg).filter(|v| bitspkg::names::check_version(v).is_ok())?;
    let dir = in_snapshot(snap, &format!("{apps_rel}/{name}/{v}")).ok()?;
    fs::symlink_metadata(&dir).ok().filter(|m| m.is_dir())?;
    Some((v, dir))
}

/// Restores program `name` of the bitspkg store at `root` ("/": /apps and /data/appdata) from snapshot `id`.
pub fn restore_program(dev: &mut Dev, id: u64, name: &str, root: &Path) -> Result<ProgramReport, String> {
    bitspkg::names::check_name(name)?;
    if !root.is_absolute() {
        return Err(format!("{}: the store root must be an absolute path", root.display()));
    }
    let st = Store::new(root);
    let (apps_rel, data_rel) = (on_data(&st.apps), on_data(&st.data_dir(name)));
    if apps_rel.is_none() && data_rel.is_none() {
        return Err(format!("neither {} nor {} lies on /data: nothing of {name} is in a snapshot", st.apps.display(), st.data_dir(name).display()));
    }
    with_snapshot(dev, id, |dev, snap| {
        let ver = apps_rel.as_deref().and_then(|a| snapshot_version(snap, a, name));
        let data_src = data_rel.as_deref().and_then(|r| in_snapshot(snap, r).ok());
        if ver.is_none() && data_src.is_none() {
            return Err(format!("{name}: neither a package version nor a data folder in snapshot {id}: nothing changed"));
        }
        let before = dev.take(BEFORE_LABEL, false, false).map_err(|e| format!("saving the state before: {e}"))?;
        let mut rep = ProgramReport { name: name.into(), from: id, before: before.id, was: st.current(name), ..Default::default() };
        rep.package = match &ver {
            Some((v, _)) if rep.was.as_deref() == Some(v.as_str()) => format!("{v} is current already"),
            Some((v, dir)) => {
                let r = bitspkg::install::install(&st, dir).map_err(|e| format!("{name} {v} from snapshot {id}: {e} (the data folder was not touched; the state before is snapshot {})", before.id))?;
                if r.hooks_failed > 0 {
                    rep.notes.push(format!("{} settings/permission/action registrations failed: bitspkg register {name} repeats them", r.hooks_failed));
                }
                match r.outcome {
                    Outcome::AlreadyThere => format!("switched to {v} (it was still installed)"),
                    Outcome::Installed => format!("{v} installed again from the snapshot"),
                }
            }
            None if apps_rel.is_none() => format!("{} is not on /data: the package stays as it is", st.apps.display()),
            None => format!("{name} was not installed then: the package stays as it is (bitspkg remove {name} removes it)"),
        };
        rep.version = ver.map(|(v, _)| v);
        match (data_src, data_rel) {
            (Some(src), Some(rel)) => {
                let mut r = Report { from: id, before: before.id, path: format!("{DATA}/{rel}"), ..Report::default() };
                // The package is switched already (its hooks ran): say so, and where the state before is.
                in_place(DATA, &src, &rel, snap, &mut r)
                    .map_err(|e| format!("{name}: package: {}; the data folder was not restored: {e}; the state before both is snapshot {} (undo: restore program {name} from snapshot {})", rep.package, before.id, before.id))?;
                rep.data = Some(r);
            }
            _ => rep.notes.push(format!("no data folder of {name} in snapshot {id}: the data folder stays as it is")),
        }
        rep.notes.push(format!("if {name} is running, start it again: it still has its old files open"));
        crate::log(&format!("restored program {name} from snapshot {id}: {} (before: {})", rep.package, rep.before));
        Ok(rep)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_on_data() {
        let tmp = std::env::temp_dir().join(format!("bitssnap-ondata-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        assert_eq!(on_data(&tmp), None, "a host temp folder is not on /data");
        assert_eq!(on_data(&tmp.join("missing/below")), None);
        // A path below /data that need not exist: the missing rest is appended to what exists.
        if !Path::new("/data").exists() {
            assert_eq!(on_data(Path::new("/data/appdata/brave")).as_deref(), Some("appdata/brave"));
        }
        assert_eq!(on_data(Path::new("/data")), None, "/data itself is the volume, not a path below it");
        fs::remove_dir_all(&tmp).unwrap();
    }

    /// A bad name or store root is refused before any command reaches the device (nothing mounted or taken).
    #[test]
    fn bad_names_touch_nothing() {
        let log = std::env::temp_dir().join(format!("bitssnap-noprog-{}", std::process::id()));
        fs::write(&log, b"").unwrap();
        let mut dev = Dev::open_at(log.to_str().unwrap()).unwrap();
        for bad in ["", "../x", "a/b", ".", "..", "x\0y", "-r"] {
            assert!(restore_program(&mut dev, 1, bad, Path::new("/")).is_err(), "{bad:?} was accepted");
        }
        assert!(restore_program(&mut dev, 1, "demo", Path::new("rel")).unwrap_err().contains("absolute"));
        // A store not on /data: nothing of it is in a snapshot (a host temp folder).
        assert!(restore_program(&mut dev, 1, "demo", &std::env::temp_dir()).unwrap_err().contains("lies on /data"));
        assert_eq!(fs::read(&log).unwrap(), b"", "a command reached the device");
        fs::remove_file(&log).unwrap();
    }

    #[test]
    fn report_text() {
        let r = ProgramReport { name: "brave".into(), from: 4, before: 9, package: "switched to 1.2".into(), notes: vec!["x".into()], ..Default::default() };
        assert_eq!(r.text(), "brave: package: switched to 1.2; x. The state before is snapshot 9 (undo: restore program brave from snapshot 9).");
    }
}
