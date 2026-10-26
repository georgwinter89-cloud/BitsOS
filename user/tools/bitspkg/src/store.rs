//! The package store under a root (`/` on BitsOS, a temp folder in tests):
//!
//! ```text
//! apps/<name>/<version>/      a package version, read-only (folders 0555, files 0444/0555)
//! apps/<name>/current         symlink -> <version>   (switched by rename, so always whole)
//! apps/<name>/previous        symlink -> <version>   (the one kept for rollback)
//! apps/<name>/id              the program's ProgramId (never reused)
//! apps/.store/<sha256>[.x]    content store: version files are hard links to these (.x = executable)
//! apps/.trash/                removals in progress (finished by `recover`)
//! apps/.lock, apps/.next-id   one changing command at a time; the next ProgramId
//! data/appdata/<name>/        the program's data folder
//! ```

use crate::fsx::{self, err};
use crate::hooks::Hooks;
use crate::manifest::{FILE_NAME, Manifest};
use crate::names::{check_name, check_version};
use std::collections::HashSet;
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// ProgramIds below this are the system's (AB-2 notes: 1 = started by Claude or the kernel).
pub const FIRST_ID: u64 = 100;
/// How long a second changing command waits for the first (a CLI tool, class Normal: bounded, never forever).
const LOCK_WAIT: Duration = Duration::from_secs(60);

pub struct Store {
    pub root: PathBuf,
    pub apps: PathBuf,
    pub data: PathBuf,
    /// settingsd, permd, MC-7 (PK-2): what install registers and removal takes back.
    pub hooks: Hooks,
}

pub struct Lock(#[allow(dead_code)] File);

impl Store {
    pub fn new(root: &Path) -> Store {
        Store { root: root.to_path_buf(), apps: root.join("apps"), data: root.join("data/appdata"), hooks: Hooks::for_root(root) }
    }
    pub fn pkg(&self, name: &str) -> PathBuf {
        self.apps.join(name)
    }
    pub fn version_dir(&self, name: &str, v: &str) -> PathBuf {
        self.apps.join(name).join(v)
    }
    pub fn content(&self) -> PathBuf {
        self.apps.join(".store")
    }
    pub fn trash(&self) -> PathBuf {
        self.apps.join(".trash")
    }
    pub fn data_trash(&self) -> PathBuf {
        self.data.join(".trash")
    }
    pub fn data_dir(&self, name: &str) -> PathBuf {
        self.data.join(name)
    }
    pub fn content_file(&self, sha: &str, exec: bool) -> PathBuf {
        self.content().join(if exec { format!("{sha}.x") } else { sha.to_string() })
    }

    pub fn ensure_layout(&self) -> Result<(), String> {
        for d in [&self.apps, &self.content(), &self.trash(), &self.data] {
            if !d.is_dir() {
                fs::create_dir_all(d).map_err(|e| err(d, e))?;
                // First creation: make the new names durable up to the root (a cut must not lose `apps/`).
                for a in d.ancestors().skip(1).take_while(|a| a.starts_with(&self.root)) {
                    fsx::sync_dir(a)?;
                }
            }
        }
        Ok(())
    }

    /// Takes the store lock (flock, so a killed holder never leaves it taken) and finishes what a cut left.
    pub fn lock(&self) -> Result<Lock, String> {
        self.ensure_layout()?;
        let p = self.apps.join(".lock");
        let f = File::options().create(true).truncate(false).write(true).open(&p).map_err(|e| err(&p, e))?;
        let start = Instant::now();
        loop {
            match f.try_lock() {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) if start.elapsed() < LOCK_WAIT => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(fs::TryLockError::WouldBlock) => return Err(format!("{}: another bitspkg is still working", p.display())),
                Err(fs::TryLockError::Error(e)) => return Err(err(&p, e)),
            }
        }
        self.recover()?;
        Ok(Lock(f))
    }

    /// Package names in the store (folders with a legal package name; a stray folder is not a package), sorted.
    pub fn packages(&self) -> Result<Vec<String>, String> {
        list_names(&self.apps, |n, ft| ft.is_dir() && check_name(n).is_ok())
    }

    /// Installed versions of a package (folders with a legal version name), sorted.
    pub fn versions(&self, name: &str) -> Result<Vec<String>, String> {
        list_names(&self.pkg(name), |n, ft| ft.is_dir() && check_version(n).is_ok())
    }

    /// Where `current` points; a link that does not name a legal version (tampered) counts as none.
    pub fn current(&self, name: &str) -> Option<String> {
        fsx::read_link(&self.pkg(name).join("current")).filter(|v| check_version(v).is_ok())
    }
    pub fn previous(&self, name: &str) -> Option<String> {
        fsx::read_link(&self.pkg(name).join("previous")).filter(|v| check_version(v).is_ok())
    }

    /// A version's manifest; name and version are checked first, so command-line words cannot leave `apps/`.
    pub fn manifest(&self, name: &str, v: &str) -> Result<Manifest, String> {
        check_name(name)?;
        check_version(v)?;
        let p = self.version_dir(name, v).join(FILE_NAME);
        Manifest::from_text(&read_manifest_text(&p)?).map_err(|e| err(&p, e))
    }

    pub fn program_id(&self, name: &str) -> Option<u64> {
        fs::read_to_string(self.pkg(name).join("id")).ok().and_then(|s| s.trim().parse().ok())
    }

    /// The package's ProgramId, given once at its first install and never reused (the counter only grows).
    pub fn ensure_id(&self, name: &str) -> Result<u64, String> {
        if let Some(id) = self.program_id(name) {
            return Ok(id);
        }
        let next_p = self.apps.join(".next-id");
        let next = fs::read_to_string(&next_p).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(FIRST_ID).max(FIRST_ID);
        fsx::write_atomic(&next_p, format!("{}\n", next + 1).as_bytes(), 0o644)?;
        fsx::write_atomic(&self.pkg(name).join("id"), format!("{next}\n").as_bytes(), 0o444)?;
        Ok(next)
    }

    /// Finishes or undoes what a cut left: staging folders, half-written store files and links, removals in the
    /// trash (a whole-package removal also takes its data folder unless it was `--keep-data`).
    pub fn recover(&self) -> Result<(), String> {
        // Every command holds the lock while this runs: once one entry's hooks failed (settingsd down or not
        // answering, each request bounded by its timeout) the others wait for a later command, and an entry that
        // cannot be finished (a busy or foreign folder) is an event, not an error of every command.
        let mut hooks_down = false;
        for t in list_names(&self.trash(), |_, _| true)? {
            if let Err(e) = self.recover_entry(&t, &mut hooks_down) {
                eprintln!("bitspkg: event=recover.failed entry={t} reason=\"{e}\"");
            }
        }
        if let Err(e) = fsx::remove_tree(&self.data_trash()) {
            eprintln!("bitspkg: event=recover.failed entry=data-trash reason=\"{e}\"");
        }
        for n in list_names(&self.content(), |n, _| n.starts_with(".tmp-"))? {
            fsx::remove_tree(&self.content().join(n))?;
        }
        for name in self.packages()? {
            let pkg = self.pkg(&name);
            // Staging folders and temporaries of fsx::tmp_name (no version may end in `.new`, names::check_version).
            for n in list_names(&pkg, |n, _| n.starts_with(".new-") || n.ends_with(".new"))? {
                fsx::remove_tree(&pkg.join(n))?;
            }
        }
        for n in list_names(&self.apps, |n, ft| !ft.is_dir() && n.starts_with('.') && n.ends_with(".new"))? {
            fsx::remove_tree(&self.apps.join(n))?;
        }
        Ok(())
    }

    fn recover_entry(&self, t: &str, hooks_down: &mut bool) -> Result<(), String> {
        let p = self.trash().join(t);
        let mut parts = t.splitn(3, '-');
        let (_, kind, name) = (parts.next(), parts.next(), parts.next());
        if let (Some("drop"), Some(name)) = (kind, name)
            && !self.pkg(name).exists()
            && self.data_dir(name).exists()
        {
            self.trash_data(name)?;
        }
        if let (Some(k @ ("drop" | "keep")), Some(name)) = (kind, name) {
            let ms = manifests_in(&p);
            let r = if *hooks_down && self.hooks.list.iter().any(|h| h.needed(&ms)) {
                Err("an earlier entry's hooks failed in this command".to_string())
            } else {
                self.hooks.remove(name, &ms, k == "keep")
            };
            if let Err(e) = r {
                // The entry (with its manifests) stays until the hooks succeed; installing `name` waits for it.
                eprintln!("bitspkg: event=remove.pending package={name} reason=\"{e}\"");
                *hooks_down = true;
                return Ok(());
            }
        }
        fsx::remove_tree(&p)
    }

    /// A whole-package removal of `name` whose hooks have not finished yet (settingsd was not reachable).
    pub fn pending_removal(&self, name: &str) -> Result<bool, String> {
        let (drop, keep) = (format!("drop-{name}"), format!("keep-{name}"));
        let pending = |n: &str| n.split_once('-').is_some_and(|(_, r)| r == drop || r == keep);
        Ok(!list_names(&self.trash(), |n, _| pending(n))?.is_empty())
    }

    /// A unique trash name `<nonce>-<kind>-<name>` (kind: drop, keep, ver).
    pub fn trash_name(&self, kind: &str, name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        self.trash().join(format!("{}{nonce}-{kind}-{name}", std::process::id()))
    }

    pub fn trash_data(&self, name: &str) -> Result<(), String> {
        let d = self.data_dir(name);
        if !d.exists() {
            return Ok(());
        }
        fs::create_dir_all(self.data_trash()).map_err(|e| err(&self.data_trash(), e))?;
        fsx::rename(&d, &self.data_trash().join(name))?;
        fsx::remove_tree(&self.data_trash())
    }

    /// Content hashes every installed version refers to (from their manifests, not from link counts).
    pub fn referenced(&self) -> Result<HashSet<String>, String> {
        let mut used = HashSet::new();
        for name in self.packages()? {
            for v in self.versions(&name)? {
                for f in self.manifest(&name, &v)?.files {
                    used.insert(content_name(&f.sha256, f.exec));
                }
            }
        }
        Ok(used)
    }

    /// Removes store files no version uses: not named by any manifest, or with no hard link left (after a copy
    /// fallback, or a cut between storing and linking). When a manifest cannot be read, only files without any
    /// link go (they belong to no version whatever the manifests say).
    pub fn gc_content(&self) -> Result<u64, String> {
        let used = match self.referenced() {
            Ok(u) => Some(u),
            Err(e) => {
                eprintln!("bitspkg: event=gc.partial reason=\"{e}\" (store files still linked are kept)");
                None
            }
        };
        let mut freed = 0;
        for n in list_names(&self.content(), |_, ft| ft.is_file())? {
            let p = self.content().join(&n);
            let md = fs::symlink_metadata(&p).map_err(|e| err(&p, e))?;
            let unused = used.as_ref().is_some_and(|u| !u.contains(&n));
            if unused || md.nlink() <= 1 {
                fsx::remove_tree(&p)?;
                freed += md.len();
            }
        }
        Ok(freed)
    }

    /// gc_content after a command's commit point: the command is done and durable, so a failure here is an
    /// event, not an error (the next command or `gc` tries again). Returns the bytes freed.
    pub fn gc_content_after_commit(&self) -> u64 {
        self.gc_content().unwrap_or_else(|e| {
            eprintln!("bitspkg: event=gc.failed reason=\"{e}\" (the command itself is complete; run bitspkg gc)");
            0
        })
    }
}

/// The manifests of the versions in a package folder (installed, or in the trash), `current` first; one that
/// cannot be read is logged and left out.
pub fn manifests_in(pkg: &Path) -> Vec<Manifest> {
    let cur = fsx::read_link(&pkg.join("current"));
    let mut vs = list_names(pkg, |n, ft| ft.is_dir() && check_version(n).is_ok()).unwrap_or_default();
    vs.sort_by_key(|v| Some(v) != cur.as_ref());
    let mut out = Vec::new();
    for v in vs {
        let p = pkg.join(&v).join(FILE_NAME);
        match read_manifest_text(&p).and_then(|t| Manifest::from_text(&t).map_err(|e| err(&p, e))) {
            Ok(m) => out.push(m),
            Err(e) => eprintln!("bitspkg: event=manifest.unreadable reason=\"{e}\""),
        }
    }
    out
}

pub fn content_name(sha: &str, exec: bool) -> String {
    if exec { format!("{sha}.x") } else { sha.to_string() }
}

/// A manifest file, refused before reading when it is larger than the parser's bound.
pub fn read_manifest_text(p: &Path) -> Result<String, String> {
    let md = fs::symlink_metadata(p).map_err(|e| err(p, e))?;
    if !md.is_file() {
        return Err(err(p, "manifest is not a regular file"));
    }
    if md.len() > crate::toml::MAX_BYTES as u64 {
        return Err(err(p, format!("manifest is {} bytes, more than {}", md.len(), crate::toml::MAX_BYTES)));
    }
    fs::read_to_string(p).map_err(|e| err(p, e))
}

fn list_names(dir: &Path, keep: impl Fn(&str, &fs::FileType) -> bool) -> Result<Vec<String>, String> {
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(err(dir, e)),
    };
    let mut out = Vec::new();
    for e in rd {
        let e = e.map_err(|e| err(dir, e))?;
        let ft = e.file_type().map_err(|x| err(&e.path(), x))?;
        if let Some(n) = e.file_name().to_str()
            && keep(n, &ft)
        {
            out.push(n.to_string());
        }
    }
    out.sort();
    Ok(out)
}
